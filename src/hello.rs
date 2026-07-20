//! The built-in hello-world corpus: onboarding passages recall falls back to when there's no local
//! index yet, so a fresh install returns something useful. Superseded once `funes index` runs.

use crate::chunk::Chunk;
use crate::dataset;
use crate::index::{self, DIM};
use crate::inference::Embedder;
use anyhow::Result;
use arrow_array::RecordBatchIterator;
use lance::dataset::Dataset;
use tempfile::TempDir;

/// Synthetic session these passages belong to: they surface as `funes/hello` in recall output and
/// resolve under `funes get hello <turn>`.
const SESSION: &str = "hello";
const WORKDIR: &str = "funes";

/// The onboarding passages as `(role, text)`. The first is the user's question (so `list` has a
/// summary line); the rest are guidance.
pub const PASSAGES: &[(&str, &str)] = &[
    ("user", "What is funes and how do I get started?"),
    (
        "assistant",
        "funes gives an AI agent durable, mid-term memory of your past coding-agent sessions. It \
         indexes your transcripts locally and serves selective, reranked recall: you ask in \
         natural language and get back the few relevant passages with exact provenance — not a \
         flood of detail, and never an LLM-rewritten summary.",
    ),
    (
        "assistant",
        "Get started by wiring funes into your agent: run `funes add <agent>` — claude, codex, \
         pi, or hermes. New sessions then get the `recall` and `get` tools plus \
         instructions on when to use them, so the agent reaches for prior decisions and rationale \
         on its own — without you pasting context. For Claude, Codex, and Hermes, `funes add` also \
         builds your first index — a fast, text-first pass, after asking — and installs hooks that \
         keep it current as you work, with deeper content backfilling per turn. Nothing is left to \
         run by hand.",
    ),
    (
        "assistant",
        "Under the hood, recall reads a local store built by `funes index`: it walks \
         ~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions, and ~/.hermes/state.db, \
         parses each session, and embeds the turns into ~/.funes. It's incremental and text-first — \
         re-running it (and the hooks `funes add` installs for Claude, Codex, and Hermes) adds new \
         turns and backfills deeper content a step at a time. With pi, re-run it yourself so the \
         latest turns are searchable.",
    ),
    (
        "assistant",
        "Recall with `funes recall \"<your question>\"`. The pipeline is hybrid vector + BM25 \
         search, then a cross-encoder rerank, then recency weighting, then neighbor expansion. \
         Narrow with `--type text|thinking|tool_use|tool_result` and `--harness <name>`; tune \
         breadth with `--k` (results) and `--candidates` (the rerank pool).",
    ),
    (
        "assistant",
        "Drill into a hit: every recall result prints a `→ get <session_id> <turn_uuid>` line. Run \
         `funes get <session_id> <turn_uuid>` to expand that hit into its full surrounding turns. \
         Try it on this corpus: `funes get hello hello-0005`.",
    ),
    (
        "assistant",
        "Optional, and later: share your memory across machines or a team via the Hugging Face \
         Hub. Name a dataset repo you own when you add funes to an agent — `funes add claude \
         <org>/<repo>` — and recall reads it while your sessions publish to it. Or drive it \
         directly: `funes push <org>/<repo>` publishes your local store, and `recall --store \
         <org>/<repo>` reads any store for one call. You never need the Hub to use funes locally — \
         it's a tier you opt into.",
    ),
    (
        "assistant",
        "funes is local-first and deterministic: no LLM in the ingest path, the embedding model is \
         pinned (BAAI/bge-small-en-v1.5), and the store is a disposable derived artifact you can \
         always rebuild from your transcripts. The transcripts are the source of truth.",
    ),
];

/// Build the corpus as an ephemeral lance dataset; the returned temp dir backs it (keep alive
/// while reading). With an `embedder`, passages get real vectors for search; without, zeros.
pub async fn dataset(embedder: Option<&mut dyn Embedder>) -> Result<(TempDir, Dataset)> {
    let ts = chrono::Utc::now().to_rfc3339();
    let chunks: Vec<Chunk> = PASSAGES
        .iter()
        .enumerate()
        .map(|(i, (role, text))| Chunk {
            id: format!("{SESSION}-{i:04}"),
            text: (*text).to_string(),
            session_id: SESSION.to_string(),
            workdir: WORKDIR.to_string(),
            turn_uuid: format!("{SESSION}-{i:04}"),
            parent_uuid: (i > 0).then(|| format!("{SESSION}-{:04}", i - 1)),
            seq: i as i64,
            ts: ts.clone(),
            role: (*role).to_string(),
            block_type: "text".to_string(),
            tool_name: None,
            source_path: "built-in".to_string(),
            block_idx: 0,
            split_idx: 0,
            harness: "claude_code".to_string(),
            repo: String::new(),
        })
        .collect();

    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let vectors: Vec<Vec<f32>> = match embedder {
        Some(e) => e.embed(&texts)?,
        None => vec![vec![0.0; DIM as usize]; chunks.len()],
    };

    let batch = index::build_batch(&chunks, &vectors)?;
    let schema = batch.schema();
    let dir = tempfile::tempdir()?;
    let uri = dataset::table_uri(&dir.path().to_string_lossy());
    let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
    let ds = Dataset::write(reader, &uri, None).await?;
    Ok((dir, ds))
}
