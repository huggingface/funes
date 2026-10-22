//! The encoders on the GPU, built as MPSGraphs and run on Metal: the embedder's BERT in [`bert`]
//! and the reranker's ModernBERT in [`modernbert`]. This module holds what both share: the device,
//! the weights resident on it in fp16, the graph compiled per bucket, and a GPU's bring-up and
//! fallback. macOS only: [`super::embedder`] and [`super::reranker`] fall back to the CPU forward
//! wherever the GPU cannot start.
//!
//! A graph is compiled per (batch, padded length) bucket, the first time a forward needs it: inputs
//! are sorted by length and padded only to their group's longest, rounded up to [`ROUND`] so a run
//! compiles a handful of buckets, not one per length. The weights are fed to every bucket from the
//! same GPU buffers rather than baked into each graph as constants, so the buckets share one copy.

mod bert;
mod modernbert;

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;

use anyhow::{anyhow, bail, Context, Result};
use objc2::exception::{catch, Exception};
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
use tokenizers::Encoding;

use super::MAX_LEN;

pub use self::bert::{Hybrid, MetalEmbedder};
pub use self::modernbert::{HybridReranker, MetalReranker};

/// The CPU forward's layer-norm epsilon, so both backends normalize alike. (The models' configs say
/// 1e-12, which fp16 cannot hold.)
const EPS: f32 = 1e-5;
/// The smallest batch a forward pads a short group to.
const MIN_GROUP: usize = 8;
/// Padded lengths are rounded up to a multiple of this, which bounds the buckets a run compiles at
/// [`MAX_LEN`]/ROUND per group size. 16 compiles twice the buckets for no gain; 64 pads too much.
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

/// What an encoder keeps on the GPU: the device and its queue, its weights resident in fp16, and
/// the graph compiled for each (batch, padded length) bucket.
struct Graphs {
    device: Retained<MPSGraphDevice>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// Each weight's shape in the graph and its data, resident on the GPU.
    weights: HashMap<String, (Vec<usize>, Retained<MPSGraphTensorData>)>,
    buckets: HashMap<(usize, usize), Bucket>,
}

/// What a graph builder builds an encoder from, for `b` sequences of `l` tokens.
struct Inputs<'a> {
    g: &'a MPSGraph,
    /// Token ids, [b·l] int32.
    ids: &'a MPSGraphTensor,
    /// 1 for a real token and 0 for padding, [b, l] fp16.
    mask: &'a MPSGraphTensor,
    /// A weight by name, as the fp16 placeholder its resident data is fed to.
    w: &'a dyn Fn(&str) -> Result<Retained<MPSGraphTensor>>,
}

// SAFETY: the Metal device and command queue are thread-safe; the graphs, executables and tensor
// data are only ever touched through `&mut self`, so moving an encoder to another thread never
// shares them.
unsafe impl Send for Graphs {}

/// The Metal device, where its MPSGraph has fused attention (macOS 15); otherwise what is missing.
fn device() -> Result<Retained<ProtocolObject<dyn MTLDevice>>> {
    let mtl = MTLCreateSystemDefaultDevice().context("no Metal device")?;
    let attention = sel!(scaledDotProductAttentionWithQueryTensor:keyTensor:valueTensor:maskTensor:scale:name:);
    if !unsafe { MPSGraph::new() }.respondsToSelector(attention) {
        bail!("this MPSGraph has no fused attention (macOS 15 or later)");
    }
    Ok(mtl)
}

impl Graphs {
    /// Fails where there is no usable [`device`].
    fn new() -> Result<Self> {
        let mtl = device()?;
        let queue = mtl.newCommandQueue().context("no Metal command queue")?;
        let device = unsafe { MPSGraphDevice::deviceWithMTLDevice(&mtl) };
        Ok(Self {
            device,
            queue,
            weights: HashMap::new(),
            buckets: HashMap::new(),
        })
    }

    /// Put `vals` on the GPU in fp16, as the weight `name` the graphs take in shape `dims`.
    fn upload(&mut self, name: String, dims: Vec<usize>, vals: &[f32]) {
        let data = autoreleasepool(|_| unsafe {
            MPSGraphTensorData::initWithDevice_data_shape_dataType(
                MPSGraphTensorData::alloc(),
                &self.device,
                &NSData::from_vec(f16_bytes(vals)),
                &shape(&dims),
                MPSDataType::Float16,
            )
        });
        self.weights.insert(name, (dims, data));
    }

    /// One forward over `b` sequences padded to `l`: `ids` and `mask` are [b·l], row-major. Runs the
    /// graph `build` makes, compiled the first time the bucket is asked for, and returns the `out`
    /// f32 values of its output.
    fn run(
        &mut self,
        b: usize,
        l: usize,
        ids: &[i32],
        mask: &[f32],
        out: usize,
        build: impl FnOnce(Inputs<'_>) -> Result<Retained<MPSGraphTensor>>,
    ) -> Result<Vec<f32>> {
        if !self.buckets.contains_key(&(b, l)) {
            let bucket =
                catch(AssertUnwindSafe(|| autoreleasepool(|_| self.compile(b, l, build)))).map_err(raised)??;
            self.buckets.insert((b, l), bucket);
        }
        let bucket = &self.buckets[&(b, l)];
        let run = || {
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
                let first = results.firstObject().context("the encoder returned nothing")?;
                let mut rows = vec![0f32; out];
                first
                    .mpsndarray()
                    .readBytes_strideBytes(NonNull::new_unchecked(rows.as_mut_ptr().cast()), std::ptr::null_mut());
                Ok(rows)
            })
        };
        catch(AssertUnwindSafe(run)).map_err(raised)?
    }

    /// Build the graph for `b` sequences of `l` tokens and compile it. Optimization level 0: level
    /// 1 also tries placing ops on the Neural Engine, which costs seconds of compile per bucket and
    /// runs no faster.
    fn compile(
        &self,
        b: usize,
        l: usize,
        build: impl FnOnce(Inputs<'_>) -> Result<Retained<MPSGraphTensor>>,
    ) -> Result<Bucket> {
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
            let out = build(Inputs {
                g: &g,
                ids: &ids,
                mask: &mask,
                w: &w,
            })?;

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
}

/// An Objective-C exception as the error it stands for. MPSGraph raises one for a shape or an op
/// the device rejects, and uncaught it would abort the process.
fn raised(e: Option<Retained<Exception>>) -> anyhow::Error {
    match e {
        Some(e) => anyhow!("MPSGraph raised: {e}"),
        None => anyhow!("MPSGraph raised an exception"),
    }
}

/// The sequences of `group`, length-sorted, laid out for one forward as (b, l, ids, mask), ids and
/// mask [b·l]. A short group is padded with empty rows to a power of two, at least [`MIN_GROUP`], so
/// a run compiles a graph per power of two rather than per group size, and every row to the group's
/// longest rounded up to [`ROUND`].
fn pad_group(encs: &[Encoding], group: &[usize]) -> (usize, usize, Vec<i32>, Vec<f32>) {
    let longest = group.iter().map(|&i| encs[i].get_ids().len()).max().unwrap_or(1);
    let l = (longest.div_ceil(ROUND).max(1) * ROUND).min(MAX_LEN);
    let b = group.len().next_power_of_two().max(MIN_GROUP);
    let mut ids = vec![0i32; b * l];
    let mut mask = vec![0f32; b * l];
    for (row, &i) in group.iter().enumerate() {
        for (t, &id) in encs[i].get_ids().iter().enumerate() {
            ids[row * l + t] = id as i32;
            mask[row * l + t] = 1.0;
        }
    }
    (b, l, ids, mask)
}

/// A linear's weight [out, in] transposed to [in, out], so the graph's `x·W` needs no transpose per
/// forward.
fn transposed(vals: &[f32], out: usize, inp: usize) -> (Vec<usize>, Vec<f32>) {
    let mut t = vec![0f32; out * inp];
    for r in 0..out {
        for c in 0..inp {
            t[c * out + r] = vals[r * inp + c];
        }
    }
    (vec![inp, out], t)
}

/// A GPU encoder, brought up on its first use. One that cannot start, or fails a call, is not tried
/// again.
enum Gpu<T> {
    Untried,
    Ready(Box<T>),
    Unavailable,
}

impl<T> Gpu<T> {
    /// The encoder, started with `start` if it was never tried. `what` names the work in the note
    /// a failed start leaves.
    fn get(&mut self, what: &str, start: impl FnOnce() -> Result<T>) -> Option<&mut T> {
        if let Gpu::Untried = self {
            *self = match start() {
                Ok(gpu) => Gpu::Ready(Box::new(gpu)),
                Err(e) => {
                    eprintln!("note: {what} on the CPU — the GPU is unavailable: {e:#}");
                    Gpu::Unavailable
                }
            };
        }
        match self {
            Gpu::Ready(gpu) => Some(gpu),
            _ => None,
        }
    }

    /// Stop using the GPU after it failed a call.
    fn give_up(&mut self, what: &str, e: anyhow::Error) {
        eprintln!("note: {what} on the CPU — the GPU failed: {e:#}");
        *self = Gpu::Unavailable;
    }
}
