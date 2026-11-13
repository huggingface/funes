//! Seven workloads of documents to embed and rerank, because they stress different things. A batch
//! of short docs is dominated by per-call overheads (tokenization, thread spawns), while 30 docs at
//! the 512-token truncation cap, recall's rerank worst case, are dominated by GEMM throughput and
//! memory behavior. The ragged batch is real indexing's shape: batch-longest padding masks most
//! attention columns, whose softmax weights underflow, and the scores×V GEMM then reads what they
//! leave behind. The real batch spreads its lengths the way measured chunks do, so it shows what a
//! backend spends on padding a mixed batch. The 256-document workloads are an indexing call's size
//! on the CPU, and show how grouping by length scales. The 100-document one is recall's pool at
//! `--candidates 100`.
//!
//! No two documents of a workload are the same text: the GPU embedder embeds a repeated text
//! once, so a workload with repeats would time fewer documents than it holds.

/// The query every reranker workload scores its documents against.
pub const QUERY: &str = "why did we move the reranker off the onnx runtime";

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

/// `n` documents of about 500 tokens, truncated at 512: recall's rerank candidates at the cap.
fn capped_docs(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{i} {}", SENT.repeat(18))).collect()
}

/// A ragged batch, two ~512-token docs and the rest short: real indexing's shape, and the one that
/// exposes padding stalls.
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

/// One doc per quantile of real chunk lengths, in a scrambled order: the length spread indexing
/// hands `embed`, and about recall's 30 rerank candidates.
pub fn real_docs() -> Vec<String> {
    let words: Vec<&str> = SENT.split_whitespace().cycle().take(400).collect();
    (0..32).map(|i| words[..REAL_WORDS[i * 13 % 32]].join(" ")).collect()
}

/// Like `mixed_docs` but 256 documents, an indexing call's size on the CPU.
fn big_mixed_docs() -> Vec<String> {
    let short = short_docs();
    let mut docs = capped_docs(32);
    docs.extend((0..256 - docs.len()).map(|i| format!("{i} {}", short[i % short.len()])));
    docs
}

/// Like `real_docs` but 256 documents, with the same length spread: an indexing call's size on the
/// CPU.
fn big_real_docs() -> Vec<String> {
    let words: Vec<&str> = SENT.split_whitespace().cycle().take(400).collect();
    (0..256)
        .map(|i| format!("{i} {}", words[..REAL_WORDS[i % 32]].join(" ")))
        .collect()
}

/// Like `real_docs` but 100 documents: recall's pool at `--candidates 100`.
fn deep_docs() -> Vec<String> {
    let words: Vec<&str> = SENT.split_whitespace().cycle().take(400).collect();
    (0..100)
        .map(|i| format!("{i} {}", words[..REAL_WORDS[i * 13 % 32]].join(" ")))
        .collect()
}

/// Each workload's label and documents.
pub fn workloads() -> Vec<(&'static str, Vec<String>)> {
    vec![
        ("16×short", short_docs()),
        ("30×~500tok", capped_docs(30)),
        ("16×mixed", mixed_docs()),
        ("32×real", real_docs()),
        ("100×real", deep_docs()),
        ("256×mixed", big_mixed_docs()),
        ("256×real", big_real_docs()),
    ]
}
