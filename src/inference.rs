//! The inference backend behind funes' two model operations — embedding and reranking. The rest of
//! funes talks to these traits via the [`embedder`]/[`reranker`] factories, never a concrete ML
//! stack, so an alternative backend slots in behind the same interface. The backend is chosen at
//! build time in one place — the `Default*` aliases below: default build → BLAS (a from-scratch
//! forward on Accelerate/faer); `--no-default-features --features onnx` → fastembed/ort.

use anyhow::Result;

// The single backend-selection point. One of these alias pairs is compiled; the factories below
// box whichever it names. When both backends are compiled in (the backend benchmark builds that
// way to use ONNX as its reference), BLAS is the one funes runs.
#[cfg(all(feature = "onnx", not(feature = "blas")))]
use self::{OnnxEmbedder as DefaultEmbedder, OnnxReranker as DefaultReranker};
#[cfg(feature = "blas")]
use crate::blas::{BlasEmbedder as DefaultEmbedder, BlasReranker as DefaultReranker};
#[cfg(not(any(feature = "blas", feature = "onnx")))]
compile_error!("funes needs an inference backend: feature `blas` (default) or `onnx`");

/// Embed each text into a dense vector, in input order.
pub trait Embedder: Send {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

/// Score each doc against the query; one score per doc, in input order (higher = more relevant).
pub trait Reranker: Send {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;
}

/// Build the embedder for the compiled-in backend. Call sites use this instead of naming a
/// concrete type, so the backend is decided only by the `Default*` alias above.
pub fn embedder() -> Result<Box<dyn Embedder>> {
    Ok(Box::new(DefaultEmbedder::new()?))
}

/// Build the reranker for the compiled-in backend. See [`embedder`].
pub fn reranker() -> Result<Box<dyn Reranker>> {
    Ok(Box::new(DefaultReranker::new()?))
}

/// fastembed/ort embedder: BAAI/bge-small-en-v1.5 on the ONNX Runtime CPU EP.
#[cfg(feature = "onnx")]
pub struct OnnxEmbedder(fastembed::TextEmbedding);

#[cfg(feature = "onnx")]
impl OnnxEmbedder {
    pub fn new() -> Result<Self> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
        Ok(Self(TextEmbedding::try_new(InitOptions::new(
            EmbeddingModel::BGESmallENV15,
        ))?))
    }
}

#[cfg(feature = "onnx")]
impl Embedder for OnnxEmbedder {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.0.embed(texts, None)
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
