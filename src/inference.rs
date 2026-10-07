//! The inference backend behind funes' two model operations — embedding and reranking. The rest of
//! funes talks to these traits via the [`embedder`]/[`reranker`] factories, never a concrete ML
//! stack, so an alternative backend slots in behind the same interface. The backend is chosen at
//! build time in one place — the `Default*` aliases below: default build → BLAS (a from-scratch
//! forward on Accelerate/faer); `--no-default-features --features onnx` → fastembed/ort.

#[cfg(feature = "blas")]
pub mod blas;

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

    pub fn id(self) -> &'static str {
        match self {
            EmbeddingModel::BgeSmallEn => "BAAI/bge-small-en-v1.5",
            EmbeddingModel::MultilingualE5Small => "intfloat/multilingual-e5-small",
        }
    }

    pub fn from_id(id: &str) -> Option<EmbeddingModel> {
        EmbeddingModel::ALL.into_iter().find(|m| m.id() == id)
    }

    /// e5 was trained with these query and passage prefixes, and its model card asks for them.
    fn prefixes(self) -> (&'static str, &'static str) {
        match self {
            EmbeddingModel::BgeSmallEn => ("", ""),
            EmbeddingModel::MultilingualE5Small => ("query: ", "passage: "),
        }
    }
}

/// Embed each text into a dense vector, in input order.
pub trait Embedder: Send {
    fn model(&self) -> EmbeddingModel;

    /// Encode each text as given, in input order.
    fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

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
    Ok(Box::new(DefaultEmbedder::new(model)?))
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

/// Embed `texts` in batches of [`EMBED_BATCH`], calling `on_batch(embedded_so_far)` after each so a
/// caller can report progress (or pass a no-op).
pub(crate) fn embed_batched(
    embedder: &mut dyn Embedder,
    texts: &[&str],
    mut on_batch: impl FnMut(usize),
) -> Result<Vec<Vec<f32>>> {
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for group in texts.chunks(EMBED_BATCH) {
        vectors.extend(embedder.embed(group)?);
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
