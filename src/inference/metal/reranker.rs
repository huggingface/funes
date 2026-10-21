//! The reranker on the GPU: the encoder of [`crate::inference::blas::modernbert`] as an MPSGraph.
//! Matmuls and attention run in fp16. The residual stream and its norms stay in fp32: a pre-norm
//! stream grows over the 22 layers, and fp16 would round away what each block adds to it. The graph
//! ends at mean pooling, and the classifier head runs on the CPU, in fp32.

use std::collections::HashMap;
use std::f64::consts::FRAC_1_SQRT_2;

use anyhow::Result;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData};
use objc2_metal_performance_shaders::MPSDataType;
use objc2_metal_performance_shaders_graph::MPSGraphTensor;
use tokenizers::Tokenizer;

use super::{f16_bytes, pad_group, shape, transposed, Gpu, Graphs, Inputs, EPS, GROUP, MASKED};
use crate::inference::blas::modernbert::{
    encode_pairs, global, head, keys, GteReranker, Rope, FFN, H, HD, HEAD, HEADS, LAYERS, REPO,
};
use crate::inference::blas::{hf_snapshot, load_tokenizer, load_weights};
use crate::inference::Reranker;

/// gte-reranker-modernbert-base on the GPU: its encoder and mean pool as a graph, then [`head`].
pub struct MetalReranker {
    tok: Tokenizer,
    graphs: Graphs,
    /// The [`HEAD`] weights, kept in f32 for the CPU.
    head: HashMap<String, Vec<f32>>,
}

impl MetalReranker {
    /// Fails where there is no usable GPU, or the weights cannot be loaded.
    pub fn new() -> Result<Self> {
        let mut graphs = Graphs::new()?;
        let dir = hf_snapshot(REPO)?;
        let tok = load_tokenizer(&dir)?;
        let mut head = HashMap::new();
        for (name, vals) in load_weights(&dir)? {
            if HEAD.contains(&name.as_str()) {
                head.insert(name, vals);
            } else {
                let (dims, vals) = graph_layout(&name, vals);
                graphs.upload(name, dims, &vals);
            }
        }
        Ok(Self { tok, graphs, head })
    }
}

/// A weight as the graph takes it: a linear's [out, in] transposed to [in, out], the embedding
/// table as [rows, H], a norm's weight as it is.
fn graph_layout(name: &str, vals: Vec<f32>) -> (Vec<usize>, Vec<f32>) {
    if name.ends_with("attn.Wqkv.weight") {
        transposed(&vals, 3 * H, H)
    } else if name.ends_with("attn.Wo.weight") {
        transposed(&vals, H, H)
    } else if name.ends_with("mlp.Wi.weight") {
        transposed(&vals, 2 * FFN, H)
    } else if name.ends_with("mlp.Wo.weight") {
        transposed(&vals, H, FFN)
    } else if name.ends_with("tok_embeddings.weight") {
        (vec![vals.len() / H, H], vals)
    } else {
        (vec![vals.len()], vals)
    }
}

impl Reranker for MetalReranker {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        let encs = encode_pairs(&self.tok, query, docs)?;
        let mut order: Vec<usize> = (0..encs.len()).collect();
        order.sort_by_key(|&i| encs[i].get_ids().len());
        let mut scores = vec![0f32; encs.len()];
        for group in order.chunks(GROUP) {
            let (b, l, ids, mask) = pad_group(&encs, group);
            let pooled = self.graphs.run(b, l, &ids, &mask, b * H, |x| modernbert(x, b, l))?;
            let logits = head(&self.head, &pooled[..group.len() * H], group.len());
            for (&i, s) in group.iter().zip(logits) {
                scores[i] = s;
            }
        }
        Ok(scores)
    }
}

/// gte's encoder and mean pool for `b` pairs of `l` tokens: [b, H] in fp32.
fn modernbert(x: Inputs<'_>, b: usize, l: usize) -> Result<Retained<MPSGraphTensor>> {
    let Inputs { g, ids, mask, w } = x;
    let (f16, f32) = (MPSDataType::Float16, MPSDataType::Float32);
    unsafe {
        let add = |x: &MPSGraphTensor, y: &MPSGraphTensor| g.additionWithPrimaryTensor_secondaryTensor_name(x, y, None);
        let mul =
            |x: &MPSGraphTensor, y: &MPSGraphTensor| g.multiplicationWithPrimaryTensor_secondaryTensor_name(x, y, None);
        let matmul = |x: &MPSGraphTensor, y: &MPSGraphTensor| {
            g.matrixMultiplicationWithPrimaryTensor_secondaryTensor_name(x, y, None)
        };
        let reshape = |x: &MPSGraphTensor, dims: &[usize]| g.reshapeTensor_withShape_name(x, &shape(dims), None);
        let cast = |x: &MPSGraphTensor, to: MPSDataType| g.castTensor_toType_name(x, to, None);
        let slice = |x: &MPSGraphTensor, dim: usize, start: usize, len: usize| {
            g.sliceTensor_dimension_start_length_name(x, dim, start as isize, len as isize, None)
        };
        let scalar = |v: f64, t: MPSDataType| g.constantWithScalar_dataType(v, t);
        let constant = |bytes: Vec<u8>, dims: &[usize], t: MPSDataType| {
            g.constantWithData_shape_dataType(&NSData::from_vec(bytes), &shape(dims), t)
        };
        let last = shape(&[1]);
        // LayerNorm without a bias, in fp32 like the stream it normalizes.
        let norm = |x: &MPSGraphTensor, name: &str| -> Result<Retained<MPSGraphTensor>> {
            let gamma = cast(&*w(name)?, f32);
            let mean = g.meanOfTensor_axes_name(x, &last, None);
            let var = g.varianceOfTensor_meanTensor_axes_name(x, &mean, &last, None);
            Ok(
                g.normalizationWithTensor_meanTensor_varianceTensor_gammaTensor_betaTensor_epsilon_name(
                    x,
                    &mean,
                    &var,
                    Some(&gamma),
                    None,
                    EPS,
                    None,
                ),
            )
        };

        let emb = g.gatherWithUpdatesTensor_indicesTensor_axis_batchDimensions_name(
            &*w("model.embeddings.tok_embeddings.weight")?,
            ids,
            0,
            0,
            None,
        );
        let mut x = norm(&cast(&emb, f32), "model.embeddings.norm.weight")?;

        // Padded keys get MASKED added to their scores, (mask − 1)·30000. A window layer adds it again
        // to the keys the CPU forward's `keys` leaves out, which stays inside fp16's range.
        let pad = reshape(
            &mul(&add(mask, &scalar(-1.0, f16)), &scalar(-MASKED, f16)),
            &[b, 1, 1, l],
        );
        let band: Vec<f32> = (0..l)
            .flat_map(|i| {
                let seen = keys(i, l, true);
                (0..l).map(move |j| if seen.contains(&j) { 0.0 } else { MASKED as f32 })
            })
            .collect();
        let windowed = add(&pad, &constant(f16_bytes(&band), &[1, 1, l, l], f16));

        // Each position's angles across a head, [l, HD]: the half-head's, twice, as transformers
        // lays them out.
        let angles = |rope: &Rope| {
            let half = HD / 2;
            let table = |t: &[f32]| -> Vec<u8> {
                (0..l)
                    .flat_map(|p| {
                        let row = &t[p * half..][..half];
                        row.iter().chain(row)
                    })
                    .flat_map(|v| v.to_le_bytes())
                    .collect()
            };
            (
                constant(table(&rope.cos), &[l, HD], f32),
                constant(table(&rope.sin), &[l, HD], f32),
            )
        };
        let (full_rope, window_rope) = (angles(&Rope::global()), angles(&Rope::local()));
        // q or k [b, HEADS, l, HD], rotated in fp32 as transformers rotates them, back in fp16.
        let rotate = |t: &MPSGraphTensor, (cos, sin): &(Retained<MPSGraphTensor>, Retained<MPSGraphTensor>)| {
            let t = cast(t, f32);
            let (first, second) = (slice(&t, 3, 0, HD / 2), slice(&t, 3, HD / 2, HD / 2));
            let negated = g.negativeWithTensor_name(&second, None);
            let turned = g.concatTensors_dimension_name(&NSArray::from_slice(&[&*negated, &*first]), 3, None);
            cast(&add(&mul(&t, cos), &mul(&turned, sin)), f16)
        };

        for ly in 0..LAYERS {
            let p = format!("model.layers.{ly}");
            let linear = |x: &MPSGraphTensor, n: &str| -> Result<Retained<MPSGraphTensor>> {
                Ok(matmul(x, &*w(&format!("{p}.{n}.weight"))?))
            };
            // Layer 0 has no norm before attention: the embeddings' norm stands in for it.
            let h = if ly == 0 {
                cast(&x, f16)
            } else {
                cast(&*norm(&x, &format!("{p}.attn_norm.weight"))?, f16)
            };
            let qkv = linear(&h, "attn.Wqkv")?;
            let heads = |i: usize| {
                let part = reshape(&slice(&qkv, 1, i * H, H), &[b, l, HEADS, HD]);
                g.transposeTensor_permutation_name(&part, &shape(&[0, 2, 1, 3]), None)
            };
            let (rope, bias) = if global(ly) {
                (&full_rope, &pad)
            } else {
                (&window_rope, &windowed)
            };
            let (q, k, v) = (rotate(&heads(0), rope), rotate(&heads(1), rope), heads(2));
            let ctx = g.scaledDotProductAttentionWithQueryTensor_keyTensor_valueTensor_maskTensor_scale_name(
                &q,
                &k,
                &v,
                Some(&**bias),
                1.0 / (HD as f32).sqrt(),
                None,
            );
            let ctx = reshape(
                &g.transposeTensor_permutation_name(&ctx, &shape(&[0, 2, 1, 3]), None),
                &[b * l, H],
            );
            x = add(&x, &cast(&*linear(&ctx, "attn.Wo")?, f32));

            let h = cast(&*norm(&x, &format!("{p}.mlp_norm.weight"))?, f16);
            let inter = linear(&h, "mlp.Wi")?;
            let (input, gate) = (slice(&inter, 1, 0, FFN), slice(&inter, 1, FFN, FFN));
            // GeGLU: the exact GELU of the first half, 0.5·x·(1 + erf(x/√2)), times the second.
            let erf = g.erfWithTensor_name(&mul(&input, &scalar(FRAC_1_SQRT_2, f16)), None);
            let act = mul(
                &mul(&mul(&input, &add(&erf, &scalar(1.0, f16))), &scalar(0.5, f16)),
                &gate,
            );
            x = add(&x, &cast(&*linear(&act, "mlp.Wo")?, f32));
        }
        let x = reshape(&*norm(&x, "model.final_norm.weight")?, &[b, l, H]);

        // The mean of the real tokens' rows.
        let m = cast(&reshape(mask, &[b, l, 1]), f32);
        let axis = shape(&[1]);
        let sum = g.reductionSumWithTensor_axes_name(&mul(&x, &m), Some(&axis), None);
        let kept = g.reductionSumWithTensor_axes_name(&m, Some(&axis), None);
        Ok(reshape(
            &g.divisionWithPrimaryTensor_secondaryTensor_name(&sum, &kept, None),
            &[b, H],
        ))
    }
}

/// Calls of at least this many pairs bring the GPU up; once it is up, every call runs there.
const GPU_MIN_PAIRS: usize = 8;

/// The CPU forward for small calls and the GPU for the rest, each brought up on its first call. A
/// GPU that cannot start, or fails a call, is given up on: that call and every one after run on the
/// CPU.
pub struct HybridReranker {
    cpu: Option<GteReranker>,
    gpu: Gpu<MetalReranker>,
}

impl HybridReranker {
    pub fn new() -> Result<Self> {
        // Fetch the weights now, so a failed download fails here, as an eager backend's would.
        hf_snapshot(REPO)?;
        Ok(Self {
            cpu: None,
            gpu: Gpu::Untried,
        })
    }

    fn cpu(&mut self) -> Result<&mut GteReranker> {
        if self.cpu.is_none() {
            self.cpu = Some(GteReranker::new()?);
        }
        Ok(self.cpu.as_mut().expect("just set"))
    }
}

impl Reranker for HybridReranker {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        let on_gpu = match self.gpu {
            Gpu::Ready(_) => true,
            Gpu::Untried => docs.len() >= GPU_MIN_PAIRS,
            Gpu::Unavailable => false,
        };
        if on_gpu {
            if let Some(gpu) = self.gpu.get("reranking", MetalReranker::new) {
                match gpu.rerank(query, docs) {
                    Ok(scores) => return Ok(scores),
                    Err(e) => self.gpu.give_up("reranking", e),
                }
            }
        }
        self.cpu()?.rerank(query, docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::blas::modernbert::reference;

    /// The GPU scores what transformers does, up to fp16: the card's pairs one query at a time, and
    /// the long pairs in one call, padded to a bucket.
    #[test]
    fn gpu_logits_agree_with_transformers() {
        if let Err(e) = super::super::device() {
            eprintln!("skipping: {e:#}");
            return;
        }
        let mut gpu = MetalReranker::new().expect("gte weights");
        let mut got = Vec::new();
        let mut want = Vec::new();
        for (q, p, logit) in reference::CARD {
            got.extend(gpu.rerank(q, &[p]).unwrap());
            want.push(logit);
        }
        let long = reference::long();
        let docs: Vec<&str> = long.iter().map(|(p, _)| p.as_str()).collect();
        got.extend(gpu.rerank(reference::QUERY, &docs).unwrap());
        want.extend(long.iter().map(|(_, logit)| *logit));
        let worst = got.iter().zip(&want).map(|(g, w)| (g - w).abs()).fold(0f32, f32::max);
        eprintln!("largest logit difference to transformers: {worst:.2e}");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert!((g - w).abs() < 2e-2, "pair {i}: {g} against transformers' {w}");
        }
    }
}
