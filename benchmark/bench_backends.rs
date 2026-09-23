//! A/B the inference backends behind the `Embedder`/`Reranker` traits: latency + agreement (does a
//! faster backend embed/rank the same as the reference?). Backend-agnostic and cross-platform — it
//! compares whatever backends are compiled in, with ONNX (fastembed) as the reference when present:
//!   cargo run --release --features onnx --example bench_backends
//!
//! Six workloads, because they stress different things: a batch of short docs is dominated by
//! per-call overheads (tokenization, thread spawns), while 30 docs at the 512-token truncation
//! cap — recall's rerank worst case — is dominated by GEMM throughput and memory behavior. The
//! ragged batch is real indexing's shape — batch-longest padding masks most attention columns,
//! whose softmax weights underflow, and the scores×V GEMM then reads what they leave behind. The
//! real batch spreads its lengths the way measured chunks do, so it shows what a backend spends
//! on padding a mixed batch. The 256-document workloads match the production batch size and show
//! how grouping by length scales.
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
        "the memory is a lance dataset with an IVF_PQ vector index and a full-text index.",
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

const SENT: &str = "recall fuses vector ann and bm25 hits by reciprocal rank before the cross-encoder \
                    rescores each candidate against the query using joint attention over the pair. ";

fn capped_docs(n: usize) -> Vec<String> {
    // ~500 tokens after tokenization, truncated at 512 — recall's rerank candidates at the cap.
    let doc = SENT.repeat(18);
    vec![doc; n]
}

fn long_docs() -> Vec<String> {
    capped_docs(30)
}

/// A ragged batch — a couple of ~512-token docs, the rest short: real indexing's shape, and the
/// one that exposes padding stalls.
fn mixed_docs() -> Vec<String> {
    let short = short_docs();
    let mut docs = capped_docs(2);
    docs.extend((0..16 - docs.len()).map(|i| short[i % short.len()].clone()));
    docs
}

/// Words of `SENT` whose embedder token counts land on the 32 quantiles of real chunk lengths,
/// measured over 12,971 chunks of 100 Claude Code sessions: 21 up to the 512 cap, median 314.
const REAL_WORDS: [usize; 32] = [
    13, 30, 38, 51, 66, 87, 106, 126, 145, 163, 176, 187, 196, 205, 213, 218, 226, 232, 238, 247, 255, 262, 269, 280,
    288, 296, 306, 314, 327, 340, 362, 363,
];

/// One doc per quantile of real chunk lengths, in a scrambled order — the length spread indexing
/// hands `embed`, and about recall's 30 rerank candidates.
fn real_docs() -> Vec<String> {
    let words: Vec<&str> = SENT.split_whitespace().cycle().take(400).collect();
    (0..32).map(|i| words[..REAL_WORDS[i * 13 % 32]].join(" ")).collect()
}

/// Like `mixed_docs` but 256 documents — the batch size `embed_batched` uses in production.
fn big_mixed_docs() -> Vec<String> {
    let short = short_docs();
    let mut docs = capped_docs(32);
    docs.extend((0..256 - docs.len()).map(|i| short[i % short.len()].clone()));
    docs
}

/// Like `real_docs` but 256 documents — the production batch size, with the same length spread.
fn big_real_docs() -> Vec<String> {
    let words: Vec<&str> = SENT.split_whitespace().cycle().take(400).collect();
    (0..256).map(|i| words[..REAL_WORDS[i % 32]].join(" ")).collect()
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
        use funes::inference::blas::{BlasEmbedder, BlasReranker};
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

    // (label, docs, warmups, timed iters) — few iterations where a single forward runs seconds,
    // except the ragged batch: it carries the padding behavior worth measuring, and one timed
    // iteration cannot resolve a difference of a few percent.
    let workloads = [
        ("16×short", short_docs(), 2, 5),
        ("30×~500tok", long_docs(), 1, 3),
        ("16×mixed", mixed_docs(), 1, 5),
        ("32×real", real_docs(), 1, 3),
        ("256×mixed", big_mixed_docs(), 1, 1),
        ("256×real", big_real_docs(), 1, 1),
    ];

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
