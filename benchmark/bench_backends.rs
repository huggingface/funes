//! A/B the inference backends behind the `Embedder`/`Reranker` traits: latency + agreement (does a
//! faster backend embed/rank the same as the reference?). Backend-agnostic and cross-platform — it
//! compares whatever backends are compiled in, with ONNX (fastembed) as the reference when present:
//!   cargo run --release --features onnx --example bench_backends
//!
//! Two workloads, because they stress different things: a batch of short docs is dominated by
//! per-call overheads (tokenization, thread spawns), while 30 docs at the 512-token truncation
//! cap — recall's rerank worst case — is dominated by GEMM throughput and memory behavior.
//!
//! Adding a backend = impl Embedder+Reranker, gate it behind a feature, and push it in `backends()`.

use std::time::Instant;

use anyhow::Result;
use funes::inference::{Embedder, Reranker};

const QUERY: &str = "why did we move the reranker off the onnx runtime";

fn short_docs() -> Vec<String> {
    [
        "the reranker is a cross-encoder scoring query-passage pairs jointly.",
        "onnx runtime uses MLAS on the CPU, which on Apple Silicon runs on NEON, not AMX.",
        "a hand-written forward calls Accelerate cblas_sgemm, which reaches the AMX matrix units.",
        "the embedding model is bge-small-en-v1.5, a 384-dimensional BERT sentence encoder.",
        "recall fuses vector ANN and BM25 hits by reciprocal rank before reranking.",
        "the store is a lance dataset with an IVF_PQ vector index and a full-text index.",
        "candle's metal backend was missing a layer-norm kernel, so it could not run the model.",
        "the cat knocked a glass off the counter and it shattered on the floor.",
        "quarterly revenue rose twelve percent on strong subscription renewals.",
        "tokenization uses the huggingface tokenizers crate loading the model's tokenizer.json.",
        "softmax over the attention scores uses a vectorized exp from the platform seam.",
        "the recipe needs two cups of flour, a teaspoon of salt, and three eggs.",
        "fp8 has no hardware path on apple silicon, so int8 is the only accelerated low-precision.",
        "attention masks let the transformer ignore padding tokens in a ragged batch.",
        "the marathon route winds through six neighborhoods before the riverside finish.",
        "hf-hub fetches whole files because the xet cdn taxes every byte-range read.",
    ]
    .map(String::from)
    .to_vec()
}

fn long_docs() -> Vec<String> {
    let sent = "recall fuses vector ann and bm25 hits by reciprocal rank before the cross-encoder \
                rescores each candidate against the query using joint attention over the pair. ";
    // ~500 tokens after tokenization, truncated at 512 — recall's rerank candidates at the cap.
    let doc = sent.repeat(18);
    vec![doc; 30]
}

struct Backend {
    name: &'static str,
    emb: Box<dyn Embedder>,
    rr: Box<dyn Reranker>,
}

fn backends() -> Result<Vec<Backend>> {
    let mut v: Vec<Backend> = Vec::new();
    // ONNX first when compiled in: the first backend is the agreement reference.
    #[cfg(feature = "onnx")]
    {
        use funes::inference::{OnnxEmbedder, OnnxReranker};
        v.push(Backend {
            name: "onnx",
            emb: Box::new(OnnxEmbedder::new()?),
            rr: Box::new(OnnxReranker::new()?),
        });
    }
    #[cfg(feature = "blas")]
    {
        use funes::blas::{BlasEmbedder, BlasReranker};
        v.push(Backend {
            name: "blas",
            emb: Box::new(BlasEmbedder::new()?),
            rr: Box::new(BlasReranker::new()?),
        });
    }
    Ok(v)
}

fn time<F: FnMut()>(warmups: usize, iters: usize, mut f: F) -> f64 {
    for _ in 0..warmups {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    t.elapsed().as_secs_f64() * 1000.0 / iters as f64
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

fn main() -> Result<()> {
    let mut backs = backends()?;
    if backs.len() < 2 {
        eprintln!("note: only one backend is compiled — build with `--features onnx` to A/B against the reference\n");
    }

    // (label, docs, warmups, timed iters) — fewer iterations for the long shape, where a single
    // forward runs for seconds.
    let workloads = [("16×short", short_docs(), 2, 5), ("30×~500tok", long_docs(), 1, 3)];

    println!(
        "{:<12} {:<12} {:>10} {:>11} {:>16} {:>16}",
        "workload", "backend", "embed ms", "rerank ms", "embed cos↔ref", "rerank Δ↔ref"
    );
    for (label, docs, warmups, iters) in &workloads {
        let docs: Vec<&str> = docs.iter().map(String::as_str).collect();
        // Run each backend once for correctness, then time it. Reference = the first backend.
        let mut emb_out: Vec<Vec<Vec<f32>>> = Vec::new();
        let mut rr_out: Vec<Vec<f32>> = Vec::new();
        for b in backs.iter_mut() {
            let e = b.emb.embed(&docs)?;
            let r = b.rr.rerank(QUERY, &docs)?;
            let ems = time(*warmups, *iters, || {
                b.emb.embed(&docs).unwrap();
            });
            let rms = time(*warmups, *iters, || {
                b.rr.rerank(QUERY, &docs).unwrap();
            });
            let (ecos, rdiff) = if emb_out.is_empty() {
                ("(ref)".to_string(), "(ref)".to_string())
            } else {
                let ref_e = &emb_out[0];
                let cos_min = e.iter().zip(ref_e).map(|(a, b)| cosine(a, b)).fold(1f32, f32::min);
                let d = r
                    .iter()
                    .zip(&rr_out[0])
                    .map(|(a, b)| (a - b).abs())
                    .fold(0f32, f32::max);
                (format!("{cos_min:.6}"), format!("{d:.6}"))
            };
            println!(
                "{label:<12} {:<12} {ems:>10.1} {rms:>11.1} {ecos:>16} {rdiff:>16}",
                b.name
            );
            emb_out.push(e);
            rr_out.push(r);
        }
    }
    Ok(())
}
