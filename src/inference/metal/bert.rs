//! The embedder on the GPU: the BERT encoder of [`crate::inference::blas::bert`], built as an
//! MPSGraph and run in fp16. Indexing is embedding-bound, and the GPU runs it faster than the CPU
//! forward, agreeing with it to a cosine of 0.99999.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{anyhow, Result};
use objc2::rc::Retained;
use objc2_metal_performance_shaders::MPSDataType;
use objc2_metal_performance_shaders_graph::MPSGraphTensor;
use tokenizers::Tokenizer;

use super::{pad_group, shape, transposed, Gpu, Graphs, Inputs, EPS, MASKED};
use crate::inference::blas::{hf_snapshot, l2_normalize, load_tokenizer, load_weights};
use crate::inference::{DefaultEmbedder, Embedder, EmbeddingModel, EMBED_BATCH};

const H: usize = 384;
const HEADS: usize = 12;
const HD: usize = H / HEADS;
const FFN: usize = 1536;
const LAYERS: usize = 12;
/// Sequences per forward. On real chunk lengths, fewer leave the GPU idle between forwards, and
/// more bring the padding back.
const GROUP: usize = 64;

/// BERT encoder → pooling → L2-normalize, on the GPU. Both embedding models share the shape.
pub struct MetalEmbedder {
    model: EmbeddingModel,
    tok: Tokenizer,
    graphs: Graphs,
}

impl MetalEmbedder {
    /// Fails where there is no usable [`device`], or the model's weights cannot be loaded.
    pub fn new(model: EmbeddingModel) -> Result<Self> {
        let mut graphs = Graphs::new()?;
        let dir = hf_snapshot(model.id())?;
        let tok = load_tokenizer(&dir)?;
        for (name, vals) in load_weights(&dir)? {
            if let Some((dims, vals)) = graph_layout(&name, vals) {
                graphs.upload(name, dims, &vals);
            }
        }
        Ok(Self { model, tok, graphs })
    }

    /// One forward over `b` sequences padded to `l`: `ids` and `mask` are [b·l], row-major. Returns
    /// the [b·H] pooled rows.
    fn forward(&mut self, ids: &[i32], mask: &[f32], b: usize, l: usize) -> Result<Vec<f32>> {
        let model = self.model;
        self.graphs.run(b, l, ids, mask, b * H, |x| bert(x, model, b, l))
    }
}

/// The BERT encoder and `model`'s pooling for `b` sequences of `l` tokens: [b, H] in fp32.
fn bert(x: Inputs<'_>, model: EmbeddingModel, b: usize, l: usize) -> Result<Retained<MPSGraphTensor>> {
    let Inputs { g, ids, mask, w } = x;
    let f16 = MPSDataType::Float16;
    unsafe {
        let scalar = |v: f64| g.constantWithScalar_dataType(v, f16);
        let add = |x: &MPSGraphTensor, y: &MPSGraphTensor| g.additionWithPrimaryTensor_secondaryTensor_name(x, y, None);
        let mul =
            |x: &MPSGraphTensor, y: &MPSGraphTensor| g.multiplicationWithPrimaryTensor_secondaryTensor_name(x, y, None);
        let reshape = |x: &MPSGraphTensor, dims: &[usize]| g.reshapeTensor_withShape_name(x, &shape(dims), None);
        let last = shape(&[1]);
        let layer_norm = |x: &MPSGraphTensor, gamma: &MPSGraphTensor, beta: &MPSGraphTensor| {
            let mean = g.meanOfTensor_axes_name(x, &last, None);
            let var = g.varianceOfTensor_meanTensor_axes_name(x, &mean, &last, None);
            g.normalizationWithTensor_meanTensor_varianceTensor_gammaTensor_betaTensor_epsilon_name(
                x,
                &mean,
                &var,
                Some(gamma),
                Some(beta),
                EPS,
                None,
            )
        };

        // Embeddings: word + position + token type 0, then layer norm. Hidden states stay
        // [b·l, H] so every linear is one 2-D matmul.
        let word = g.gatherWithUpdatesTensor_indicesTensor_axis_batchDimensions_name(
            &*w("embeddings.word_embeddings.weight")?,
            ids,
            0,
            0,
            None,
        );
        let pos = g.sliceTensor_dimension_start_length_name(
            &*w("embeddings.position_embeddings.weight")?,
            0,
            0,
            l as isize,
            None,
        );
        let typ =
            g.sliceTensor_dimension_start_length_name(&*w("embeddings.token_type_embeddings.weight")?, 0, 0, 1, None);
        let x = add(&add(&reshape(&word, &[b, l, H]), &pos), &typ);
        let mut x = layer_norm(
            &reshape(&x, &[b * l, H]),
            &*w("embeddings.LayerNorm.weight")?,
            &*w("embeddings.LayerNorm.bias")?,
        );

        // Padded columns get MASKED added to their attention scores: (mask − 1)·30000.
        let bias = mul(&add(mask, &scalar(-1.0)), &scalar(-MASKED));
        let bias = reshape(&bias, &[b, 1, 1, l]);
        let (half, one, inv_sqrt2) = (scalar(0.5), scalar(1.0), scalar(std::f64::consts::FRAC_1_SQRT_2));
        for ly in 0..LAYERS {
            let p = format!("encoder.layer.{ly}");
            let linear = |x: &MPSGraphTensor, n: &str| -> Result<Retained<MPSGraphTensor>> {
                let y = g.matrixMultiplicationWithPrimaryTensor_secondaryTensor_name(
                    x,
                    &*w(&format!("{p}.{n}.weight"))?,
                    None,
                );
                Ok(add(&y, &*w(&format!("{p}.{n}.bias"))?))
            };
            let heads = |t: &MPSGraphTensor| {
                g.transposeTensor_permutation_name(&reshape(t, &[b, l, HEADS, HD]), &shape(&[0, 2, 1, 3]), None)
            };
            let q = heads(&*linear(&x, "attention.self.query")?);
            let k = heads(&*linear(&x, "attention.self.key")?);
            let v = heads(&*linear(&x, "attention.self.value")?);
            let ctx = g.scaledDotProductAttentionWithQueryTensor_keyTensor_valueTensor_maskTensor_scale_name(
                &q,
                &k,
                &v,
                Some(&bias),
                1.0 / (HD as f32).sqrt(),
                None,
            );
            let ctx = reshape(
                &g.transposeTensor_permutation_name(&ctx, &shape(&[0, 2, 1, 3]), None),
                &[b * l, H],
            );
            let attn = linear(&ctx, "attention.output.dense")?;
            x = layer_norm(
                &add(&x, &attn),
                &*w(&format!("{p}.attention.output.LayerNorm.weight"))?,
                &*w(&format!("{p}.attention.output.LayerNorm.bias"))?,
            );
            // Exact GELU: 0.5·x·(1 + erf(x/√2)).
            let inter = linear(&x, "intermediate.dense")?;
            let erf = g.erfWithTensor_name(&mul(&inter, &inv_sqrt2), None);
            let inter = mul(&mul(&inter, &add(&erf, &one)), &half);
            let out = linear(&inter, "output.dense")?;
            x = layer_norm(
                &add(&x, &out),
                &*w(&format!("{p}.output.LayerNorm.weight"))?,
                &*w(&format!("{p}.output.LayerNorm.bias"))?,
            );
        }

        // Pool in fp32: bge takes the CLS row, e5 the mean of the real tokens'.
        let x = g.castTensor_toType_name(&reshape(&x, &[b, l, H]), MPSDataType::Float32, None);
        let pooled = match model {
            EmbeddingModel::BgeSmallEn => g.sliceTensor_dimension_start_length_name(&x, 1, 0, 1, None),
            EmbeddingModel::MultilingualE5Small => {
                let m = g.castTensor_toType_name(&reshape(mask, &[b, l, 1]), MPSDataType::Float32, None);
                let axis = shape(&[1]);
                let sum = g.reductionSumWithTensor_axes_name(&mul(&x, &m), Some(&axis), None);
                let kept = g.reductionSumWithTensor_axes_name(&m, Some(&axis), None);
                g.divisionWithPrimaryTensor_secondaryTensor_name(&sum, &kept, None)
            }
        };
        Ok(reshape(&pooled, &[b, H]))
    }
}

/// A weight as the graph takes it: a linear's [out, in] transposed to [in, out], so `x·W` needs no
/// transpose per forward; `None` for a tensor the encoder does not use (the pooler).
fn graph_layout(name: &str, vals: Vec<f32>) -> Option<(Vec<usize>, Vec<f32>)> {
    if !(name.starts_with("embeddings.") || name.starts_with("encoder.")) {
        return None;
    }
    if name.ends_with("LayerNorm.weight") || name.ends_with("LayerNorm.bias") || name.ends_with(".bias") {
        return Some((vec![vals.len()], vals));
    }
    if name.ends_with("intermediate.dense.weight") {
        return Some(transposed(&vals, FFN, H));
    }
    if name.ends_with("output.dense.weight") && !name.contains("attention") {
        return Some(transposed(&vals, H, FFN));
    }
    if name.starts_with("encoder.") {
        return Some(transposed(&vals, H, H));
    }
    // Embedding tables: [rows, H].
    let rows = vals.len() / H;
    Some((vec![rows, H], vals))
}

impl Embedder for MetalEmbedder {
    fn model(&self) -> EmbeddingModel {
        self.model
    }

    fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // A tool result read twice is one text: embed each distinct text once.
        let mut slot: HashMap<&str, usize> = HashMap::new();
        let mut unique: Vec<&str> = Vec::new();
        let at: Vec<usize> = texts
            .iter()
            .map(|t| {
                *slot.entry(t).or_insert_with(|| {
                    unique.push(t);
                    unique.len() - 1
                })
            })
            .collect();
        let encs = self
            .tok
            .encode_batch(unique, true)
            .map_err(|e| anyhow!("tokenize: {e}"))?;
        let mut order: Vec<usize> = (0..encs.len()).collect();
        order.sort_by_key(|&i| encs[i].get_ids().len());
        let mut vectors = vec![Vec::new(); encs.len()];
        for group in order.chunks(GROUP) {
            let (b, l, ids, mask) = pad_group(&encs, group);
            let pooled = self.forward(&ids, &mask, b, l)?;
            for (row, &i) in group.iter().enumerate() {
                let mut e = pooled[row * H..(row + 1) * H].to_vec();
                l2_normalize(&mut e);
                vectors[i] = e;
            }
        }
        Ok(at.into_iter().map(|i| vectors[i].clone()).collect())
    }
}

/// Seconds of work a GPU call should carry: a caller reports progress and checks its budget
/// between calls, and the rate that sizes the call is measured, so this holds on any GPU.
const GPU_CALL_SECS: f64 = 2.0;
/// Texts per GPU call at most: larger calls embed no faster per text.
const GPU_BATCH: usize = 2048;

/// Calls of at least this many texts bring the GPU up. Below it the CPU forward finishes before the
/// GPU would have its weights uploaded and its first graph compiled; once it is up, every call runs
/// there.
const GPU_MIN_TEXTS: usize = 32;

/// The CPU forward for small calls — a recall query, the few chunks of one turn — and the GPU for
/// bulk ones, each brought up on its first call. A GPU that cannot start, or fails a call, is given
/// up on: that call and every one after run on the CPU.
pub struct Hybrid {
    model: EmbeddingModel,
    cpu: Option<DefaultEmbedder>,
    gpu: Gpu<MetalEmbedder>,
    /// Texts per second of the last GPU call, which sizes the next.
    gpu_rate: Option<f64>,
}

impl Hybrid {
    pub fn new(model: EmbeddingModel) -> Result<Self> {
        // Fetch the weights now, so a failed download fails here, as an eager backend's would.
        hf_snapshot(model.id())?;
        Ok(Self {
            model,
            cpu: None,
            gpu: Gpu::Untried,
            gpu_rate: None,
        })
    }

    fn cpu(&mut self) -> Result<&mut DefaultEmbedder> {
        if self.cpu.is_none() {
            self.cpu = Some(DefaultEmbedder::new(self.model)?);
        }
        Ok(self.cpu.as_mut().expect("just set"))
    }
}

impl Embedder for Hybrid {
    fn model(&self) -> EmbeddingModel {
        self.model
    }

    fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let on_gpu = match self.gpu {
            Gpu::Ready(_) => true,
            Gpu::Untried => texts.len() >= GPU_MIN_TEXTS,
            Gpu::Unavailable => false,
        };
        if on_gpu {
            let model = self.model;
            if let Some(gpu) = self.gpu.get("embedding", || MetalEmbedder::new(model)) {
                let t0 = Instant::now();
                match gpu.encode(texts) {
                    Ok(vectors) => {
                        self.gpu_rate = Some(texts.len() as f64 / t0.elapsed().as_secs_f64().max(1e-3));
                        return Ok(vectors);
                    }
                    Err(e) => self.gpu.give_up("embedding", e),
                }
            }
        }
        self.cpu()?.encode(texts)
    }

    /// [`GPU_CALL_SECS`] of work at the last GPU call's rate, once a call has measured it: until
    /// then callers size their work for the CPU.
    fn batch_size(&self) -> usize {
        match (&self.gpu, self.gpu_rate) {
            (Gpu::Ready(_), Some(rate)) => ((rate * GPU_CALL_SECS) as usize).clamp(EMBED_BATCH, GPU_BATCH),
            _ => EMBED_BATCH,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::blas::BlasEmbedder;

    /// The GPU embeds what the CPU forward does, for both models, across group sizes and padded
    /// lengths, with a repeated text embedded once and handed back at each of its positions.
    #[test]
    fn gpu_embeddings_agree_with_the_cpu_forward() {
        let sentence = "the reranker rescored each candidate against the query before recall returned it. ";
        let mut texts: Vec<String> = (0..70)
            .map(|i| format!("{i}: {}", sentence.repeat(1 + i % 23)))
            .collect();
        texts.push(texts[3].clone());
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        if let Err(e) = super::super::device() {
            eprintln!("skipping: {e:#}");
            return;
        }
        for model in EmbeddingModel::ALL {
            let mut gpu = MetalEmbedder::new(model).unwrap_or_else(|e| panic!("{}: {e:#}", model.id()));
            let got = gpu.embed(&texts).unwrap();
            let want = BlasEmbedder::new(model).unwrap().embed(&texts).unwrap();
            assert_eq!(got.len(), texts.len());
            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                let cos: f32 = g.iter().zip(w).map(|(a, b)| a * b).sum();
                assert!(cos > 0.9999, "{} text {i}: cosine {cos}", model.id());
            }
            assert_eq!(got[3], got[70], "a repeated text must embed alike");
        }
    }
}
