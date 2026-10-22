//! The BERT encoder on the CPU, for the embedders and bge-reranker-base: post-norm blocks with
//! biases, and learned positions added to the embeddings. It shares the seam, the kernels, the
//! loaders and the length-grouped batching of [`super`].

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use tokenizers::Tokenizer;

use super::{
    by_length, ceil_div, gelu, hf_snapshot, l2_normalize, layernorm, linear, load_tokenizer, load_weights, mean_pool,
    par_add, seam, seq_workers, softmax_rows, NO_T, RERANK_GROUP, T,
};
use crate::inference::{Embedder, EmbeddingModel, Reranker};

const PAD: i64 = 1;

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
    /// Sequences per forward. A forward pads every sequence to the longest it holds, so
    /// [`by_length`] runs inputs length-sorted, this many at a time. Measured on real chunk
    /// lengths: fewer shrinks the GEMMs, more brings the padding back. `Scratch` is sized to one
    /// forward, so this also bounds its high-water mark.
    group: usize,
}

const RERANK: Cfg = Cfg {
    prefix: "roberta.",
    h: 768,
    heads: 12,
    hd: 64,
    ffn: 3072,
    layers: 12,
    bert_pos: false,
    group: RERANK_GROUP,
};
const EMBED: Cfg = Cfg {
    prefix: "",
    h: 384,
    heads: 12,
    hd: 32,
    ffn: 1536,
    layers: 12,
    bert_pos: true,
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
        let (w, scratch) = (&self.w, &mut self.scratch);
        let h = RERANK.h;
        by_length(encs, RERANK.group, |ids, mask, n, l| {
            encode(w, RERANK, ids, mask, n, l, scratch);
            let mut cls = vec![0f32; n * h];
            for s in 0..n {
                cls[s * h..s * h + h].copy_from_slice(&scratch.hid[(s * l) * h..(s * l) * h + h]);
            }
            let mut d = vec![0f32; n * h];
            linear(
                &cls,
                n,
                h,
                w["classifier.dense.weight"].as_slice(),
                w["classifier.dense.bias"].as_slice(),
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
                w["classifier.out_proj.weight"].as_slice(),
                w["classifier.out_proj.bias"].as_slice(),
                1,
                &mut scores,
            );
            scores
        })
    }
}
