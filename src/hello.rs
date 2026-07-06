//! The built-in hello-world corpus: onboarding passages recall falls back to when there's no local
//! index yet, so a fresh install returns something useful. Superseded once `funes index` runs.

use crate::chunk::Chunk;
use crate::dataset;
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
        "Build the index first: run `funes index`. It walks ~/.claude/projects, \
         ~/.codex/sessions, and ~/.pi/agent/sessions, parses each session, and embeds the turns \
         into a local store at ~/.funes. It's incremental — \
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
         `funes add <agent>`. New sessions then get the `recall` and `get` tools plus \
         instructions on when to use them, so the agent reaches for prior decisions and \
         rationale on its own — without you pasting context.",
    ),
    (
        "assistant",
        "Keep recall fresh. The index only updates when `funes index` runs, so the latest turns of \
         the current session aren't searchable until you re-run it. Re-run it periodically, or add \
         a session-end hook that runs `funes index` after each session (see docs/automation.md).",
    ),
    (
        "assistant",
        "Optional, and later: share your memory across machines or a team via the Hugging Face \
         Hub. `funes use <org>/<repo>` attaches a dataset repo you own as your active store — \
         recall then reads it, and `funes push` publishes your local index to it; `funes use \
         local` detaches. To query a different store for one call without changing your default, \
         pass `recall --remote <org>/<repo>`. You never need the Hub to use funes locally — it's \
         a tier you opt into.",
    ),
    (
        "assistant",
        "funes is local-first and deterministic: no LLM in the ingest path, the embedding model is \
         pinned (BAAI/bge-small-en-v1.5), and the index is a disposable derived artifact you can \
         always rebuild from your transcripts. The transcripts are the source of truth.",
    ),
];

/// A short, human-readable welcome shown by `funes guide`. It is the friendly counterpart to
/// `recall`'s ranked, provenance-tagged output: that format is built for an agent to parse, and on
/// a fresh install it makes a person work to read a scrambled list. A first-time human runs this
/// instead. Kept separate from `PASSAGES` (which feeds the agent-facing recall fallback) so each
/// can speak to its own audience.
pub fn guide() -> String {
    "\
funes — durable, local memory for your AI coding agent

funes indexes your past AI agent sessions and lets your agent recall its own decisions,
rationale, and findings mid-task — the exact passages, with provenance, all on your machine.

Getting started

  1. Index your sessions       funes index
     Sweeps ~/.claude/projects, ~/.codex/sessions, and ~/.pi/agent/sessions into one local
     store. It's incremental, so it's cheap to re-run as you work.

  2. Add funes to your agent    funes add <agent>
     Works with claude, codex, pi, hermes, and opencode. Your agent gains `recall` and
     `get` as tools and reaches for them on its own — no pasting context back in.

  3. Just work.
     When something touches a past decision, its rationale, or an earlier finding, your
     agent recalls it for you — no re-pasting context.

Once indexed, you can query the store yourself:

  funes recall \"why did we switch off lancedb\"    ask in natural language
  funes list                                       browse indexed sessions
  funes status                                     what's indexed, and which store you're on

Share across machines or a team (optional)

  funes use <org>/<repo>    attach a Hugging Face dataset you own as your store
  funes push                publish your local index to it

Your indexed sessions stay on your machine until you `funes push`.
"
    .to_string()
}

/// Build the corpus as an ephemeral lance dataset; the returned temp dir backs it (keep alive
/// while reading). With an `embedder`, passages get real vectors for search; without, zeros.
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
            harness: "claude_code".to_string(),
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
    let uri = dataset::table_uri(&dir.path().to_string_lossy());
    let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
    let ds = Dataset::write(reader, &uri, None).await?;
    Ok((dir, ds))
}
