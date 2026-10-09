//! The embedder on the GPU: the same BERT encoder as [`super::blas`], built as an MPSGraph and run
//! in fp16 on Metal. Indexing is embedding-bound, and the GPU runs it about fifty times faster than
//! the CPU forward, agreeing with it to a cosine of 0.99999. macOS only; [`super::embedder`] falls
//! back to the CPU forward wherever this one cannot start.
//!
//! A graph is compiled per (batch, padded length) bucket, the first time a forward needs it: inputs
//! are sorted by length and padded only to their group's longest, rounded up to [`ROUND`] so a run
//! compiles a handful of buckets, not one per length. The weights are fed to every bucket from the
//! same GPU buffers rather than baked into each graph as constants, so the buckets share one copy.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;

use anyhow::{anyhow, bail, Context, Result};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2::{sel, AnyThread};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSNumber, NSObjectProtocol};
use objc2_metal::{MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice};
use objc2_metal_performance_shaders::MPSDataType;
use objc2_metal_performance_shaders_graph::{
    MPSGraph, MPSGraphCompilationDescriptor, MPSGraphDevice, MPSGraphExecutable, MPSGraphOptimization,
    MPSGraphShapedType, MPSGraphTensor, MPSGraphTensorData,
};
use tokenizers::Tokenizer;

use super::blas::{hf_snapshot, l2_normalize, load_tokenizer, load_weights};
use super::{Embedder, EmbeddingModel};

const H: usize = 384;
const HEADS: usize = 12;
const HD: usize = H / HEADS;
const FFN: usize = 1536;
const LAYERS: usize = 12;
const MAX_LEN: usize = 512;
/// The CPU forward's layer-norm epsilon, so both backends normalize alike. (The models' configs say
/// 1e-12, which fp16 cannot hold.)
const EPS: f32 = 1e-5;
/// Sequences per forward. Measured on real chunk lengths on an M5 Max: 32 leaves the GPU idle
/// between forwards, 128 brings the padding back.
const GROUP: usize = 64;
/// The smallest batch a forward pads a short group to.
const MIN_GROUP: usize = 8;
/// Padded lengths are rounded up to a multiple of this, which bounds the buckets a run compiles at
/// 512/ROUND per group size. 16 compiles twice the buckets for no gain; 64 pads too much.
const ROUND: usize = 32;
/// What a padded column adds to its attention scores: far enough below any real score that its
/// softmax weight is zero, and inside fp16's range.
const MASKED: f64 = -30000.0;

fn shape(dims: &[usize]) -> Retained<NSArray<NSNumber>> {
    let dims: Vec<Retained<NSNumber>> = dims.iter().map(|&d| NSNumber::new_isize(d as isize)).collect();
    NSArray::from_retained_slice(&dims)
}

fn f16_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| half::f16::from_f32(*x).to_le_bytes()).collect()
}

/// What an executable expects at each of its inputs, in its own feed order.
enum Feed {
    Ids,
    Mask,
    Weight(String),
}

struct Bucket {
    exec: Retained<MPSGraphExecutable>,
    feeds: Vec<Feed>,
}

/// BERT encoder → pooling → L2-normalize, on the GPU. Both embedding models share the shape.
pub struct MetalEmbedder {
    model: EmbeddingModel,
    tok: Tokenizer,
    device: Retained<MPSGraphDevice>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// Each weight's shape in the graph and its data, resident on the GPU.
    weights: HashMap<String, (Vec<usize>, Retained<MPSGraphTensorData>)>,
    buckets: HashMap<(usize, usize), Bucket>,
}

// SAFETY: the Metal device and command queue are thread-safe; the graphs, executables and tensor
// data are only ever touched through `&mut self`, so moving the embedder to another thread never
// shares them.
unsafe impl Send for MetalEmbedder {}

impl MetalEmbedder {
    /// Fails, for the caller to fall back to the CPU, when there is no Metal device or its
    /// MPSGraph predates fused attention (macOS 15).
    pub fn new(model: EmbeddingModel) -> Result<Self> {
        let mtl = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let attention = sel!(scaledDotProductAttentionWithQueryTensor:keyTensor:valueTensor:maskTensor:scale:name:);
        if !unsafe { MPSGraph::new() }.respondsToSelector(attention) {
            bail!("this MPSGraph has no fused attention (macOS 15 or later)");
        }
        let queue = mtl.newCommandQueue().context("no Metal command queue")?;
        let device = unsafe { MPSGraphDevice::deviceWithMTLDevice(&mtl) };
        let dir = hf_snapshot(model.id())?;
        let tok = load_tokenizer(&dir)?;
        let mut weights = HashMap::new();
        for (name, vals) in load_weights(&dir)? {
            let Some((dims, vals)) = graph_layout(&name, vals) else {
                continue;
            };
            let data = autoreleasepool(|_| unsafe {
                MPSGraphTensorData::initWithDevice_data_shape_dataType(
                    MPSGraphTensorData::alloc(),
                    &device,
                    &NSData::from_vec(f16_bytes(&vals)),
                    &shape(&dims),
                    MPSDataType::Float16,
                )
            });
            weights.insert(name, (dims, data));
        }
        Ok(Self {
            model,
            tok,
            device,
            queue,
            weights,
            buckets: HashMap::new(),
        })
    }

    fn bucket(&mut self, b: usize, l: usize) -> Result<&Bucket> {
        if !self.buckets.contains_key(&(b, l)) {
            let bucket = autoreleasepool(|_| self.compile(b, l))?;
            self.buckets.insert((b, l), bucket);
        }
        Ok(&self.buckets[&(b, l)])
    }

    /// Build the encoder for `b` sequences of `l` tokens and compile it. Optimization level 0: level
    /// 1 also tries placing ops on the Neural Engine, which costs seconds of compile per bucket and
    /// runs no faster.
    fn compile(&self, b: usize, l: usize) -> Result<Bucket> {
        let g = unsafe { MPSGraph::new() };
        let f16 = MPSDataType::Float16;
        let weight_feeds: RefCell<Vec<(Retained<MPSGraphTensor>, String)>> = RefCell::new(Vec::new());
        let w = |name: &str| -> Result<Retained<MPSGraphTensor>> {
            let (dims, _) = self.weights.get(name).ok_or_else(|| anyhow!("missing weight {name}"))?;
            let t = unsafe { g.placeholderWithShape_dataType_name(Some(&shape(dims)), f16, None) };
            weight_feeds.borrow_mut().push((t.clone(), name.to_string()));
            Ok(t)
        };
        unsafe {
            let ids = g.placeholderWithShape_dataType_name(Some(&shape(&[b * l])), MPSDataType::Int32, None);
            let mask = g.placeholderWithShape_dataType_name(Some(&shape(&[b, l])), f16, None);
            let scalar = |v: f64| g.constantWithScalar_dataType(v, f16);
            let add =
                |x: &MPSGraphTensor, y: &MPSGraphTensor| g.additionWithPrimaryTensor_secondaryTensor_name(x, y, None);
            let mul = |x: &MPSGraphTensor, y: &MPSGraphTensor| {
                g.multiplicationWithPrimaryTensor_secondaryTensor_name(x, y, None)
            };
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
                &ids,
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
            let typ = g.sliceTensor_dimension_start_length_name(
                &*w("embeddings.token_type_embeddings.weight")?,
                0,
                0,
                1,
                None,
            );
            let x = add(&add(&reshape(&word, &[b, l, H]), &pos), &typ);
            let mut x = layer_norm(
                &reshape(&x, &[b * l, H]),
                &*w("embeddings.LayerNorm.weight")?,
                &*w("embeddings.LayerNorm.bias")?,
            );

            // Padded columns get MASKED added to their attention scores: (mask − 1)·30000.
            let bias = mul(&add(&mask, &scalar(-1.0)), &scalar(-MASKED));
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
            let pooled = match self.model {
                EmbeddingModel::BgeSmallEn => g.sliceTensor_dimension_start_length_name(&x, 1, 0, 1, None),
                EmbeddingModel::MultilingualE5Small => {
                    let m = g.castTensor_toType_name(&reshape(&mask, &[b, l, 1]), MPSDataType::Float32, None);
                    let axis = shape(&[1]);
                    let sum = g.reductionSumWithTensor_axes_name(&mul(&x, &m), Some(&axis), None);
                    let kept = g.reductionSumWithTensor_axes_name(&m, Some(&axis), None);
                    g.divisionWithPrimaryTensor_secondaryTensor_name(&sum, &kept, None)
                }
            };
            let out = reshape(&pooled, &[b, H]);

            let weight_feeds = weight_feeds.into_inner();
            let mut keys: Vec<&MPSGraphTensor> = vec![&ids, &mask];
            keys.extend(weight_feeds.iter().map(|(t, _)| &**t));
            let types: Vec<Retained<MPSGraphShapedType>> = keys
                .iter()
                .map(|t| {
                    MPSGraphShapedType::initWithShape_dataType(
                        MPSGraphShapedType::alloc(),
                        t.shape().as_deref(),
                        t.dataType(),
                    )
                })
                .collect();
            let types: Vec<&MPSGraphShapedType> = types.iter().map(|t| &**t).collect();
            let desc = MPSGraphCompilationDescriptor::new();
            desc.setOptimizationLevel(MPSGraphOptimization::Level0);
            let exec = g.compileWithDevice_feeds_targetTensors_targetOperations_compilationDescriptor(
                Some(&self.device),
                &NSDictionary::from_slices(&keys, &types),
                &NSArray::from_slice(&[&*out]),
                None,
                Some(&desc),
            );

            // The executable takes its inputs as an array, in an order of its own choosing.
            let order = exec.feedTensors().context("the compiled encoder lists no inputs")?;
            let mut feeds = Vec::with_capacity(order.count());
            for t in order.iter() {
                feeds.push(if *t == *ids {
                    Feed::Ids
                } else if *t == *mask {
                    Feed::Mask
                } else {
                    let (_, name) = weight_feeds
                        .iter()
                        .find(|(p, _)| **p == *t)
                        .context("the compiled encoder wants an input it was not given")?;
                    Feed::Weight(name.clone())
                });
            }
            Ok(Bucket { exec, feeds })
        }
    }

    /// Compile and run once every full-group bucket, so a run's forwards find them ready. A bucket's
    /// first run costs more than its compile: Metal builds its pipelines then.
    pub fn warm(&mut self) -> Result<()> {
        let lens: Vec<usize> = (1..=MAX_LEN / ROUND).map(|i| i * ROUND).collect();
        for &l in &lens {
            self.bucket(GROUP, l)?;
        }
        for &l in &lens {
            self.forward(&vec![0; GROUP * l], &vec![0.0; GROUP * l], GROUP, l)?;
        }
        Ok(())
    }

    /// One forward over `b` sequences padded to `l`: `ids` and `mask` are [b·l], row-major. Returns
    /// the [b·H] pooled rows.
    fn forward(&mut self, ids: &[i32], mask: &[f32], b: usize, l: usize) -> Result<Vec<f32>> {
        self.bucket(b, l)?;
        let bucket = &self.buckets[&(b, l)];
        autoreleasepool(|_| unsafe {
            let tensor = |bytes: Vec<u8>, dims: &[usize], dtype: MPSDataType| {
                MPSGraphTensorData::initWithDevice_data_shape_dataType(
                    MPSGraphTensorData::alloc(),
                    &self.device,
                    &NSData::from_vec(bytes),
                    &shape(dims),
                    dtype,
                )
            };
            let ids = tensor(
                ids.iter().flat_map(|v| v.to_le_bytes()).collect(),
                &[b * l],
                MPSDataType::Int32,
            );
            let mask = tensor(f16_bytes(mask), &[b, l], MPSDataType::Float16);
            let inputs: Vec<&MPSGraphTensorData> = bucket
                .feeds
                .iter()
                .map(|f| match f {
                    Feed::Ids => &*ids,
                    Feed::Mask => &*mask,
                    Feed::Weight(name) => &*self.weights[name].1,
                })
                .collect();
            let results = bucket
                .exec
                .runWithMTLCommandQueue_inputsArray_resultsArray_executionDescriptor(
                    &self.queue,
                    &NSArray::from_slice(&inputs),
                    None,
                    None,
                );
            let out = results.firstObject().context("the encoder returned nothing")?;
            let mut rows = vec![0f32; b * H];
            out.mpsndarray()
                .readBytes_strideBytes(NonNull::new_unchecked(rows.as_mut_ptr().cast()), std::ptr::null_mut());
            Ok(rows)
        })
    }
}

/// A weight as the graph takes it: a linear's [out, in] transposed to [in, out], so `x·W` needs no
/// transpose per forward; `None` for a tensor the encoder does not use (the pooler).
fn graph_layout(name: &str, vals: Vec<f32>) -> Option<(Vec<usize>, Vec<f32>)> {
    if !(name.starts_with("embeddings.") || name.starts_with("encoder.")) {
        return None;
    }
    let linear = |out: usize, inp: usize| {
        let mut t = vec![0f32; out * inp];
        for r in 0..out {
            for c in 0..inp {
                t[c * out + r] = vals[r * inp + c];
            }
        }
        Some((vec![inp, out], t))
    };
    if name.ends_with("LayerNorm.weight") || name.ends_with("LayerNorm.bias") || name.ends_with(".bias") {
        return Some((vec![vals.len()], vals));
    }
    if name.ends_with("intermediate.dense.weight") {
        return linear(FFN, H);
    }
    if name.ends_with("output.dense.weight") && !name.contains("attention") {
        return linear(H, FFN);
    }
    if name.starts_with("encoder.") {
        return linear(H, H);
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
            let longest = group.iter().map(|&i| encs[i].get_ids().len()).max().unwrap_or(1);
            let l = (longest.div_ceil(ROUND).max(1) * ROUND).min(MAX_LEN);
            // A short last group is padded with empty rows to a power of two, so a run compiles at
            // most four batch sizes per length.
            let b = group.len().next_power_of_two().clamp(MIN_GROUP, GROUP);
            let mut ids = vec![0i32; b * l];
            let mut mask = vec![0f32; b * l];
            for (row, &i) in group.iter().enumerate() {
                for (t, &id) in encs[i].get_ids().iter().enumerate() {
                    ids[row * l + t] = id as i32;
                    mask[row * l + t] = 1.0;
                }
            }
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
        for model in EmbeddingModel::ALL {
            let Ok(mut gpu) = MetalEmbedder::new(model) else {
                eprintln!("no usable GPU; skipping {}", model.id());
                return;
            };
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
