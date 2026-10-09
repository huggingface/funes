//! The inference backend behind funes' two model operations — embedding and reranking. The rest of
//! funes talks to these traits via the [`embedder`]/[`reranker`] factories, never a concrete ML
//! stack, so an alternative backend slots in behind the same interface. The backend is chosen at
//! build time in one place — the `Default*` aliases below: default build → BLAS (a from-scratch
//! forward on Accelerate/faer); `--no-default-features --features onnx` → fastembed/ort. On macOS
//! the default build also carries `metal`, the same encoder on the GPU, which [`embedder`] hands
//! the bulk calls to (see [`Hybrid`]).

#[cfg(feature = "blas")]
pub mod blas;
#[cfg(all(feature = "metal", target_os = "macos"))]
pub mod metal;

use anyhow::{Context, Result};

// The single backend-selection point. One of these alias pairs is compiled; the factories below
// box whichever it names. When both backends are compiled in (the backend benchmark builds that
// way to use ONNX as its reference), BLAS is the one funes runs.
#[cfg(all(feature = "onnx", not(feature = "blas")))]
use self::{OnnxEmbedder as DefaultEmbedder, OnnxReranker as DefaultReranker};
#[cfg(feature = "blas")]
use blas::{BlasEmbedder as DefaultEmbedder, BlasReranker as DefaultReranker};
#[cfg(not(any(feature = "blas", feature = "onnx")))]
compile_error!("funes needs an inference backend: feature `blas` (default) or `onnx`");

/// How many texts one `embed` call takes.
const EMBED_BATCH: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EmbeddingModel {
    BgeSmallEn,
    MultilingualE5Small,
}

impl EmbeddingModel {
    pub const ALL: [EmbeddingModel; 2] = [EmbeddingModel::BgeSmallEn, EmbeddingModel::MultilingualE5Small];

    pub const fn id(self) -> &'static str {
        match self {
            EmbeddingModel::BgeSmallEn => "BAAI/bge-small-en-v1.5",
            EmbeddingModel::MultilingualE5Small => "intfloat/multilingual-e5-small",
        }
    }

    pub const fn from_id(id: &str) -> Option<EmbeddingModel> {
        let mut i = 0;
        while i < EmbeddingModel::ALL.len() {
            if same_bytes(EmbeddingModel::ALL[i].id().as_bytes(), id.as_bytes()) {
                return Some(EmbeddingModel::ALL[i]);
            }
            i += 1;
        }
        None
    }

    /// e5 was trained with these query and passage prefixes, and its model card asks for them.
    fn prefixes(self) -> (&'static str, &'static str) {
        match self {
            EmbeddingModel::BgeSmallEn => ("", ""),
            EmbeddingModel::MultilingualE5Small => ("query: ", "passage: "),
        }
    }
}

const fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Embed each text into a dense vector, in input order.
pub trait Embedder: Send {
    fn model(&self) -> EmbeddingModel;

    /// Encode each text as given, in input order.
    fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// How many texts a call should carry. A caller reports progress and checks its budget between
    /// calls, so this is about a few seconds' work; a GPU wants many texts per call to stay busy.
    fn batch_size(&self) -> usize {
        EMBED_BATCH
    }

    /// Bring up whatever the next calls will run on, ahead of them.
    fn warm_up(&mut self) {}

    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let prefix = self.model().prefixes().1;
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        self.encode(&prefixed.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        let prefixed = format!("{}{query}", self.model().prefixes().0);
        self.encode(&[prefixed.as_str()])?.pop().context("empty embedding")
    }
}

/// Score each doc against the query; one score per doc, in input order (higher = more relevant).
pub trait Reranker: Send {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;
}

/// Build the embedder for `model` on the compiled-in backend. Call sites use this instead of
/// naming a concrete type, so the backend is decided only by the `Default*` alias above.
pub fn embedder(model: EmbeddingModel) -> Result<Box<dyn Embedder>> {
    #[cfg(all(feature = "metal", target_os = "macos"))]
    return Ok(Box::new(Hybrid::new(model)?));
    #[allow(unreachable_code)]
    Ok(Box::new(DefaultEmbedder::new(model)?))
}

/// Texts per call once the GPU is in play: a couple of seconds' work for it.
#[cfg(all(feature = "metal", target_os = "macos"))]
const GPU_BATCH: usize = 2048;

/// Calls of at least this many texts bring the GPU up. Below it the CPU forward finishes before the
/// GPU would have its weights uploaded and its first graph compiled; once it is up, every call runs
/// there.
#[cfg(all(feature = "metal", target_os = "macos"))]
const GPU_MIN_TEXTS: usize = 32;

/// The CPU forward for small calls — a recall query, the few chunks of one turn — and the GPU for
/// bulk ones, each brought up on its first call. A GPU that cannot start, or fails a call, is given
/// up on: that call and every one after run on the CPU.
#[cfg(all(feature = "metal", target_os = "macos"))]
struct Hybrid {
    model: EmbeddingModel,
    cpu: Option<DefaultEmbedder>,
    gpu: Gpu,
}

#[cfg(all(feature = "metal", target_os = "macos"))]
enum Gpu {
    Untried,
    Ready(Box<metal::MetalEmbedder>),
    Unavailable,
}

#[cfg(all(feature = "metal", target_os = "macos"))]
impl Hybrid {
    fn new(model: EmbeddingModel) -> Result<Self> {
        // Fetch the weights now, so a failed download fails here, as an eager backend's would.
        blas::hf_snapshot(model.id())?;
        Ok(Self {
            model,
            cpu: None,
            gpu: Gpu::Untried,
        })
    }

    fn gpu(&mut self) -> Option<&mut metal::MetalEmbedder> {
        if let Gpu::Untried = self.gpu {
            self.gpu = match metal::MetalEmbedder::new(self.model) {
                Ok(gpu) => Gpu::Ready(Box::new(gpu)),
                Err(e) => {
                    eprintln!("note: embedding on the CPU — the GPU is unavailable: {e:#}");
                    Gpu::Unavailable
                }
            };
        }
        match &mut self.gpu {
            Gpu::Ready(gpu) => Some(gpu),
            _ => None,
        }
    }

    fn give_up_gpu(&mut self, e: anyhow::Error) {
        eprintln!("note: embedding on the CPU — the GPU failed: {e:#}");
        self.gpu = Gpu::Unavailable;
    }

    fn cpu(&mut self) -> Result<&mut DefaultEmbedder> {
        if self.cpu.is_none() {
            self.cpu = Some(DefaultEmbedder::new(self.model)?);
        }
        Ok(self.cpu.as_mut().expect("just set"))
    }
}

#[cfg(all(feature = "metal", target_os = "macos"))]
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
            if let Some(gpu) = self.gpu() {
                match gpu.encode(texts) {
                    Ok(vectors) => return Ok(vectors),
                    Err(e) => self.give_up_gpu(e),
                }
            }
        }
        self.cpu()?.encode(texts)
    }

    fn warm_up(&mut self) {
        if let Some(gpu) = self.gpu() {
            if let Err(e) = gpu.warm() {
                self.give_up_gpu(e);
            }
        }
    }

    /// The GPU's batch only once it is up: until a call has proved it, callers size their work for
    /// the CPU.
    fn batch_size(&self) -> usize {
        match self.gpu {
            Gpu::Ready(_) => GPU_BATCH,
            Gpu::Untried | Gpu::Unavailable => EMBED_BATCH,
        }
    }
}

/// Build the reranker for the compiled-in backend. See [`embedder`].
pub fn reranker() -> Result<Box<dyn Reranker>> {
    Ok(Box::new(DefaultReranker::new()?))
}

/// fastembed/ort embedder on the ONNX Runtime CPU EP.
#[cfg(feature = "onnx")]
pub struct OnnxEmbedder {
    model: EmbeddingModel,
    inner: fastembed::TextEmbedding,
}

#[cfg(feature = "onnx")]
impl OnnxEmbedder {
    pub fn new(model: EmbeddingModel) -> Result<Self> {
        use fastembed::{InitOptions, TextEmbedding};
        let fastembed_model = match model {
            EmbeddingModel::BgeSmallEn => fastembed::EmbeddingModel::BGESmallENV15,
            EmbeddingModel::MultilingualE5Small => fastembed::EmbeddingModel::MultilingualE5Small,
        };
        Ok(Self {
            model,
            inner: TextEmbedding::try_new(InitOptions::new(fastembed_model))?,
        })
    }
}

#[cfg(feature = "onnx")]
impl Embedder for OnnxEmbedder {
    fn model(&self) -> EmbeddingModel {
        self.model
    }

    fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.inner.embed(texts, None)
    }
}

/// fastembed/ort reranker: BAAI/bge-reranker-base cross-encoder on the ONNX Runtime CPU EP.
#[cfg(feature = "onnx")]
pub struct OnnxReranker(fastembed::TextRerank);

#[cfg(feature = "onnx")]
impl OnnxReranker {
    pub fn new() -> Result<Self> {
        use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
        Ok(Self(TextRerank::try_new(RerankInitOptions::new(
            RerankerModel::BGERerankerBase,
        ))?))
    }
}

#[cfg(feature = "onnx")]
impl Reranker for OnnxReranker {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        // fastembed returns results carrying the original index; project back to input order.
        let mut scores = vec![0f32; docs.len()];
        for r in self.0.rerank(query, docs, false, None)? {
            scores[r.index] = r.score;
        }
        Ok(scores)
    }
}

/// Embed `texts` in batches of the embedder's [`Embedder::batch_size`], calling
/// `on_batch(embedded_so_far)` after each so a caller can report progress (or pass a no-op).
pub(crate) fn embed_batched(
    embedder: &mut dyn Embedder,
    texts: &[&str],
    mut on_batch: impl FnMut(usize),
) -> Result<Vec<Vec<f32>>> {
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    // Asked per call: the first call may bring up a faster backend that wants larger ones.
    while vectors.len() < texts.len() {
        let end = texts.len().min(vectors.len() + embedder.batch_size().max(1));
        let batch = embedder.embed(&texts[vectors.len()..end])?;
        anyhow::ensure!(
            batch.len() == end - vectors.len(),
            "the embedder returned {} vector(s) for {} text(s)",
            batch.len(),
            end - vectors.len()
        );
        vectors.extend(batch);
        on_batch(vectors.len());
    }
    Ok(vectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorder {
        model: EmbeddingModel,
        seen: Vec<String>,
    }

    impl Embedder for Recorder {
        fn model(&self) -> EmbeddingModel {
            self.model
        }

        fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.seen.extend(texts.iter().map(|t| t.to_string()));
            Ok(vec![vec![0.0]; texts.len()])
        }
    }

    #[test]
    fn a_model_is_found_by_its_id_and_only_by_it() {
        for m in EmbeddingModel::ALL {
            assert_eq!(EmbeddingModel::from_id(m.id()), Some(m));
        }
        assert_eq!(EmbeddingModel::from_id("BAAI/bge-small-zh-v1.5"), None);
    }

    #[test]
    fn e5_encodes_prefixed_queries_and_passages() {
        let mut e = Recorder {
            model: EmbeddingModel::MultilingualE5Small,
            seen: Vec::new(),
        };
        e.embed(&["一个段落"]).unwrap();
        e.embed_query("一个问题").unwrap();
        assert_eq!(e.seen, ["passage: 一个段落", "query: 一个问题"]);
    }

    #[test]
    fn bge_encodes_queries_and_passages_as_given() {
        let mut e = Recorder {
            model: EmbeddingModel::BgeSmallEn,
            seen: Vec::new(),
        };
        e.embed(&["a passage"]).unwrap();
        e.embed_query("a question").unwrap();
        assert_eq!(e.seen, ["a passage", "a question"]);
    }
}
