//! Time funes's embedder and reranker on one inference backend:
//!
//! ```text
//! cargo run --release --example bench_models -- --backend metal
//! ```
//!
//! `--backend blas` (the default) times the CPU forwards every build carries, `--backend metal` the
//! GPU forwards of a macOS build. These are the forwards themselves: funes runs them behind a router
//! that keeps a call on the CPU until it holds `GPU_MIN_TEXTS` texts or `GPU_MIN_PAIRS` pairs, and
//! the two backends' startup and first calls are what those thresholds weigh. Each model's startup
//! comes first: loading it, then its first call, which a one-shot process pays in full (the GPU
//! uploads its weights and compiles a graph per batch shape on first use). Then each workload's time
//! per call, after one warm-up call.

#[path = "workloads.rs"]
mod workloads;

use std::time::Instant;

use anyhow::{anyhow, Result};
use clap::{Parser, ValueEnum};
use funes::inference::{Embedder, EmbeddingModel, Reranker};

#[derive(Parser)]
struct Args {
    /// The backend whose forwards are timed.
    #[arg(long, value_enum, default_value_t = Backend::Blas)]
    backend: Backend,
    /// The embedding model, by its Hub id. `BAAI/bge-small-en-v1.5` when omitted.
    #[arg(long)]
    model: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    /// The CPU forwards, in every build.
    Blas,
    /// The GPU forwards, in a macOS build.
    #[cfg(all(feature = "metal", target_os = "macos"))]
    Metal,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Backend::Blas => "blas",
            #[cfg(all(feature = "metal", target_os = "macos"))]
            Backend::Metal => "metal",
        }
    }

    fn embedder(self, model: EmbeddingModel) -> Result<Box<dyn Embedder>> {
        Ok(match self {
            Backend::Blas => Box::new(funes::inference::blas::BlasEmbedder::new(model)?),
            #[cfg(all(feature = "metal", target_os = "macos"))]
            Backend::Metal => Box::new(funes::inference::metal::MetalEmbedder::new(model)?),
        })
    }

    fn reranker(self) -> Result<Box<dyn Reranker>> {
        Ok(match self {
            Backend::Blas => Box::new(funes::inference::blas::GteReranker::new()?),
            #[cfg(all(feature = "metal", target_os = "macos"))]
            Backend::Metal => Box::new(funes::inference::metal::MetalReranker::new()?),
        })
    }
}

/// Timed calls for a workload of `n` documents: fewer where a call runs seconds. Up to 16 documents
/// get 5, since the ragged batch carries the padding behavior worth measuring and one call cannot
/// resolve a difference of a few percent.
fn iterations(n: usize) -> usize {
    match n {
        0..=16 => 5,
        17..=100 => 3,
        _ => 1,
    }
}

/// What `f` returns, and the milliseconds it took.
fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<(T, f64)> {
    let t = Instant::now();
    let out = f()?;
    Ok((out, t.elapsed().as_secs_f64() * 1000.0))
}

/// Milliseconds per call of `f` over `iters` calls, after one warm-up call.
fn per_call(iters: usize, mut f: impl FnMut() -> Result<()>) -> Result<f64> {
    f()?;
    let ((), ms) = timed(|| (0..iters).try_for_each(|_| f()))?;
    Ok(ms / iters as f64)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let model = match &args.model {
        Some(id) => EmbeddingModel::from_id(id).ok_or_else(|| anyhow!("funes does not run {id:?}"))?,
        None => EmbeddingModel::BgeSmallEn,
    };
    let real = workloads::real_docs();
    let real: Vec<&str> = real.iter().map(String::as_str).collect();

    let (mut emb, emb_load) = timed(|| args.backend.embedder(model))?;
    let (_, emb_first) = timed(|| emb.embed(&real))?;
    let (mut rr, rr_load) = timed(|| args.backend.reranker())?;
    let (_, rr_first) = timed(|| rr.rerank(workloads::QUERY, &real))?;
    println!(
        "{} backend: {} and gte-reranker-modernbert-base",
        args.backend.name(),
        model.id()
    );
    println!("\nstartup: load, then a first call on 32×real");
    println!("{:<10} {:>10} {:>14}", "model", "load ms", "first call ms");
    println!("{:<10} {emb_load:>10.0} {emb_first:>14.0}", "embedder");
    println!("{:<10} {rr_load:>10.0} {rr_first:>14.0}", "reranker");

    println!(
        "\n{:<12} {:>10} {:>10} {:>11} {:>10}",
        "workload", "embed ms", "texts/s", "rerank ms", "pairs/s"
    );
    for (label, docs) in workloads::workloads() {
        let docs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let iters = iterations(docs.len());
        let embed_ms = per_call(iters, || emb.embed(&docs).map(drop))?;
        let rerank_ms = per_call(iters, || rr.rerank(workloads::QUERY, &docs).map(drop))?;
        let per_s = |ms: f64| docs.len() as f64 * 1000.0 / ms;
        println!(
            "{label:<12} {embed_ms:>10.1} {:>10.0} {rerank_ms:>11.1} {:>10.0}",
            per_s(embed_ms),
            per_s(rerank_ms)
        );
    }
    Ok(())
}
