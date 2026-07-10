//! A/B the inference backends behind the `Embedder`/`Reranker` traits: latency + agreement (does a
//! faster backend embed/rank the same as the reference?). Backend-agnostic and cross-platform — it
//! compares whatever backends are compiled in. ONNX (fastembed) is always present and is the
//! reference; enable others via features:
//!   cargo run --release --features blas --example bench_backends
//!
//! Adding a backend = impl Embedder+Reranker, gate it behind a feature, and push it in `backends()`.

use std::time::Instant;

use anyhow::Result;
use funes::inference::{Embedder, OnnxEmbedder, OnnxReranker, Reranker};

const QUERY: &str = "why did we move the reranker off the onnx runtime";

fn docs() -> Vec<&'static str> {
    vec![
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
}

struct Backend {
    name: &'static str,
    emb: Box<dyn Embedder>,
    rr: Box<dyn Reranker>,
}

fn backends() -> Result<Vec<Backend>> {
    let mut v: Vec<Backend> = Vec::new();
    // ONNX is always available and serves as the agreement reference.
    v.push(Backend {
        name: "onnx",
        emb: Box::new(OnnxEmbedder::new()?),
        rr: Box::new(OnnxReranker::new()?),
    });
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

fn time<F: FnMut()>(mut f: F) -> f64 {
    for _ in 0..2 {
        f();
    }
    let t = Instant::now();
    for _ in 0..5 {
        f();
    }
    t.elapsed().as_secs_f64() * 1000.0 / 5.0
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

fn main() -> Result<()> {
    let docs = docs();
    let mut backs = backends()?;
    if backs.len() < 2 {
        eprintln!("note: only the `onnx` backend is compiled — enable another to compare, e.g. --features blas\n");
    }

    // Run each backend once for correctness, then time it. Reference = the first backend (onnx).
    let mut emb_out: Vec<Vec<Vec<f32>>> = Vec::new();
    let mut rr_out: Vec<Vec<f32>> = Vec::new();
    println!(
        "{:<12} {:>10} {:>11} {:>16} {:>16}",
        "backend", "embed ms", "rerank ms", "embed cos↔onnx", "rerank Δ↔onnx"
    );
    for b in backs.iter_mut() {
        let e = b.emb.embed(&docs)?;
        let r = b.rr.rerank(QUERY, &docs)?;
        let ems = time(|| {
            b.emb.embed(&docs).unwrap();
        });
        let rms = time(|| {
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
        println!("{:<12} {ems:>10.1} {rms:>11.1} {ecos:>16} {rdiff:>16}", b.name);
        emb_out.push(e);
        rr_out.push(r);
    }
    Ok(())
}
