//! The built-in hello-world corpus: a tiny set of onboarding passages funes can recall before
//! the user has indexed anything. A fresh install runs the real recall pipeline against this, so
//! the first `funes recall` returns something useful — and the passages double as the
//! getting-started guide (how to index, recall, wire into an agent, and optionally push).
//!
//! It's a read-only fallback: the moment `funes index` builds the local store, recall reads the
//! user's own history and this corpus steps aside (see [`crate::recall`]).

use crate::chunk::Chunk;
use crate::index::{self, DIM};
use anyhow::Result;
use arrow_array::RecordBatchIterator;
use fastembed::TextEmbedding;
use lance::dataset::Dataset;
use tempfile::TempDir;

/// Synthetic session these passages belong to: they surface as `funes/hello` in recall output and
/// resolve under `funes get hello <turn>`.
const SESSION: &str = "hello";
const PROJECT: &str = "funes";

/// The onboarding passages, in reading order, as `(role, text)`. The first is phrased as the
/// user's question so `list` has a sensible summary line; the rest are guidance. Edit these
/// freely — they're plain text, embedded fresh at runtime.
pub const PASSAGES: &[(&str, &str)] = &[
    ("user", "What is funes and how do I get started?"),
    (
        "assistant",
        "funes gives an AI agent durable, mid-term memory of your past Claude Code sessions. It \
         indexes your transcripts locally and serves selective, reranked recall: you ask in \
         natural language and get back the few relevant passages with exact provenance — not a \
         flood of detail, and never an LLM-rewritten summary.",
    ),
    (
        "assistant",
        "Build the index first: run `funes index`. It walks ~/.claude/projects, parses each \
         session, and embeds the turns into a local store at ~/.funes. It's incremental — \
         re-running it only adds new turns — so it's cheap to run often.",
    ),
    (
        "assistant",
        "Recall with `funes recall \"<your question>\"`. The pipeline is hybrid vector + BM25 \
         search, then a cross-encoder rerank, then recency weighting, then neighbor expansion. \
         Narrow with `--type text|thinking|tool_use|tool_result` and `--project <name>`; tune \
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
        "Wire funes into your agent so it can recall on its own: register the MCP server with \
         `claude mcp add funes -- /path/to/funes mcp`. New sessions then get the `recall` and \
         `get` tools plus instructions on when to use them, so the agent reaches for prior \
         decisions and rationale on its own — without you pasting context.",
    ),
    (
        "assistant",
        "Keep recall fresh. The index only updates when `funes index` runs, so the latest turns of \
         the current session aren't searchable until you re-run it. Re-run it periodically, or add \
         a Claude Code Stop hook that runs `funes index` after each session.",
    ),
    (
        "assistant",
        "Optional, and later: share your memory across machines or a team via the Hugging Face \
         Hub. `funes use <org>/<repo>` attaches a dataset repo you own as your active store — from \
         then on `index` publishes to it and recall reads it; `funes use local` detaches. To query \
         a different store for one call without changing your default, pass `recall --remote \
         <org>/<repo>`. You never need the Hub to use funes locally — it's a tier you opt into.",
    ),
    (
        "assistant",
        "funes is local-first and deterministic: no LLM in the ingest path, the embedding model is \
         pinned (BAAI/bge-small-en-v1.5), and the index is a disposable derived artifact you can \
         always rebuild from your transcripts. The transcripts are the source of truth.",
    ),
];

/// Build the hello-world corpus as an ephemeral lance dataset, returning the temp dir that backs
/// it — keep it alive for as long as the dataset is read. With an `embedder`, passages get real
/// vectors so `recall` can search them; without one, vectors are zero (enough for `get`/`list`,
/// which scan columns but never the vector).
pub async fn dataset(embedder: Option<&mut TextEmbedding>) -> Result<(TempDir, Dataset)> {
    let ts = chrono::Utc::now().to_rfc3339();
    let chunks: Vec<Chunk> = PASSAGES
        .iter()
        .enumerate()
        .map(|(i, (role, text))| Chunk {
            id: format!("{SESSION}-{i:04}"),
            text: (*text).to_string(),
            session_id: SESSION.to_string(),
            project: PROJECT.to_string(),
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
        })
        .collect();

    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let vectors: Vec<Vec<f32>> = match embedder {
        Some(e) => e.embed(texts, None)?,
        None => vec![vec![0.0; DIM as usize]; chunks.len()],
    };

    let batch = index::build_batch(&chunks, &vectors)?;
    let schema = batch.schema();
    let dir = tempfile::tempdir()?;
    let uri = crate::dataset::table_uri(&dir.path().to_string_lossy());
    let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
    let ds = Dataset::write(reader, &uri, None).await?;
    Ok((dir, ds))
}
