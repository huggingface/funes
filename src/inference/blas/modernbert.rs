//! gte-reranker-modernbert-base on the CPU: a ModernBERT cross-encoder, mean-pooled into a small
//! classifier head. Unlike the BERT encoder of [`super::bert`], its blocks normalize before
//! attention and before the feed-forward, carry no biases, rotate q and k by position (RoPE)
//! instead of adding a position embedding, and attend over the whole pair only on every third
//! layer: the others see the keys within [`HALF_WINDOW`] positions. It shares the seam, the loaders
//! and the length-grouped batching of [`super`].

use std::collections::HashMap;
use std::ops::Range;

use anyhow::{anyhow, Result};
use tokenizers::{Encoding, Tokenizer};

use super::{
    by_length, ceil_div, gelu, hf_snapshot, load_tokenizer, load_weights, matmul, mean_pool, nthreads, par_add,
    par_rows, seam, seq_workers, softmax_rows, EPS, NO_T, PAR_MIN, RERANK_GROUP, T,
};
use crate::inference::Reranker;

pub(in crate::inference) const REPO: &str = "Alibaba-NLP/gte-reranker-modernbert-base";
pub(in crate::inference) const H: usize = 768;
pub(in crate::inference) const HEADS: usize = 12;
pub(in crate::inference) const HD: usize = H / HEADS;
/// The feed-forward's width: `mlp.Wi` projects to twice this, a GELU half and a gate half.
pub(in crate::inference) const FFN: usize = 1152;
pub(in crate::inference) const LAYERS: usize = 22;
/// The longest pair [`load_tokenizer`] lets through, so the positions RoPE needs.
pub(in crate::inference) const MAX_LEN: usize = 512;
/// How far a window layer's token sees on each side: transformers' `local_attention // 2`.
pub(in crate::inference) const HALF_WINDOW: usize = 64;
const GLOBAL_THETA: f32 = 160_000.0;
const LOCAL_THETA: f32 = 10_000.0;
/// The weights the classifier head reads, after the encoder.
pub(in crate::inference) const HEAD: [&str; 4] = [
    "head.dense.weight",
    "head.norm.weight",
    "classifier.weight",
    "classifier.bias",
];

/// Whether layer `ly` attends over the whole pair: every third layer from the first does.
pub(in crate::inference) fn global(ly: usize) -> bool {
    ly.is_multiple_of(3)
}

/// The keys a query at position `i` attends to, in a sequence of `real` tokens followed by
/// padding: all of them on a full layer, those within [`HALF_WINDOW`] on a window layer.
pub(in crate::inference) fn keys(i: usize, real: usize, window: bool) -> Range<usize> {
    if !window {
        return 0..real;
    }
    let end = (i + HALF_WINDOW + 1).min(real);
    i.saturating_sub(HALF_WINDOW).min(end)..end
}

/// RoPE's cos and sin for each position and frequency, [MAX_LEN × HD/2], built in f32 the way
/// transformers builds them. A head's dims i and i + HD/2 rotate together, by the i-th angle.
pub(in crate::inference) struct Rope {
    pub(in crate::inference) cos: Vec<f32>,
    pub(in crate::inference) sin: Vec<f32>,
}

impl Rope {
    fn new(theta: f32) -> Self {
        let half = HD / 2;
        let inv: Vec<f32> = (0..half)
            .map(|i| 1.0 / theta.powf((2 * i) as f32 / HD as f32))
            .collect();
        let (mut cos, mut sin) = (Vec::with_capacity(MAX_LEN * half), Vec::with_capacity(MAX_LEN * half));
        for t in 0..MAX_LEN {
            for f in &inv {
                let a = t as f32 * f;
                cos.push(a.cos());
                sin.push(a.sin());
            }
        }
        Self { cos, sin }
    }

    /// The full layers' angles.
    pub(in crate::inference) fn global() -> Self {
        Self::new(GLOBAL_THETA)
    }

    /// The window layers' angles.
    pub(in crate::inference) fn local() -> Self {
        Self::new(LOCAL_THETA)
    }

    /// Rotate one head's rows in place: `x` is [l × HD], the row at position t by t's angles.
    fn rotate(&self, x: &mut [f32]) {
        let half = HD / 2;
        for (t, row) in x.chunks_exact_mut(HD).enumerate() {
            let (cos, sin) = (&self.cos[t * half..][..half], &self.sin[t * half..][..half]);
            let (a, b) = row.split_at_mut(half);
            for (((x0, x1), c), s) in a.iter_mut().zip(b.iter_mut()).zip(cos).zip(sin) {
                let (u, v) = (*x0, *x1);
                *x0 = u * c - v * s;
                *x1 = v * c + u * s;
            }
        }
    }
}

/// LayerNorm without a bias, in place over rows of [`H`].
fn norm(x: &mut [f32], g: &[f32]) {
    par_rows(x, H, |row| {
        let hf = H as f32;
        let mean: f32 = row.iter().sum::<f32>() / hf;
        let var: f32 = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / hf;
        let inv = 1.0 / (var + EPS).sqrt();
        for (v, gc) in row.iter_mut().zip(g) {
            *v = (*v - mean) * inv * gc;
        }
    });
}

/// `f(d, s)` over the paired rows of `dst` (`dw` wide) and `src` (`sw` wide), split across
/// threads like [`par_rows`].
fn par_row_pairs<F: Fn(&mut [f32], &[f32]) + Sync>(dst: &mut [f32], dw: usize, src: &[f32], sw: usize, f: F) {
    if dst.len() <= PAR_MIN {
        for (d, s) in dst.chunks_exact_mut(dw).zip(src.chunks_exact(sw)) {
            f(d, s);
        }
        return;
    }
    let per = ceil_div(dst.len() / dw, nthreads()).max(1);
    std::thread::scope(|scope| {
        for (d, s) in dst.chunks_mut(per * dw).zip(src.chunks(per * sw)) {
            let f = &f;
            scope.spawn(move || {
                for (d, s) in d.chunks_exact_mut(dw).zip(s.chunks_exact(sw)) {
                    f(d, s);
                }
            });
        }
    });
}

/// GeGLU: the GELU of each `Wi` row's first half, times its second half.
fn geglu(inter: &[f32], glu: &mut [f32]) {
    par_row_pairs(glu, FFN, inter, 2 * FFN, |g, x| {
        g.copy_from_slice(&x[..FFN]);
        gelu(g);
        for (a, b) in g.iter_mut().zip(&x[FFN..]) {
            *a *= b;
        }
    });
}

/// Self-attention of every sequence and head, from the fused q|k|v rows [n·l × 3H] into `ctx`
/// [n·l × H]: q and k rotated by `rope`, the keys outside [`keys`] masked out.
fn attend(qkv: &[f32], mask: &[i64], n: usize, l: usize, rope: &Rope, window: bool, ctx: &mut [f32]) {
    let scale = 1.0 / (HD as f32).sqrt();
    let seqs_per = ceil_div(n, seq_workers(n));
    std::thread::scope(|scope| {
        for (gi, grp) in ctx.chunks_mut(seqs_per * l * H).enumerate() {
            scope.spawn(move || {
                let (mut qh, mut kh, mut vh) = (vec![0f32; l * HD], vec![0f32; l * HD], vec![0f32; l * HD]);
                let (mut sc, mut ch) = (vec![0f32; l * l], vec![0f32; l * HD]);
                for (si, out) in grp.chunks_mut(l * H).enumerate() {
                    let s = gi * seqs_per + si;
                    let real = mask[s * l..(s + 1) * l].iter().filter(|&&m| m != 0).count();
                    for head in 0..HEADS {
                        let at = head * HD;
                        for i in 0..l {
                            let row = &qkv[(s * l + i) * 3 * H..][..3 * H];
                            qh[i * HD..][..HD].copy_from_slice(&row[at..][..HD]);
                            kh[i * HD..][..HD].copy_from_slice(&row[H + at..][..HD]);
                            vh[i * HD..][..HD].copy_from_slice(&row[2 * H + at..][..HD]);
                        }
                        rope.rotate(&mut qh);
                        rope.rotate(&mut kh);
                        seam::sgemm(NO_T, T, l, l, HD, scale, &qh, HD, &kh, HD, &mut sc);
                        for (i, row) in sc.chunks_exact_mut(l).enumerate() {
                            let seen = keys(i, real, window);
                            row[..seen.start].fill(-1e30);
                            row[seen.end..].fill(-1e30);
                        }
                        softmax_rows(&mut sc, l, l);
                        seam::sgemm(NO_T, NO_T, l, HD, l, 1.0, &sc, l, &vh, HD, &mut ch);
                        for i in 0..l {
                            out[i * H + at..][..HD].copy_from_slice(&ch[i * HD..][..HD]);
                        }
                    }
                }
            });
        }
    });
}

/// Forward-pass buffers, owned by the model and reused across calls (see `super::bert::Scratch`).
#[derive(Default)]
struct Scratch {
    /// The residual stream, and its normalized copy for the block reading it.
    hid: Vec<f32>,
    norm: Vec<f32>,
    qkv: Vec<f32>,
    ctx: Vec<f32>,
    attn: Vec<f32>,
    inter: Vec<f32>,
    glu: Vec<f32>,
    ffn_out: Vec<f32>,
}

/// The two RoPE tables, one per kind of layer.
struct Ropes {
    global: Rope,
    local: Rope,
}

/// The encoder over `n` sequences of `l` tokens: `s.hid` [n·l × H] ends as the final norm's output.
/// `mask[s·l + j]` is 1 for a real token, 0 for padding.
fn encode(
    w: &HashMap<String, Vec<f32>>,
    ropes: &Ropes,
    ids: &[i64],
    mask: &[i64],
    n: usize,
    l: usize,
    s: &mut Scratch,
) {
    let g = |k: &str| w.get(k).unwrap_or_else(|| panic!("missing weight {k}")).as_slice();
    let m = n * l;
    for (buf, len) in [
        (&mut s.hid, m * H),
        (&mut s.norm, m * H),
        (&mut s.qkv, m * 3 * H),
        (&mut s.ctx, m * H),
        (&mut s.attn, m * H),
        (&mut s.inter, m * 2 * FFN),
        (&mut s.glu, m * FFN),
        (&mut s.ffn_out, m * H),
    ] {
        buf.resize(len, 0.0);
    }
    let emb = g("model.embeddings.tok_embeddings.weight");
    for (row, &id) in s.hid.chunks_exact_mut(H).zip(ids) {
        row.copy_from_slice(&emb[id as usize * H..][..H]);
    }
    norm(&mut s.hid, g("model.embeddings.norm.weight"));

    for ly in 0..LAYERS {
        let p = format!("model.layers.{ly}");
        // Layer 0 has no norm before attention: the embeddings' norm stands in for it.
        let x = if ly == 0 {
            &s.hid
        } else {
            s.norm.copy_from_slice(&s.hid);
            norm(&mut s.norm, g(&format!("{p}.attn_norm.weight")));
            &s.norm
        };
        matmul(x, m, H, g(&format!("{p}.attn.Wqkv.weight")), 3 * H, &mut s.qkv);
        let (rope, window) = if global(ly) {
            (&ropes.global, false)
        } else {
            (&ropes.local, true)
        };
        attend(&s.qkv, mask, n, l, rope, window, &mut s.ctx);
        matmul(&s.ctx, m, H, g(&format!("{p}.attn.Wo.weight")), H, &mut s.attn);
        par_add(&mut s.hid, &s.attn);

        s.norm.copy_from_slice(&s.hid);
        norm(&mut s.norm, g(&format!("{p}.mlp_norm.weight")));
        matmul(&s.norm, m, H, g(&format!("{p}.mlp.Wi.weight")), 2 * FFN, &mut s.inter);
        geglu(&s.inter, &mut s.glu);
        matmul(&s.glu, m, FFN, g(&format!("{p}.mlp.Wo.weight")), H, &mut s.ffn_out);
        par_add(&mut s.hid, &s.ffn_out);
    }
    norm(&mut s.hid, g("model.final_norm.weight"));
}

/// The classifier over mean-pooled rows [n × H]: dense, GELU, norm, then one logit a row. `w` holds
/// at least the [`HEAD`] weights.
pub(in crate::inference) fn head(w: &HashMap<String, Vec<f32>>, pooled: &[f32], n: usize) -> Vec<f32> {
    let [dense, gamma, cw, cb] = HEAD.map(|name| w[name].as_slice());
    let mut d = vec![0f32; n * H];
    matmul(pooled, n, H, dense, H, &mut d);
    gelu(&mut d);
    norm(&mut d, gamma);
    d.chunks_exact(H)
        .map(|row| row.iter().zip(cw).map(|(a, b)| a * b).sum::<f32>() + cb[0])
        .collect()
}

/// Each (query, doc) pair as `[CLS] query [SEP] doc [SEP]`, cut to [`MAX_LEN`] longest first.
pub(in crate::inference) fn encode_pairs(tok: &Tokenizer, query: &str, docs: &[&str]) -> Result<Vec<Encoding>> {
    let pairs: Vec<(&str, &str)> = docs.iter().map(|d| (query, *d)).collect();
    tok.encode_batch(pairs, true).map_err(|e| anyhow!("tokenize: {e}"))
}

/// gte-reranker-modernbert-base cross-encoder: ModernBERT encoder → mean pool → [`head`].
pub struct GteReranker {
    w: HashMap<String, Vec<f32>>,
    tok: Tokenizer,
    ropes: Ropes,
    scratch: Scratch,
}

impl GteReranker {
    pub fn new() -> Result<Self> {
        let dir = hf_snapshot(REPO)?;
        Ok(Self {
            w: load_weights(&dir)?,
            tok: load_tokenizer(&dir)?,
            ropes: Ropes {
                global: Rope::global(),
                local: Rope::local(),
            },
            scratch: Scratch::default(),
        })
    }
}

impl Reranker for GteReranker {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        let encs = encode_pairs(&self.tok, query, docs)?;
        let (w, ropes, scratch) = (&self.w, &self.ropes, &mut self.scratch);
        by_length(encs, RERANK_GROUP, |ids, mask, n, l| {
            encode(w, ropes, ids, mask, n, l, scratch);
            let pooled: Vec<f32> = (0..n)
                .flat_map(|s| mean_pool(&scratch.hid[s * l * H..(s + 1) * l * H], &mask[s * l..(s + 1) * l], H))
                .collect();
            head(w, &pooled, n)
        })
    }
}

/// The pairs and logits the forwards are checked against.
#[cfg(test)]
pub(in crate::inference) mod reference {
    /// The model card's three pairs, and what transformers 5.20 scores them in fp32.
    pub const CARD: [(&str, &str, f32); 3] = [
        ("what is the capital of China?", "Beijing", 2.138259),
        (
            "how to implement quick sort in python?",
            "Introduction of quick sort",
            2.46096,
        ),
        (
            "how to implement quick sort in python?",
            "The weather is nice today",
            -1.677548,
        ),
    ];

    pub const QUERY: &str = "why did the reranker move to the gpu";
    const SENT: &str =
        "the reranker scored each candidate against the query on the gpu, and recall kept the eight best. ";
    const OTHER: &str = "the weather stayed nice all week, so the team went hiking instead of reading logs. ";

    /// Passages for [`QUERY`] from 37 tokens to the 512-token cap, so most of them reach past the
    /// window, with what transformers 5.20 scores each in fp32, scored alone. A window one token
    /// narrower or wider moves the logits of the pairs past 65 tokens by 1.4e-3 to 6.5e-3.
    pub fn long() -> Vec<(String, f32)> {
        vec![
            (SENT.to_string(), 2.33419),
            (SENT.repeat(3), 2.385389),
            (SENT.repeat(9), 2.274217),
            (SENT.repeat(20), 2.268834),
            (SENT.repeat(40), 2.276644),
            (OTHER.repeat(12), 0.7769119),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CPU forward scores what transformers does: the card's pairs one query at a time, and the
    /// long pairs in one call, whose groups pad the shorter ones.
    #[test]
    fn logits_match_transformers() {
        let mut rr = GteReranker::new().expect("gte weights");
        let mut got = Vec::new();
        let mut want = Vec::new();
        for (q, p, logit) in reference::CARD {
            got.extend(rr.rerank(q, &[p]).unwrap());
            want.push(logit);
        }
        let long = reference::long();
        let docs: Vec<&str> = long.iter().map(|(p, _)| p.as_str()).collect();
        got.extend(rr.rerank(reference::QUERY, &docs).unwrap());
        want.extend(long.iter().map(|(_, logit)| *logit));
        let worst = got.iter().zip(&want).map(|(g, w)| (g - w).abs()).fold(0f32, f32::max);
        eprintln!("largest logit difference to transformers: {worst:.2e}");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert!((g - w).abs() < 5e-4, "pair {i}: {g} against transformers' {w}");
        }
    }

    /// Dims i and i + HD/2 of a head rotate together, by position t times θ^(-2i/HD).
    #[test]
    fn rope_rotates_each_dim_with_its_partner_half_a_head_away() {
        let rope = Rope::local();
        let (t, i) = (5, 3);
        let mut x = vec![0f32; (t + 1) * HD];
        x[t * HD + i] = 1.0;
        rope.rotate(&mut x);
        let angle = t as f64 * (LOCAL_THETA as f64).powf(-2.0 * i as f64 / HD as f64);
        assert!((x[t * HD + i] as f64 - angle.cos()).abs() < 1e-5);
        assert!((x[t * HD + i + HD / 2] as f64 - angle.sin()).abs() < 1e-5);
        let moved = x.iter().enumerate().filter(|(_, v)| **v != 0.0).count();
        assert_eq!(moved, 2, "only the dim and its partner move: {x:?}");
    }

    /// Full layers are 0, 3, ..., 21. A window layer sees 64 positions each side of the query,
    /// within the real tokens.
    #[test]
    fn window_layers_see_64_positions_each_side() {
        let full: Vec<usize> = (0..LAYERS).filter(|&ly| global(ly)).collect();
        assert_eq!(full, [0, 3, 6, 9, 12, 15, 18, 21]);
        assert_eq!(keys(100, 300, false), 0..300);
        assert_eq!(keys(100, 300, true), 36..165);
        assert_eq!(keys(10, 300, true), 0..75);
        assert_eq!(keys(290, 300, true), 226..300);
        assert!(
            keys(400, 300, true).is_empty(),
            "a padded row far past the real tokens sees none"
        );
    }
}
