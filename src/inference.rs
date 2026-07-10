//! The inference backend behind funes' two model operations — embedding and reranking. The rest of
//! funes talks to these traits via the [`embedder`]/[`reranker`] factories, never a concrete ML
//! stack, so an alternative backend slots in behind the same interface. The backend is chosen at
//! build time in one place — the `Default*` aliases below: default build → ONNX (fastembed/ort);
//! `--features blas` → a from-scratch forward on the platform BLAS (Accelerate on macOS).

use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, RerankInitOptions, RerankerModel, TextEmbedding, TextRerank};

// The single backend-selection point. One of these alias pairs is compiled; the factories below
// box whichever it names. The ONNX types stay defined in both builds (the backend benchmark uses
// them as its reference), so only these aliases — not the call sites — decide what funes runs.
#[cfg(not(feature = "blas"))]
use self::{OnnxEmbedder as DefaultEmbedder, OnnxReranker as DefaultReranker};
#[cfg(feature = "blas")]
use crate::blas::{BlasEmbedder as DefaultEmbedder, BlasReranker as DefaultReranker};

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
pub struct OnnxEmbedder(TextEmbedding);

impl OnnxEmbedder {
    pub fn new() -> Result<Self> {
        Ok(Self(TextEmbedding::try_new(InitOptions::new(
            EmbeddingModel::BGESmallENV15,
        ))?))
    }
}

impl Embedder for OnnxEmbedder {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.0.embed(texts, None)
    }
}

/// fastembed/ort reranker: BAAI/bge-reranker-base cross-encoder on the ONNX Runtime CPU EP.
pub struct OnnxReranker(TextRerank);

impl OnnxReranker {
    pub fn new() -> Result<Self> {
        Ok(Self(TextRerank::try_new(RerankInitOptions::new(
            RerankerModel::BGERerankerBase,
        ))?))
    }
}

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
