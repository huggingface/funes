//! A from-scratch forward for funes' models — a BLAS-backed alternative to the ONNX backend
//! (see `inference`). Every matmul goes through a small platform **seam** (`sgemm`/`vexp`): macOS
//! wires it to Accelerate (cblas_sgemm on AMX + vForce vvexpf), Linux to faer's pure-Rust GEMM
//! plus a runtime-dispatched polynomial exp. This module holds the seam, the kernels, the model
//! loaders and the length-grouped batching. The encoders built on them are cross-platform source:
//! BERT (the embedders and bge-reranker-base) in [`bert`], ModernBERT
//! (gte-reranker-modernbert-base) in [`modernbert`]. Gated behind the `blas` feature.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use safetensors::tensor::Metadata;
use tokenizers::utils::padding::pad_encodings;
use tokenizers::{Encoding, PaddingParams, Tokenizer, TruncationParams};

pub(super) mod bert;
pub(super) mod modernbert;
pub use bert::{BlasEmbedder, BlasReranker};
pub use modernbert::GteReranker;

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
        fn vDSP_maxv(a: *const f32, stride: isize, out: *mut f32, n: usize);
        fn vDSP_sve(a: *const f32, stride: isize, out: *mut f32, n: usize);
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
    /// Vectorized max reduction (empty slice → -inf, like the fold it replaces).
    pub fn vmax(x: &[f32]) -> f32 {
        let mut out = f32::MIN;
        unsafe { vDSP_maxv(x.as_ptr(), 1, &mut out, x.len()) };
        out
    }
    /// Vectorized sum reduction.
    pub fn vsum(x: &[f32]) -> f32 {
        let mut out = 0f32;
        unsafe { vDSP_sve(x.as_ptr(), 1, &mut out, x.len()) };
        out
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
    /// Max relative error 2.5e-7. Returns 0, not a subnormal, wherever e^x falls below FLT_MIN.
    #[multiversion::multiversion(targets("x86_64+avx512f+avx512bw+avx512dq", "x86_64+avx2+fma"))]
    pub fn vexp(buf: &mut [f32]) {
        const LOG2E: f32 = std::f32::consts::LOG2_E;
        // The digits are load-bearing: 355/512 is exactly representable, so f·LN2_HI is exact.
        #[allow(clippy::excessive_precision)]
        const LN2_HI: f32 = 0.693359375;
        const LN2_LO: f32 = -2.121_944_4e-4;
        const MAGIC: f32 = 12582912.0; // 1.5·2^23: add then subtract rounds to nearest integer
        const C: [f32; 5] = [1.0 / 720.0, 1.0 / 120.0, 1.0 / 24.0, 1.0 / 6.0, 0.5];
        // e^x is a normal f32 only down to ln(FLT_MIN) = -126·ln2 = -87.3365448, and a subnormal
        // result costs a microcode assist per operation on x86. This is the smallest f32 above
        // that boundary: e^-87.3365402 = 1.1754997e-38, against FLT_MIN = 1.1754944e-38. The next
        // f32 down, -87.3365479, gives 1.1754907e-38, which is subnormal.
        const UNDERFLOW: f32 = -87.33654;
        for x in buf.iter_mut() {
            let v = *x;
            let c = v.clamp(UNDERFLOW, 88.722);
            let f = (c * LOG2E + MAGIC) - MAGIC;
            let r = c - f * LN2_HI - f * LN2_LO;
            let p = (((((C[0] * r + C[1]) * r + C[2]) * r + C[3]) * r + C[4]) * r + 1.0) * r + 1.0;
            // A select rather than a branch: a branch here would stop LLVM vectorizing the loop.
            *x = if v < UNDERFLOW {
                0.0
            } else {
                p * f32::from_bits((((f as i32) + 127) << 23) as u32)
            };
        }
    }

    /// A strict-order fold over floats can't be vectorized (reassociation changes the result);
    /// independent lane accumulators make the reduction order fixed and the loop vectorizable.
    /// LANES spans two AVX2 registers / four NEON ones.
    const LANES: usize = 16;

    /// Vectorized max reduction (empty slice → -inf, like the fold it replaces).
    #[multiversion::multiversion(targets("x86_64+avx512f+avx512bw+avx512dq", "x86_64+avx2+fma"))]
    pub fn vmax(x: &[f32]) -> f32 {
        let mut acc = [f32::MIN; LANES];
        let chunks = x.chunks_exact(LANES);
        let rem = chunks.remainder();
        for c in chunks {
            for (a, v) in acc.iter_mut().zip(c) {
                *a = a.max(*v);
            }
        }
        let m = rem.iter().copied().fold(f32::MIN, f32::max);
        acc.into_iter().fold(m, f32::max)
    }

    /// Vectorized sum reduction.
    #[multiversion::multiversion(targets("x86_64+avx512f+avx512bw+avx512dq", "x86_64+avx2+fma"))]
    pub fn vsum(x: &[f32]) -> f32 {
        let mut acc = [0f32; LANES];
        let chunks = x.chunks_exact(LANES);
        let rem = chunks.remainder();
        for c in chunks {
            for (a, v) in acc.iter_mut().zip(c) {
                *a += *v;
            }
        }
        acc.into_iter().sum::<f32>() + rem.iter().sum::<f32>()
    }
}

const NO_T: i32 = 111;
const T: i32 = 112;
const EPS: f32 = 1e-5;
const GELU_P: f32 = 0.3275911;
const GELU_A: [f32; 5] = [0.254_829_6, -0.284_496_72, 1.421_413_8, -1.453_152_1, 1.061_405_4];
const INV_SQRT2: f32 = std::f32::consts::FRAC_1_SQRT_2;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const RERANK_GROUP: usize = 16; // Apple Silicon AMX saturates at larger groups
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const RERANK_GROUP: usize = 8;

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

/// y[m,out] = x[m,in] · W[out,in]ᵀ. The caller owns y (m·out): buffers are reused across layers —
/// a fresh allocation per call makes the allocator release and re-fault hundreds of MB per layer on
/// platforms that return large frees to the OS.
fn matmul(x: &[f32], m: usize, in_: usize, w: &[f32], out: usize, y: &mut [f32]) {
    seam::sgemm(NO_T, T, m, out, in_, 1.0, x, in_, w, in_, y);
}

/// y[m,out] = x[m,in] · W[out,in]ᵀ + b[out], into a caller-owned y like [`matmul`].
fn linear(x: &[f32], m: usize, in_: usize, w: &[f32], b: &[f32], out: usize, y: &mut [f32]) {
    matmul(x, m, in_, w, out, y);
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
        let mx = seam::vmax(row);
        for v in row.iter_mut() {
            *v -= mx;
        }
    }
    seam::vexp(s);
    for r in 0..rows {
        let row = &mut s[r * cols..r * cols + cols];
        let inv = 1.0 / seam::vsum(row);
        for v in row.iter_mut() {
            *v *= inv;
        }
    }
}

pub(super) fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in v.iter_mut() {
        *x /= norm;
    }
}

// ---------------------------------------------------------------------------------------------
// Model loading + tokenization (cross-platform)
// ---------------------------------------------------------------------------------------------

/// Resolve a snapshot dir for `repo` (e.g. "BAAI/bge-small-en-v1.5") in the standard HF cache,
/// downloading the two files the forward needs on first use (like the ONNX backend does).
pub(super) fn hf_snapshot(repo: &str) -> Result<PathBuf> {
    if let Some(dir) = local_snapshot(repo) {
        return Ok(dir);
    }
    eprintln!("downloading {repo} (first use)…");
    // A dedicated thread: the blocking hf-hub client drives its own runtime with block_on,
    // which would panic if called from a thread already inside a tokio runtime (the MCP server).
    let owned = repo.to_string();
    let dir = std::thread::spawn(move || -> Result<PathBuf> {
        let (owner, name) = owned
            .split_once('/')
            .ok_or_else(|| anyhow!("model repo {owned} is not owner/name"))?;
        let client = hf_hub::HFClientSync::new().map_err(|e| anyhow!("hf-hub client: {e}"))?;
        let r = client.model(owner, name);
        r.download_file()
            .filename("tokenizer.json")
            .send()
            .with_context(|| format!("downloading {owned}/tokenizer.json"))?;
        let weights = r
            .download_file()
            .filename("model.safetensors")
            .send()
            .with_context(|| format!("downloading {owned}/model.safetensors"))?;
        Ok(weights
            .parent()
            .ok_or_else(|| anyhow!("downloaded file has no parent dir"))?
            .to_path_buf())
    })
    .join()
    .map_err(|_| anyhow!("model download thread panicked"))??;
    Ok(dir)
}

/// A cached snapshot of `repo` holding both files the forward needs, if any. With several
/// snapshots cached the pick is arbitrary — fine here: any complete copy of these frozen model
/// repos is interchangeable.
fn local_snapshot(repo: &str) -> Option<PathBuf> {
    let base = std::env::var("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/huggingface"));
    let snaps = base
        .join("hub")
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("snapshots");
    for e in std::fs::read_dir(&snaps).ok()?.flatten() {
        let p = e.path();
        if p.join("model.safetensors").exists() && p.join("tokenizer.json").exists() {
            return Some(p);
        }
    }
    None
}

/// The f32 tensors of `dir/model.safetensors`, one tensor at a time. Reading the file whole would
/// hold its bytes and the f32 copies of them side by side — 2.2 GB for the reranker's 1.1 GB of
/// weights, on a run that keeps 1.2 GB — so only the header is buffered and each tensor is
/// converted through a small window.
pub(super) fn load_weights(dir: &Path) -> Result<HashMap<String, Vec<f32>>> {
    /// Bytes read per conversion. A multiple of 4, so no f32 straddles two reads.
    const WINDOW: usize = 1 << 20;
    /// What safetensors itself accepts, so a corrupt length cannot ask for an unbounded allocation.
    const MAX_HEADER: u64 = 100_000_000;

    let path = dir.join("model.safetensors");
    let mut file = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata()?.len();

    // A safetensors file is an 8-byte little-endian header length, that many bytes of JSON naming
    // each tensor, then the tensor data the header's offsets index into.
    let mut len = [0u8; 8];
    file.read_exact(&mut len)
        .with_context(|| format!("reading the header length of {}", path.display()))?;
    let head_len = u64::from_le_bytes(len);
    if head_len > MAX_HEADER || head_len + 8 > size {
        bail!(
            "{} declares a {head_len}-byte header, which its {size} bytes cannot hold",
            path.display()
        );
    }
    let mut head = vec![0u8; head_len as usize];
    file.read_exact(&mut head)
        .with_context(|| format!("reading the header of {}", path.display()))?;
    // Deserializing `Metadata` runs safetensors' own validation: the offsets tile the data from zero
    // and each tensor's shape and dtype account for exactly its byte range.
    let meta: Metadata =
        serde_json::from_slice(&head).with_context(|| format!("parsing the header of {}", path.display()))?;

    // The one thing the header cannot answer on its own: the data has to be as long as it accounts for.
    let data_start = 8 + head_len;
    let accounted = meta.tensors().values().map(|i| i.data_offsets.1).max().unwrap_or(0) as u64;
    let data_len = size - data_start;
    if accounted != data_len {
        bail!(
            "{} carries {data_len} bytes of tensor data, its header accounts for {accounted}",
            path.display()
        );
    }

    let mut tensors: Vec<(String, usize, usize)> = meta
        .tensors()
        .into_iter()
        .filter(|(_, i)| i.dtype == safetensors::Dtype::F32)
        .map(|(name, i)| (name, i.data_offsets.0, i.data_offsets.1))
        .collect();
    // Front to back, so the reads below never seek backwards over the file.
    tensors.sort_by_key(|(_, start, _)| *start);

    let mut weights = HashMap::with_capacity(tensors.len());
    let mut window = vec![0u8; WINDOW];
    let mut at = 0usize;
    for (name, start, end) in tensors {
        if at != start {
            file.seek(SeekFrom::Start(data_start + start as u64))
                .with_context(|| format!("seeking to {name} in {}", path.display()))?;
        }
        let mut vals: Vec<f32> = Vec::with_capacity((end - start) / 4);
        let mut left = end - start;
        while left > 0 {
            let take = left.min(WINDOW);
            file.read_exact(&mut window[..take])
                .with_context(|| format!("reading {name} from {}", path.display()))?;
            vals.extend(
                window[..take]
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            );
            left -= take;
        }
        at = end;
        weights.insert(name, vals);
    }
    Ok(weights)
}

pub(super) fn load_tokenizer(dir: &Path) -> Result<Tokenizer> {
    let mut tok = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| anyhow!("load tokenizer: {e}"))?;
    tok.with_truncation(Some(TruncationParams {
        max_length: 512,
        ..Default::default()
    }))
    .map_err(|e| anyhow!("set truncation: {e}"))?;
    // Unpadded: `by_length` pads each forward's group to its own longest.
    tok.with_padding(None);
    Ok(tok)
}

/// Tokenized batch → (ids, mask, n, l), both flattened row-major, padded to the batch's longest.
fn to_batch(encs: &[Encoding]) -> (Vec<i64>, Vec<i64>, usize, usize) {
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

/// Run `forward` over `encs` in length-sorted groups of at most `group`, each padded to its own
/// longest, and return the per-sequence outputs in input order. Chunks run from a few tokens to the
/// 512 cap, so one forward over a whole call — indexing hands `embed` 256 texts at a time — would
/// spend most of its work on padding. `forward` gets one group laid out as [`to_batch`] does and
/// returns one output per sequence.
fn by_length<T: Default + Clone>(
    mut encs: Vec<Encoding>,
    group: usize,
    mut forward: impl FnMut(&[i64], &[i64], usize, usize) -> Vec<T>,
) -> Result<Vec<T>> {
    let mut order: Vec<usize> = (0..encs.len()).collect();
    order.sort_by_key(|&i| encs[i].get_ids().len());
    let mut out = vec![T::default(); encs.len()];
    for group in order.chunks(group) {
        let mut batch: Vec<Encoding> = group.iter().map(|&i| std::mem::take(&mut encs[i])).collect();
        pad_encodings(&mut batch, &PaddingParams::default()).map_err(|e| anyhow!("pad: {e}"))?;
        let (ids, mask, n, l) = to_batch(&batch);
        for (&i, y) in group.iter().zip(forward(&ids, &mask, n, l)) {
            out[i] = y;
        }
    }
    Ok(out)
}

fn mean_pool(rows: &[f32], mask: &[i64], h: usize) -> Vec<f32> {
    let mut sum = vec![0f32; h];
    let mut kept = 0f32;
    for (row, &m) in rows.chunks_exact(h).zip(mask) {
        if m != 0 {
            sum.iter_mut().zip(row).for_each(|(s, x)| *s += x);
            kept += 1.0;
        }
    }
    sum.iter_mut().for_each(|s| *s /= kept);
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built safetensors file: every f32 tensor comes back whole whatever order the header
    /// lists it in, a tensor of another dtype is skipped, and a file whose header and data disagree
    /// is refused instead of read.
    #[test]
    fn load_weights_streams_every_f32_tensor_and_refuses_a_bad_file() {
        fn file(dir: &Path, header: &str, data: &[u8]) {
            let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
            bytes.extend_from_slice(header.as_bytes());
            bytes.extend_from_slice(data);
            std::fs::write(dir.join("model.safetensors"), bytes).unwrap();
        }
        let mut data = Vec::new();
        data.extend(1.0f32.to_le_bytes()); // "second", listed first in the header
        data.extend((-2.5f32).to_le_bytes());
        data.extend(7i64.to_le_bytes()); // an i64 tensor, skipped
        data.extend(0.5f32.to_le_bytes()); // "first", listed last
        let header = r#"{"__metadata__":{"format":"pt"},"second":{"dtype":"F32","shape":[2],"data_offsets":[0,8]},"skipped":{"dtype":"I64","shape":[1],"data_offsets":[8,16]},"first":{"dtype":"F32","shape":[1],"data_offsets":[16,20]}}"#;

        let dir = tempfile::tempdir().unwrap();
        file(dir.path(), header, &data);
        let w = load_weights(dir.path()).unwrap();
        assert_eq!(w.len(), 2, "only the f32 tensors are loaded: {:?}", w.keys());
        assert_eq!(w["second"], vec![1.0, -2.5]);
        assert_eq!(w["first"], vec![0.5]);

        // A shape its byte range cannot hold, and a gap between two tensors: safetensors' own
        // validation catches both while the header is deserialized. `{err:#}` to see its message
        // under the context this adds.
        let bad = tempfile::tempdir().unwrap();
        file(
            bad.path(),
            r#"{"t":{"dtype":"F32","shape":[64],"data_offsets":[0,4]}}"#,
            &data[..4],
        );
        let err = format!("{:#}", load_weights(bad.path()).unwrap_err());
        assert!(
            err.contains("invalid shape, data type, or offset"),
            "a shape its bytes cannot hold must be refused, got: {err}"
        );

        let gap = tempfile::tempdir().unwrap();
        let two = r#"{"a":{"dtype":"F32","shape":[1],"data_offsets":[0,4]},"b":{"dtype":"F32","shape":[1],"data_offsets":[8,12]}}"#;
        file(gap.path(), two, &data[..12]);
        let err = format!("{:#}", load_weights(gap.path()).unwrap_err());
        assert!(
            err.contains("invalid offset for tensor"),
            "a gap in the data must be refused, got: {err}"
        );

        // Data the header does not account for. The header alone cannot catch this, so the loader
        // checks it against the file's length.
        let extra = tempfile::tempdir().unwrap();
        file(
            extra.path(),
            r#"{"t":{"dtype":"F32","shape":[1],"data_offsets":[0,4]}}"#,
            &data[..8],
        );
        let err = format!("{:#}", load_weights(extra.path()).unwrap_err());
        assert!(
            err.contains("8 bytes of tensor data") && err.contains("accounts for 4"),
            "a longer data section must be refused, got: {err}"
        );

        // And a truncated one, which the same check catches from the other side.
        let short = tempfile::tempdir().unwrap();
        file(
            short.path(),
            r#"{"t":{"dtype":"F32","shape":[2],"data_offsets":[0,8]}}"#,
            &data[..4],
        );
        let err = format!("{:#}", load_weights(short.path()).unwrap_err());
        assert!(
            err.contains("4 bytes of tensor data") && err.contains("accounts for 8"),
            "a truncated data section must be refused, got: {err}"
        );
    }

    /// Padded columns carry a -1e30 mask into the softmax. Should `exp` return FLT_MIN for them
    /// instead of 0, the `*= 1/sum` step scales that into the 1e-40 range, leaving every masked
    /// column of the scores×V GEMM subnormal.
    #[test]
    fn masked_columns_leave_no_subnormal_weight() {
        let (rows, cols, real) = (4, 512, 384);
        let mut s: Vec<f32> = (0..rows * cols)
            .map(|i| {
                if i % cols < real {
                    (i % 17) as f32 * 0.31 - 2.0
                } else {
                    -1e30
                }
            })
            .collect();
        softmax_rows(&mut s, rows, cols);

        assert!(
            !s.iter().any(|w| w.is_subnormal()),
            "softmax produced a subnormal weight"
        );
        assert!(s.iter().skip(real).take(cols - real).all(|w| *w == 0.0));
        for r in 0..rows {
            let sum: f32 = s[r * cols..(r + 1) * cols].iter().sum();
            assert!((sum - 1.0).abs() < 1e-5, "row {r} sums to {sum}");
        }
    }

    /// `UNDERFLOW` has one correct value and two ways to be wrong: one f32 lower lets a subnormal
    /// through, one f32 higher flushes e^x values that are representable normals to zero. This
    /// walks every f32 across the boundary and rejects both. Linux only — the macOS seam calls
    /// Accelerate's `exp`, which returns the subnormals rather than zeroing them.
    #[cfg(target_os = "linux")]
    #[test]
    fn vexp_cutoff_sits_on_the_subnormal_boundary() {
        let (mut bits, end) = ((-87.5f32).to_bits(), (-87.2f32).to_bits());
        let (mut flushed_a_normal, mut kept_a_subnormal) = (0, 0);
        while bits >= end {
            let v = f32::from_bits(bits);
            let mut buf = [v];
            seam::vexp(&mut buf);
            if buf[0] == 0.0 && (v as f64).exp() >= f32::MIN_POSITIVE as f64 {
                flushed_a_normal += 1;
            }
            if buf[0].is_subnormal() {
                kept_a_subnormal += 1;
            }
            bits -= 1;
        }
        assert_eq!(flushed_a_normal, 0, "zeroed an exp that is a normal f32");
        assert_eq!(kept_a_subnormal, 0, "let a subnormal through");
    }

    /// `by_length` must hand `forward` length-sorted groups of at most `group`, each padded only
    /// to its own longest, and put every output back at its input's position.
    #[test]
    fn by_length_pads_each_group_to_its_own_longest_in_input_order() {
        use tokenizers::Token;
        let group = 8;
        let lens: Vec<usize> = (0..3 * group + 5).map(|i| 1 + (i * 37) % 200).collect();
        let encs = lens
            .iter()
            .map(|&n| {
                let toks = (0..n).map(|t| Token::new(2 + t as u32, String::new(), (0, 0)));
                Encoding::from_tokens(toks.collect(), 0)
            })
            .collect();
        let mut padded = 0;
        let out = by_length(encs, group, |_, mask, n, l| {
            let real: Vec<usize> = mask
                .chunks(l)
                .map(|row| row.iter().filter(|&&m| m == 1).count())
                .collect();
            assert!(n <= group, "a group of {n} exceeds {group}");
            assert_eq!(real.iter().max(), Some(&l), "a group padded past its longest");
            padded += n * l;
            real
        })
        .unwrap();
        assert_eq!(out, lens, "outputs are not in input order");
        let mut sorted = lens.clone();
        sorted.sort();
        let want: usize = sorted.chunks(group).map(|g| g.len() * g[g.len() - 1]).sum();
        assert_eq!(padded, want, "groups are not length-sorted");
    }

    /// Zeroing below the cutoff must leave e^x untouched everywhere above it.
    #[test]
    fn vexp_is_accurate_in_range() {
        let mut got: Vec<f32> = (-870..=880).map(|i| i as f32 / 10.0).collect();
        let want: Vec<f32> = got.iter().map(|x| x.exp()).collect();
        seam::vexp(&mut got);
        for (g, w) in got.iter().zip(&want) {
            assert!((g - w).abs() <= 1e-6 * w, "exp mismatch: {g} vs {w}");
        }
    }
}
