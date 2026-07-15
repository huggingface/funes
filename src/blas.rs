//! A from-scratch forward for funes' two models — a BLAS-backed alternative to the ONNX backend
//! (see `inference`). Every matmul goes through a small platform **seam** (`sgemm`/`vexp`): macOS
//! wires it to Accelerate (cblas_sgemm on AMX + vForce vvexpf), Linux to faer's pure-Rust GEMM
//! plus a runtime-dispatched polynomial exp. Everything else — the encoder, tokenization, the
//! trait impls — is shared, cross-platform source. Gated behind the `blas` feature.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use safetensors::SafeTensors;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::inference::{Embedder, Reranker};

// ---------------------------------------------------------------------------------------------
// Platform seam: the ONLY OS-specific code. A platform backend implements just these two
// functions (plus any library link in build.rs) and inherits the entire encoder below.
// ---------------------------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod seam {
    const ROW: i32 = 101;
    extern "C" {
        fn cblas_sgemm(
            order: i32,
            ta: i32,
            tb: i32,
            m: i32,
            n: i32,
            k: i32,
            alpha: f32,
            a: *const f32,
            lda: i32,
            b: *const f32,
            ldb: i32,
            beta: f32,
            c: *mut f32,
            ldc: i32,
        );
        fn vvexpf(y: *mut f32, x: *const f32, n: *const i32);
    }
    /// C[m,n] = alpha·op(A)·op(B); row-major; ta/tb are CBLAS 111 (NoTrans) / 112 (Trans).
    #[allow(clippy::too_many_arguments)]
    pub fn sgemm(
        ta: i32,
        tb: i32,
        m: usize,
        n: usize,
        k: usize,
        alpha: f32,
        a: &[f32],
        lda: usize,
        b: &[f32],
        ldb: usize,
        y: &mut [f32],
    ) {
        unsafe {
            cblas_sgemm(
                ROW,
                ta,
                tb,
                m as i32,
                n as i32,
                k as i32,
                alpha,
                a.as_ptr(),
                lda as i32,
                b.as_ptr(),
                ldb as i32,
                0.0,
                y.as_mut_ptr(),
                n as i32,
            );
        }
    }
    /// In-place vectorized exp: buf[i] = e^{buf[i]}.
    pub fn vexp(buf: &mut [f32]) {
        let n = buf.len() as i32;
        unsafe { vvexpf(buf.as_mut_ptr(), buf.as_ptr(), &n) };
    }
}

#[cfg(target_os = "linux")]
mod seam {
    use faer::linalg::matmul::matmul;
    use faer::{Accum, MatMut, MatRef, Par};

    /// Below this many flops a GEMM runs sequentially: the small per-head attention GEMMs come
    /// from worker threads that already own the parallelism; only the big batched linears
    /// (called from the main thread) benefit from faer's rayon splitting.
    const PAR_FLOPS: f64 = 1e8;

    /// C[m,n] = alpha·op(A)·op(B); row-major; ta/tb are CBLAS 111 (NoTrans) / 112 (Trans).
    #[allow(clippy::too_many_arguments)]
    pub fn sgemm(
        ta: i32,
        tb: i32,
        m: usize,
        n: usize,
        k: usize,
        alpha: f32,
        a: &[f32],
        lda: usize,
        b: &[f32],
        ldb: usize,
        y: &mut [f32],
    ) {
        // op(A) is m×k, op(B) is k×n; a transposed operand swaps its (row, col) strides.
        let (ars, acs) = if ta == super::NO_T {
            (lda as isize, 1)
        } else {
            (1, lda as isize)
        };
        let (brs, bcs) = if tb == super::NO_T {
            (ldb as isize, 1)
        } else {
            (1, ldb as isize)
        };
        let par = if 2.0 * m as f64 * n as f64 * k as f64 >= PAR_FLOPS {
            Par::rayon(0)
        } else {
            Par::Seq
        };
        unsafe {
            let a = MatRef::from_raw_parts(a.as_ptr(), m, k, ars, acs);
            let b = MatRef::from_raw_parts(b.as_ptr(), k, n, brs, bcs);
            let y = MatMut::from_raw_parts_mut(y.as_mut_ptr(), m, n, n as isize, 1);
            matmul(y, Accum::Replace, a, b, alpha, par);
        }
    }

    /// In-place vectorized exp: buf[i] = e^{buf[i]}. exp(x) = 2^f·exp(r), f = round(x/ln2), with
    /// exp(r) a degree-6 Taylor on |r| ≤ ln2/2 and 2^f built in the float's exponent bits — all
    /// branch-free so LLVM auto-vectorizes; multiversion picks the widest clone the CPU runs.
    /// Max relative error 2.5e-7.
    #[multiversion::multiversion(targets("x86_64+avx512f+avx512bw+avx512dq", "x86_64+avx2+fma"))]
    pub fn vexp(buf: &mut [f32]) {
        const LOG2E: f32 = std::f32::consts::LOG2_E;
        // The digits are load-bearing: 355/512 is exactly representable, so f·LN2_HI is exact.
        #[allow(clippy::excessive_precision)]
        const LN2_HI: f32 = 0.693359375;
        const LN2_LO: f32 = -2.121_944_4e-4;
        const MAGIC: f32 = 12582912.0; // 1.5·2^23: add then subtract rounds to nearest integer
        const C: [f32; 5] = [1.0 / 720.0, 1.0 / 120.0, 1.0 / 24.0, 1.0 / 6.0, 0.5];
        for x in buf.iter_mut() {
            let c = (*x).clamp(-87.336, 88.722);
            let f = (c * LOG2E + MAGIC) - MAGIC;
            let r = c - f * LN2_HI - f * LN2_LO;
            let p = (((((C[0] * r + C[1]) * r + C[2]) * r + C[3]) * r + C[4]) * r + 1.0) * r + 1.0;
            *x = p * f32::from_bits((((f as i32) + 127) << 23) as u32);
        }
    }
}

const NO_T: i32 = 111;
const T: i32 = 112;
const EPS: f32 = 1e-5;
const PAD: i64 = 1;
const GELU_P: f32 = 0.3275911;
const GELU_A: [f32; 5] = [0.254_829_6, -0.284_496_72, 1.421_413_8, -1.453_152_1, 1.061_405_4];
const INV_SQRT2: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// A BERT-family encoder config. Reranker is XLM-R (pad-offset positions); embedder is BERT.
#[derive(Clone, Copy)]
struct Cfg {
    prefix: &'static str,
    h: usize,
    heads: usize,
    hd: usize,
    ffn: usize,
    layers: usize,
    bert_pos: bool,
}
const RERANK: Cfg = Cfg {
    prefix: "roberta.",
    h: 768,
    heads: 12,
    hd: 64,
    ffn: 3072,
    layers: 12,
    bert_pos: false,
};
const EMBED: Cfg = Cfg {
    prefix: "",
    h: 384,
    heads: 12,
    hd: 32,
    ffn: 1536,
    layers: 12,
    bert_pos: true,
};

fn nthreads() -> usize {
    use std::sync::OnceLock;
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::env::var("NT").ok().and_then(|s| s.parse().ok()).unwrap_or(8))
}
/// Worker count for compute-bound per-sequence work (attention, embedding gather): one worker per
/// sequence up to the core count. Unlike the memory-bound `par_*` helpers (see `nthreads`), this
/// work scales with cores, so it must not be capped by the small NT default.
fn seq_workers(n: usize) -> usize {
    let cores = std::thread::available_parallelism().map(|c| c.get()).unwrap_or(8);
    n.min(cores).max(1)
}
fn ceil_div(a: usize, b: usize) -> usize {
    a.div_ceil(b)
}

/// Below this many elements the par_* helpers run inline: spawning nthreads() scoped threads
/// costs more than the memory pass it would split.
const PAR_MIN: usize = 1 << 16;

fn par_chunks<F: Fn(&mut [f32]) + Sync>(x: &mut [f32], f: F) {
    if x.len() <= PAR_MIN {
        return f(x);
    }
    let cs = ceil_div(x.len(), nthreads()).max(1);
    std::thread::scope(|s| {
        for chunk in x.chunks_mut(cs) {
            let f = &f;
            s.spawn(move || f(chunk));
        }
    });
}
fn par_rows<F: Fn(&mut [f32]) + Sync>(x: &mut [f32], row_len: usize, f: F) {
    if x.len() <= PAR_MIN {
        for row in x.chunks_mut(row_len) {
            f(row);
        }
        return;
    }
    let cr = ceil_div(x.len() / row_len, nthreads()).max(1);
    std::thread::scope(|s| {
        for chunk in x.chunks_mut(cr * row_len) {
            let f = &f;
            s.spawn(move || {
                for row in chunk.chunks_mut(row_len) {
                    f(row);
                }
            });
        }
    });
}
fn par_add(dst: &mut [f32], src: &[f32]) {
    if dst.len() <= PAR_MIN {
        for (d, sc) in dst.iter_mut().zip(src) {
            *d += sc;
        }
        return;
    }
    let cs = ceil_div(dst.len(), nthreads()).max(1);
    std::thread::scope(|s| {
        for (d, sc) in dst.chunks_mut(cs).zip(src.chunks(cs)) {
            s.spawn(move || {
                for i in 0..d.len() {
                    d[i] += sc[i];
                }
            });
        }
    });
}

/// y[m,out] = x[m,in] · W[out,in]ᵀ + b[out]. The caller owns y (m·out): buffers are reused
/// across layers — a fresh allocation per call makes the allocator release and re-fault
/// hundreds of MB per layer on platforms that return large frees to the OS.
fn linear(x: &[f32], m: usize, in_: usize, w: &[f32], b: &[f32], out: usize, y: &mut [f32]) {
    seam::sgemm(NO_T, T, m, out, in_, 1.0, x, in_, w, in_, y);
    par_rows(y, out, |row| {
        for (c, bc) in row.iter_mut().zip(b) {
            *c += bc;
        }
    });
}

fn layernorm(x: &mut [f32], h: usize, g: &[f32], b: &[f32]) {
    par_rows(x, h, |row| {
        let hf = row.len() as f32;
        let mean: f32 = row.iter().sum::<f32>() / hf;
        let var: f32 = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / hf;
        let inv = 1.0 / (var + EPS).sqrt();
        for c in 0..row.len() {
            row[c] = (row[c] - mean) * inv * g[c] + b[c];
        }
    });
}

/// Exact GELU: 0.5·x·(1+erf(x/√2)); erf via Abramowitz-Stegun 7.1.26, exp via the seam.
fn gelu(v: &mut [f32]) {
    par_chunks(v, |chunk| {
        let n = chunk.len();
        let mut e: Vec<f32> = chunk
            .iter()
            .map(|&x| {
                let u = x * INV_SQRT2;
                -u * u
            })
            .collect();
        seam::vexp(&mut e);
        for i in 0..n {
            let x = chunk[i];
            let u = x * INV_SQRT2;
            let s = if u < 0.0 { -1.0 } else { 1.0 };
            let t = 1.0 / (1.0 + GELU_P * u.abs());
            let poly = ((((GELU_A[4] * t + GELU_A[3]) * t + GELU_A[2]) * t + GELU_A[1]) * t + GELU_A[0]) * t;
            let erf = s * (1.0 - poly * e[i]);
            chunk[i] = 0.5 * x * (1.0 + erf);
        }
    });
}

fn softmax_rows(s: &mut [f32], rows: usize, cols: usize) {
    for r in 0..rows {
        let row = &mut s[r * cols..r * cols + cols];
        let mx = row.iter().copied().fold(f32::MIN, f32::max);
        for v in row.iter_mut() {
            *v -= mx;
        }
    }
    seam::vexp(s);
    for r in 0..rows {
        let row = &mut s[r * cols..r * cols + cols];
        let sum: f32 = row.iter().sum();
        let inv = 1.0 / sum;
        for v in row.iter_mut() {
            *v *= inv;
        }
    }
}

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in v.iter_mut() {
        *x /= norm;
    }
}

/// Forward-pass buffers, owned by the model and reused across calls, growing to the high-water
/// mark. Freeing them between forwards hands the pages back to the OS, and the next call pays a
/// page fault per 4 KiB rewritten — hundreds of MB per forward (see linear()).
#[derive(Default)]
struct Scratch {
    hid: Vec<f32>,
    q: Vec<f32>,
    k: Vec<f32>,
    v: Vec<f32>,
    ctx: Vec<f32>,
    attn: Vec<f32>,
    inter: Vec<f32>,
    ffn_out: Vec<f32>,
}

/// Shared BERT-family encoder → last_hidden_state in `s.hid` [n*l*h]. `mask[s*l+j]` 1=real, 0=pad.
fn encode(w: &HashMap<String, Vec<f32>>, c: Cfg, ids: &[i64], mask: &[i64], n: usize, l: usize, s: &mut Scratch) {
    let g = |k: &str| w.get(k).unwrap_or_else(|| panic!("missing weight {k}")).as_slice();
    let (h, ffn, hd, heads) = (c.h, c.ffn, c.hd, c.heads);
    let m = n * l;
    let word = g(&format!("{}embeddings.word_embeddings.weight", c.prefix));
    let pos = g(&format!("{}embeddings.position_embeddings.weight", c.prefix));
    let typ = g(&format!("{}embeddings.token_type_embeddings.weight", c.prefix));
    for (buf, len) in [
        (&mut s.hid, m * h),
        (&mut s.q, m * h),
        (&mut s.k, m * h),
        (&mut s.v, m * h),
        (&mut s.ctx, m * h),
        (&mut s.attn, m * h),
        (&mut s.inter, m * ffn),
        (&mut s.ffn_out, m * h),
    ] {
        buf.resize(len, 0.0);
    }
    let seqs_per = ceil_div(n, seq_workers(n));
    std::thread::scope(|scope| {
        for (gi, grp) in s.hid.chunks_mut(seqs_per * l * h).enumerate() {
            scope.spawn(move || {
                for (si, seq) in grp.chunks_mut(l * h).enumerate() {
                    let sq = gi * seqs_per + si;
                    let mut run = 0i64;
                    for i in 0..l {
                        let id = ids[sq * l + i];
                        let posid = if c.bert_pos {
                            i
                        } else if id != PAD {
                            run += 1;
                            (PAD + run) as usize
                        } else {
                            PAD as usize
                        };
                        let dst = &mut seq[i * h..i * h + h];
                        let we = &word[id as usize * h..id as usize * h + h];
                        let pe = &pos[posid * h..posid * h + h];
                        for cc in 0..h {
                            dst[cc] = we[cc] + pe[cc] + typ[cc];
                        }
                    }
                }
            });
        }
    });
    layernorm(
        &mut s.hid,
        h,
        g(&format!("{}embeddings.LayerNorm.weight", c.prefix)),
        g(&format!("{}embeddings.LayerNorm.bias", c.prefix)),
    );

    let scale = 1.0 / (hd as f32).sqrt();
    for ly in 0..c.layers {
        let p = format!("{}encoder.layer.{ly}", c.prefix);
        linear(
            &s.hid,
            m,
            h,
            g(&format!("{p}.attention.self.query.weight")),
            g(&format!("{p}.attention.self.query.bias")),
            h,
            &mut s.q,
        );
        linear(
            &s.hid,
            m,
            h,
            g(&format!("{p}.attention.self.key.weight")),
            g(&format!("{p}.attention.self.key.bias")),
            h,
            &mut s.k,
        );
        linear(
            &s.hid,
            m,
            h,
            g(&format!("{p}.attention.self.value.weight")),
            g(&format!("{p}.attention.self.value.bias")),
            h,
            &mut s.v,
        );

        let seqs_per = ceil_div(n, seq_workers(n));
        let (qr, kr, vr, maskr) = (&s.q, &s.k, &s.v, &mask);
        std::thread::scope(|scope| {
            for (gi, grp) in s.ctx.chunks_mut(seqs_per * l * h).enumerate() {
                scope.spawn(move || {
                    let mut qh = vec![0f32; l * hd];
                    let mut kh = vec![0f32; l * hd];
                    let mut vh = vec![0f32; l * hd];
                    let mut sc = vec![0f32; l * l];
                    let mut ch = vec![0f32; l * hd];
                    let s0 = gi * seqs_per;
                    let nseq = grp.len() / (l * h);
                    for si in 0..nseq {
                        let s = s0 + si;
                        for head in 0..heads {
                            for i in 0..l {
                                let src = (s * l + i) * h + head * hd;
                                qh[i * hd..i * hd + hd].copy_from_slice(&qr[src..src + hd]);
                                kh[i * hd..i * hd + hd].copy_from_slice(&kr[src..src + hd]);
                                vh[i * hd..i * hd + hd].copy_from_slice(&vr[src..src + hd]);
                            }
                            seam::sgemm(NO_T, T, l, l, hd, scale, &qh, hd, &kh, hd, &mut sc);
                            let mrow = &maskr[s * l..s * l + l];
                            for j in 0..l {
                                if mrow[j] == 0 {
                                    for i in 0..l {
                                        sc[i * l + j] += -1e30f32;
                                    }
                                }
                            }
                            softmax_rows(&mut sc, l, l);
                            seam::sgemm(NO_T, NO_T, l, hd, l, 1.0, &sc, l, &vh, hd, &mut ch);
                            for i in 0..l {
                                let dst = (si * l + i) * h + head * hd;
                                grp[dst..dst + hd].copy_from_slice(&ch[i * hd..i * hd + hd]);
                            }
                        }
                    }
                });
            }
        });
        linear(
            &s.ctx,
            m,
            h,
            g(&format!("{p}.attention.output.dense.weight")),
            g(&format!("{p}.attention.output.dense.bias")),
            h,
            &mut s.attn,
        );
        par_add(&mut s.hid, &s.attn);
        layernorm(
            &mut s.hid,
            h,
            g(&format!("{p}.attention.output.LayerNorm.weight")),
            g(&format!("{p}.attention.output.LayerNorm.bias")),
        );

        linear(
            &s.hid,
            m,
            h,
            g(&format!("{p}.intermediate.dense.weight")),
            g(&format!("{p}.intermediate.dense.bias")),
            ffn,
            &mut s.inter,
        );
        gelu(&mut s.inter);
        linear(
            &s.inter,
            m,
            ffn,
            g(&format!("{p}.output.dense.weight")),
            g(&format!("{p}.output.dense.bias")),
            h,
            &mut s.ffn_out,
        );
        par_add(&mut s.hid, &s.ffn_out);
        layernorm(
            &mut s.hid,
            h,
            g(&format!("{p}.output.LayerNorm.weight")),
            g(&format!("{p}.output.LayerNorm.bias")),
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Model loading + tokenization (cross-platform)
// ---------------------------------------------------------------------------------------------

/// Resolve a local HF hub snapshot dir for `repo` (e.g. "BAAI/bge-small-en-v1.5"). Populated by any
/// standard HF download; errors with the command to fetch it if absent.
fn hf_snapshot(repo: &str) -> Result<PathBuf> {
    let base = std::env::var("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/huggingface"));
    let snaps = base
        .join("hub")
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("snapshots");
    let entries = std::fs::read_dir(&snaps)
        .with_context(|| format!("no local snapshot for {repo}; run: huggingface-cli download {repo}"))?;
    for e in entries {
        let p = e?.path();
        if p.join("model.safetensors").exists() {
            return Ok(p);
        }
    }
    bail!(
        "no model.safetensors under {}; run: huggingface-cli download {repo}",
        snaps.display()
    )
}

fn load_weights(dir: &Path) -> Result<HashMap<String, Vec<f32>>> {
    let bytes = std::fs::read(dir.join("model.safetensors"))?;
    let st = SafeTensors::deserialize(&bytes)?;
    let mut w = HashMap::new();
    for (name, view) in st.tensors() {
        if view.dtype() == safetensors::Dtype::F32 {
            let f: Vec<f32> = view
                .data()
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            w.insert(name, f);
        }
    }
    Ok(w)
}

fn load_tokenizer(dir: &Path) -> Result<Tokenizer> {
    let mut tok = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| anyhow!("load tokenizer: {e}"))?;
    tok.with_truncation(Some(TruncationParams {
        max_length: 512,
        ..Default::default()
    }))
    .map_err(|e| anyhow!("set truncation: {e}"))?;
    tok.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        ..Default::default()
    }));
    Ok(tok)
}

/// Tokenized batch → (ids, mask, n, l), both flattened row-major, padded to the batch's longest.
fn to_batch(encs: &[tokenizers::Encoding]) -> (Vec<i64>, Vec<i64>, usize, usize) {
    let n = encs.len();
    let l = encs[0].get_ids().len();
    let mut ids = Vec::with_capacity(n * l);
    let mut mask = Vec::with_capacity(n * l);
    for e in encs {
        ids.extend(e.get_ids().iter().map(|&x| x as i64));
        mask.extend(e.get_attention_mask().iter().map(|&x| x as i64));
    }
    (ids, mask, n, l)
}

/// bge-small-en-v1.5 embedder: BERT encoder → CLS → L2-normalize.
pub struct BlasEmbedder {
    w: HashMap<String, Vec<f32>>,
    tok: Tokenizer,
    scratch: Scratch,
}

impl BlasEmbedder {
    pub fn new() -> Result<Self> {
        let dir = hf_snapshot("BAAI/bge-small-en-v1.5")?;
        Ok(Self {
            w: load_weights(&dir)?,
            tok: load_tokenizer(&dir)?,
            scratch: Scratch::default(),
        })
    }
}

impl Embedder for BlasEmbedder {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let encs = self
            .tok
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow!("tokenize: {e}"))?;
        let (ids, mask, n, l) = to_batch(&encs);
        encode(&self.w, EMBED, &ids, &mask, n, l, &mut self.scratch);
        let hid = &self.scratch.hid;
        let h = EMBED.h;
        Ok((0..n)
            .map(|s| {
                let mut e = hid[(s * l) * h..(s * l) * h + h].to_vec();
                l2_normalize(&mut e);
                e
            })
            .collect())
    }
}

/// bge-reranker-base cross-encoder: XLM-R encoder → CLS → out_proj(tanh(dense(cls))).
pub struct BlasReranker {
    w: HashMap<String, Vec<f32>>,
    tok: Tokenizer,
    scratch: Scratch,
}

impl BlasReranker {
    pub fn new() -> Result<Self> {
        let dir = hf_snapshot("BAAI/bge-reranker-base")?;
        Ok(Self {
            w: load_weights(&dir)?,
            tok: load_tokenizer(&dir)?,
            scratch: Scratch::default(),
        })
    }
}

impl Reranker for BlasReranker {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        let pairs: Vec<(&str, &str)> = docs.iter().map(|d| (query, *d)).collect();
        let encs = self
            .tok
            .encode_batch(pairs, true)
            .map_err(|e| anyhow!("tokenize: {e}"))?;
        let (ids, mask, n, l) = to_batch(&encs);
        encode(&self.w, RERANK, &ids, &mask, n, l, &mut self.scratch);
        let hid = &self.scratch.hid;
        let h = RERANK.h;
        let mut cls = vec![0f32; n * h];
        for s in 0..n {
            cls[s * h..s * h + h].copy_from_slice(&hid[(s * l) * h..(s * l) * h + h]);
        }
        let mut d = vec![0f32; n * h];
        linear(
            &cls,
            n,
            h,
            self.w["classifier.dense.weight"].as_slice(),
            self.w["classifier.dense.bias"].as_slice(),
            h,
            &mut d,
        );
        for v in d.iter_mut() {
            *v = v.tanh();
        }
        let mut scores = vec![0f32; n];
        linear(
            &d,
            n,
            h,
            self.w["classifier.out_proj.weight"].as_slice(),
            self.w["classifier.out_proj.bias"].as_slice(),
            1,
            &mut scores,
        );
        Ok(scores)
    }
}
