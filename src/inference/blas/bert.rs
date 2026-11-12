//! The BERT encoder on the CPU, for the embedders: post-norm blocks with biases, and learned
//! positions added to the embeddings. It shares the seam, the kernels, the loaders and the
//! length-grouped batching of [`super`].

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use tokenizers::Tokenizer;

use super::{
    by_length, ceil_div, gelu, hf_snapshot, l2_normalize, layernorm, linear, load_tokenizer, load_weights, mean_pool,
    par_add, seam, seq_workers, softmax_rows, NO_T, T,
};
use crate::inference::{Embedder, EmbeddingModel};

/// The BERT encoder's shape.
#[derive(Clone, Copy)]
struct Cfg {
    h: usize,
    heads: usize,
    hd: usize,
    ffn: usize,
    layers: usize,
    /// Sequences per forward. A forward pads every sequence to the longest it holds, so
    /// [`by_length`] runs inputs length-sorted, this many at a time. Measured on real chunk
    /// lengths: fewer shrinks the GEMMs, more brings the padding back. `Scratch` is sized to one
    /// forward, so this also bounds its high-water mark.
    group: usize,
}

const EMBED: Cfg = Cfg {
    h: 384,
    heads: 12,
    hd: 32,
    ffn: 1536,
    layers: 12,
    group: 16,
};

/// Forward-pass buffers, owned by the model and reused across calls, growing to the high-water
/// mark. Freeing them between forwards hands the pages back to the OS, and the next call pays a
/// page fault per 4 KiB rewritten — hundreds of MB per forward (see matmul()).
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
    let word = g("embeddings.word_embeddings.weight");
    let pos = g("embeddings.position_embeddings.weight");
    let typ = g("embeddings.token_type_embeddings.weight");
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
                    for i in 0..l {
                        let id = ids[sq * l + i];
                        let dst = &mut seq[i * h..i * h + h];
                        let we = &word[id as usize * h..id as usize * h + h];
                        let pe = &pos[i * h..i * h + h];
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
        g("embeddings.LayerNorm.weight"),
        g("embeddings.LayerNorm.bias"),
    );

    let scale = 1.0 / (hd as f32).sqrt();
    for ly in 0..c.layers {
        let p = format!("encoder.layer.{ly}");
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

/// Sentence embedder: BERT encoder → pooling → L2-normalize. Both models have the [`EMBED`] shape.
pub struct BlasEmbedder {
    model: EmbeddingModel,
    w: HashMap<String, Vec<f32>>,
    tok: Tokenizer,
    scratch: Scratch,
}

impl BlasEmbedder {
    pub fn new(model: EmbeddingModel) -> Result<Self> {
        let dir = hf_snapshot(model.id())?;
        Ok(Self {
            model,
            w: load_weights(&dir)?,
            tok: load_tokenizer(&dir)?,
            scratch: Scratch::default(),
        })
    }
}

impl Embedder for BlasEmbedder {
    fn model(&self) -> EmbeddingModel {
        self.model
    }

    fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let encs = self
            .tok
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow!("tokenize: {e}"))?;
        let (model, w, scratch) = (self.model, &self.w, &mut self.scratch);
        let h = EMBED.h;
        by_length(encs, EMBED.group, |ids, mask, n, l| {
            encode(w, EMBED, ids, mask, n, l, scratch);
            (0..n)
                .map(|s| {
                    let rows = &scratch.hid[s * l * h..(s + 1) * l * h];
                    let mut e = match model {
                        EmbeddingModel::BgeSmallEn => rows[..h].to_vec(),
                        EmbeddingModel::MultilingualE5Small => mean_pool(rows, &mask[s * l..(s + 1) * l], h),
                    };
                    l2_normalize(&mut e);
                    e
                })
                .collect()
        })
    }
}
