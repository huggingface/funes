//! Score funes's inference backends against ONNX Runtime running each model's ONNX export: how
//! closely the CPU forwards (`blas`) and, in a macOS build, the GPU forwards (`metal`) reproduce its
//! embeddings and its reranker scores, over the workloads `bench_models` times. ONNX is the
//! reference, so the example needs it compiled in:
//!
//! ```text
//! cargo run --release --features onnx --example score_models
//! ```
//!
//! `cos↔onnx` is the lowest cosine similarity between a document's embedding on the backend and on
//! ONNX, and `Δ↔onnx` the largest absolute difference between their reranker scores.

#[path = "workloads.rs"]
mod workloads;

use anyhow::{anyhow, Result};
use clap::Parser;
use funes::inference::{Embedder, EmbeddingModel, OnnxEmbedder, OnnxReranker, Reranker};

#[derive(Parser)]
struct Args {
    /// The embedding model, by its Hub id. `BAAI/bge-small-en-v1.5` when omitted.
    #[arg(long)]
    model: Option<String>,
}

/// A backend scored against ONNX: its name, its embedder and its reranker.
type Backend = (&'static str, Box<dyn Embedder>, Box<dyn Reranker>);

/// The backends compiled in besides ONNX.
#[cfg(feature = "blas")]
fn backends(model: EmbeddingModel) -> Result<Vec<Backend>> {
    Ok(vec![
        (
            "blas",
            Box::new(funes::inference::blas::BlasEmbedder::new(model)?) as Box<dyn Embedder>,
            Box::new(funes::inference::blas::GteReranker::new()?) as Box<dyn Reranker>,
        ),
        #[cfg(all(feature = "metal", target_os = "macos"))]
        (
            "metal",
            Box::new(funes::inference::metal::MetalEmbedder::new(model)?) as Box<dyn Embedder>,
            Box::new(funes::inference::metal::MetalReranker::new()?) as Box<dyn Reranker>,
        ),
    ])
}

/// A build with ONNX alone has no other backend to score.
#[cfg(not(feature = "blas"))]
fn backends(_: EmbeddingModel) -> Result<Vec<Backend>> {
    Ok(Vec::new())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let model = match &args.model {
        Some(id) => EmbeddingModel::from_id(id).ok_or_else(|| anyhow!("funes does not run {id:?}"))?,
        None => EmbeddingModel::BgeSmallEn,
    };
    let mut backends = backends(model)?;
    if backends.is_empty() {
        println!("nothing to score: this build has no backend besides ONNX");
        return Ok(());
    }
    let mut onnx_embedder = OnnxEmbedder::new(model)?;
    let mut onnx_reranker = OnnxReranker::new()?;
    let workloads = workloads::workloads();

    println!("embedding: {}", model.id());
    println!("{:<12} {:<8} {:>12}", "workload", "backend", "cos↔onnx");
    for (label, docs) in &workloads {
        let docs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let want = onnx_embedder.embed(&docs)?;
        for (name, embedder, _) in backends.iter_mut() {
            let got = embedder.embed(&docs)?;
            let min = got.iter().zip(&want).map(|(a, b)| cosine(a, b)).fold(1f32, f32::min);
            println!("{label:<12} {name:<8} {min:>12.6}");
        }
    }

    println!("\nreranking: gte-reranker-modernbert-base");
    println!("{:<12} {:<8} {:>12}", "workload", "backend", "Δ↔onnx");
    for (label, docs) in &workloads {
        let docs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let want = onnx_reranker.rerank(workloads::QUERY, &docs)?;
        for (name, _, reranker) in backends.iter_mut() {
            let got = reranker.rerank(workloads::QUERY, &docs)?;
            let max = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
            println!("{label:<12} {name:<8} {max:>12.6}");
        }
    }
    Ok(())
}
