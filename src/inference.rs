//! The inference backend behind funes' two model operations — embedding and reranking. The rest of
//! funes talks to these traits, not to a concrete ML stack, so an alternative backend (e.g. a
//! hand-written Accelerate forward on macOS) can slot in behind the same interface, selected by a
//! build feature, without touching any call site. Today only the ONNX backend (fastembed/ort) is
//! wired.

use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, RerankInitOptions, RerankerModel, TextEmbedding, TextRerank};

/// Embed each text into a dense vector, in input order.
pub trait Embedder: Send {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

/// Score each doc against the query; one score per doc, in input order (higher = more relevant).
pub trait Reranker: Send {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;
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
