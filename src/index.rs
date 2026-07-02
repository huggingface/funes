//! The `index` command: read a [`crate::source::TraceSource`] → parse → chunk → embed → write to a
//! local Lance dataset. One generic loop drives every source — a JSONL tree today, new formats by
//! implementing the trait — indexing each of its units in a single append.
//!
//! Incremental on two levels: skip a unit whose signature is unchanged (size:mtime in state.json),
//! and within a re-read unit add only chunks whose id is new — a grown session (the same memory)
//! contributes just its new turns, nothing is re-embedded or deleted.

use crate::harness::Harness;
use crate::{chunk, dataset, scan, source, trace};
use anyhow::{anyhow, Result};
use arrow_array::types::Float32Type;
use arrow_array::{Array, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use lance::dataset::{Dataset, WriteParams};
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub const MODEL: &str = "BAAI/bge-small-en-v1.5";
pub const DIM: i32 = 384;
const EMBED_BATCH: usize = 256;

/// The table schema (column order is load-bearing for Lance).
pub(crate) fn schema() -> Arc<Schema> {
    let utf8 = |name: &str| Field::new(name, DataType::Utf8, true);
    let i64f = |name: &str| Field::new(name, DataType::Int64, true);
    Arc::new(Schema::new_with_metadata(
        vec![
            utf8("id"),
            utf8("text"),
            utf8("session_id"),
            utf8("project"),
            utf8("turn_uuid"),
            utf8("parent_uuid"),
            i64f("seq"),
            utf8("ts"),
            utf8("role"),
            utf8("block_type"),
            utf8("tool_name"),
            utf8("source_path"),
            i64f("block_idx"),
            i64f("split_idx"),
            Field::new(
                "vector",
                DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), DIM),
                true,
            ),
            // Last, after `vector`: `add_columns` appends a migrated column at the end, so a
            // freshly-built store must match that order (the tripwire test pins it).
            utf8("harness"),
        ],
        HashMap::from([("embedding_model".to_string(), MODEL.to_string())]),
    ))
}

pub(crate) fn build_batch(chunks: &[chunk::Chunk], vectors: &[Vec<f32>]) -> Result<RecordBatch> {
    let s = |f: &dyn Fn(&chunk::Chunk) -> Option<String>| -> StringArray { chunks.iter().map(f).collect() };
    let i = |f: &dyn Fn(&chunk::Chunk) -> i64| -> Int64Array { chunks.iter().map(|c| Some(f(c))).collect() };
    let vector = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        vectors
            .iter()
            .map(|v| Some(v.iter().map(|&x| Some(x)).collect::<Vec<_>>())),
        DIM,
    );
    Ok(RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(s(&|c| Some(c.id.clone()))),
            Arc::new(s(&|c| Some(c.text.clone()))),
            Arc::new(s(&|c| Some(c.session_id.clone()))),
            Arc::new(s(&|c| Some(c.project.clone()))),
            Arc::new(s(&|c| Some(c.turn_uuid.clone()))),
            Arc::new(s(&|c| c.parent_uuid.clone())),
            Arc::new(i(&|c| c.seq)),
            Arc::new(s(&|c| Some(c.ts.clone()))),
            Arc::new(s(&|c| Some(c.role.clone()))),
            Arc::new(s(&|c| Some(c.block_type.clone()))),
            Arc::new(s(&|c| c.tool_name.clone())),
            Arc::new(s(&|c| Some(c.source_path.clone()))),
            Arc::new(i(&|c| c.block_idx)),
            Arc::new(i(&|c| c.split_idx)),
            Arc::new(vector),
            Arc::new(s(&|c| Some(c.harness.clone()))),
        ],
    )?)
}

/// Embed `texts` in batches of [`EMBED_BATCH`], calling `on_batch(embedded_so_far)` after each so a
/// caller can report progress (or pass a no-op).
pub(crate) fn embed_batched(
    embedder: &mut TextEmbedding,
    texts: &[&str],
    mut on_batch: impl FnMut(usize),
) -> Result<Vec<Vec<f32>>> {
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for group in texts.chunks(EMBED_BATCH) {
        vectors.extend(embedder.embed(group, None)?);
        on_batch(vectors.len());
    }
    Ok(vectors)
}

/// Every chunk id already stored. Re-indexing keeps only the chunks whose id isn't here, so a grown
/// session (the same memory) contributes just its new turns — nothing is re-embedded or deleted. (A
/// rewritten turn arrives under new ids, i.e. as another memory.) Chunk ids are global, so one
/// unfiltered scan dedups any unit, whether it holds one session or thousands.
async fn stored_ids(ds: &Dataset) -> Result<HashSet<String>> {
    let batches = dataset::scan_rows(ds, &["id"], None, None).await?;
    let mut ids = HashSet::new();
    for batch in batches {
        if let Some(col) = batch
            .column_by_name("id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        {
            for i in 0..batch.num_rows() {
                ids.insert(col.value(i).to_string());
            }
        }
    }
    Ok(ids)
}

/// Redact secrets from a session's turns *before* chunking — so a long key that chunking would
/// split across pieces is whole when scanned, and never reaches the embedding, the local store, or
/// (via push) the Hub. Best-effort: removes a secret whose value byte-matches the stored text (the
/// common case, real newlines); anything that resists is caught downstream by the fail-closed push
/// gate. Reports to stderr what it removed.
fn redact_turns(turns: &mut [trace::Turn], scanner: &dyn scan::SecretScanner) -> Result<()> {
    let texts: Vec<String> = turns
        .iter()
        .flat_map(|t| t.blocks.iter().map(|b| b.text.clone()))
        .collect();
    if texts.is_empty() {
        return Ok(());
    }
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let per_block = scan::scan_blocks(&refs, scanner)?;

    let mut removed: Vec<String> = Vec::new();
    let redacted: Vec<String> = texts
        .iter()
        .zip(&per_block)
        .map(|(text, findings)| {
            let r = scan::excise(text, findings);
            removed.extend(r.removed_detectors);
            r.text
        })
        .collect();
    if removed.is_empty() {
        return Ok(());
    }
    let mut it = redacted.into_iter();
    for t in turns.iter_mut() {
        for b in t.blocks.iter_mut() {
            b.text = it.next().expect("one redacted text per block");
        }
    }
    let sid = turns.first().map(|t| t.session_id.as_str()).unwrap_or("?");
    eprintln!(
        "    redacted {} secret(s) in {sid}: {}",
        removed.len(),
        scan::summary(removed.iter().map(String::as_str))
    );
    Ok(())
}

/// A unit's distinct-session count and a log label: `"<sid> (<project>)"` for a single session (a
/// JSONL file), `"<n> sessions"` for a bulk unit (many sessions in one artifact), and the unit's `key` (its
/// path) when it has no turns at all. The borrow of `turns` is confined here so callers keep it mutable.
fn unit_summary(turns: &[trace::Turn], key: &str) -> (u64, String) {
    let mut sids: Vec<&str> = turns.iter().map(|t| t.session_id.as_str()).collect();
    sids.sort_unstable();
    sids.dedup();
    let label = match (sids.len(), turns.first()) {
        (0, _) => key.to_string(),
        (1, Some(t)) => format!("{} ({})", t.session_id, t.project),
        (n, _) => format!("{n} sessions"),
    };
    (sids.len() as u64, label)
}

/// The run-wide indexing state: the local store uri, the dataset (created on the first write), the
/// embedder, and the optional redaction scanner. Bundling it lets each unit be indexed by a method
/// instead of threading the same handles through every call.
struct Indexer<'a> {
    uri: &'a str,
    ds: Option<Dataset>,
    embedder: TextEmbedding,
    scanner: Option<&'a dyn scan::SecretScanner>,
    include_thinking: bool,
}

impl Indexer<'_> {
    /// Index one unit's turns: redact, chunk, keep only chunks whose id isn't already stored, and
    /// embed those in a single append. Returns `(sessions read, new chunks added)`. `key` names the
    /// unit in logs (and is the label when the unit has no sessions).
    ///
    /// Add-only-new: a grown session is the same memory — embed and add only its new turns, never
    /// re-embedding or deleting what's unchanged. (A rewritten turn lands under new ids.) A unit's
    /// turns are written in one append, so a bulk source (many sessions in one unit) stays a
    /// single Lance fragment rather than one per session.
    async fn index_unit(&mut self, progress: &str, key: &str, mut turns: Vec<trace::Turn>) -> Result<(u64, u64)> {
        let (sessions, label) = unit_summary(&turns, key);

        if let Some(scanner) = self.scanner {
            redact_turns(&mut turns, scanner)?;
        }
        let chunks = chunk::chunks_from_turns(&turns, self.include_thinking);
        let total_chunks = chunks.len();
        if total_chunks == 0 {
            eprintln!("{progress} {label} — no indexable content");
            return Ok((sessions, 0));
        }

        let existing = match &self.ds {
            Some(d) => stored_ids(d).await?,
            None => HashSet::new(),
        };
        let new_chunks: Vec<chunk::Chunk> = chunks.into_iter().filter(|c| !existing.contains(&c.id)).collect();
        if new_chunks.is_empty() {
            eprintln!("{progress} {label} — {total_chunks} chunks, all already indexed");
            return Ok((sessions, 0));
        }

        eprintln!("{progress} {label} — {} new of {total_chunks} chunks", new_chunks.len());
        let added = self.embed_and_write(&new_chunks).await?;
        Ok((sessions, added))
    }

    /// Embed `new_chunks` and add them — appending to the dataset, or creating it at `uri` on the
    /// first write. Returns the count.
    async fn embed_and_write(&mut self, new_chunks: &[chunk::Chunk]) -> Result<u64> {
        let n = new_chunks.len();
        let texts: Vec<&str> = new_chunks.iter().map(|c| c.text.as_str()).collect();
        let t0 = Instant::now();
        let vectors = embed_batched(&mut self.embedder, &texts, |done| {
            let secs = t0.elapsed().as_secs_f64().max(0.001);
            eprint!("\r    embedded {done}/{n}  ({:.0}/s)   ", done as f64 / secs);
            let _ = std::io::stderr().flush();
        })?;
        eprintln!(
            "\r    embedded {n} chunks in {:.1}s          ",
            t0.elapsed().as_secs_f64()
        );

        let batch = build_batch(new_chunks, &vectors)?;
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema());
        let uri = self.uri;
        match &mut self.ds {
            Some(d) => {
                d.append(reader, None).await?;
            }
            None => {
                self.ds = Some(Dataset::write(reader, uri, Some(WriteParams::default())).await?);
            }
        }
        Ok(n as u64)
    }
}

/// Build/update the local index from one or more source roots — each `(path, harness override)`
/// where `None` auto-detects. All roots share one store, embedder, and `state.json` (keyed by
/// absolute file path, so cross-root incremental works). Writes only locally — publishing is the
/// separate `push`. `max_sessions` caps sessions *per root* (`None` = all) — the CLI passes `None`;
/// a benchmark passes a cap to bound build time.
pub async fn run_index_roots(
    roots: &[(PathBuf, Option<Harness>)],
    no_thinking: bool,
    max_sessions: Option<usize>,
) -> Result<()> {
    let include_thinking = !no_thinking;
    let dir = dataset::funes_dir();
    std::fs::create_dir_all(&dir)?;

    let uri = dataset::table_uri(&dataset::local_store_dir());
    let ds = dataset::open(&uri, HashMap::new()).await.ok();

    // Model-pin: refuse to add to a store built with a different embedding model. The id rides
    // in the dataset's schema metadata; a pre-metadata store (no id) is tolerated and guarded only
    // by the dimension check until it is reindexed.
    if let Some(ds) = &ds {
        let schema = arrow_schema::Schema::from(ds.schema());
        if let Some(em) = schema.metadata().get("embedding_model") {
            if em != MODEL {
                return Err(anyhow!("index built with model {em:?}, refusing to mix with {MODEL:?}"));
            }
        }
    }

    // Incremental state: path -> "size:mtime".
    let state_path = dir.join("state.json");
    let mut state: HashMap<String, String> = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let embedder = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))?;
    // Best-effort secret redaction: if the scanner isn't installed, indexing continues unredacted —
    // the push gate still scans, fail-closed, before any upload, so a secret can't reach the Hub.
    let scanner = match scan::Trufflehog::find() {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("note: secret redaction disabled — {e}");
            None
        }
    };
    let scanner = scanner.as_ref().map(|s| s as &dyn scan::SecretScanner);

    let mut indexer = Indexer {
        uri: &uri,
        ds,
        embedder,
        scanner,
        include_thinking,
    };
    let (mut n_sessions, mut n_skipped, mut n_chunks) = (0u64, 0u64, 0u64);
    for (path, harness) in roots {
        let src = source::open_with_harness(path, max_sessions, *harness);
        let units = src.units()?;
        let total = units.len();
        let cached = units
            .iter()
            .filter(|u| u.signature.as_ref().is_some_and(|s| state.get(&u.key) == Some(s)))
            .count();
        eprintln!("{} — {} to index, {cached} cached", src.describe(), total - cached);

        for (idx, unit) in units.iter().enumerate() {
            // Skip a unit only if it carries a signature that still matches what's recorded; a
            // signature-less (bulk) unit has none, so it's never skipped here — always re-read (its
            // chunk-id dedup makes a re-run a no-op).
            if let Some(sig) = &unit.signature {
                if state.get(&unit.key) == Some(sig) {
                    n_skipped += 1;
                    continue;
                }
            }
            let progress = format!("[{}/{}]", idx + 1, total);
            let turns = match src.read(unit) {
                Ok(t) => t,
                // Best-effort sources retry a failed read next run (no state recorded); a fatal
                // source aborts rather than silently dropping data.
                Err(e) if !src.fatal_on_read_error() => {
                    eprintln!("{progress} {} — read failed, skipping: {e}", unit.key);
                    continue;
                }
                Err(e) => return Err(e),
            };
            let (sessions, added) = indexer.index_unit(&progress, &unit.key, turns).await?;
            n_sessions += sessions;
            n_chunks += added;

            // Record state only for signed units, even when they produced no chunks ("remembered
            // when empty"), and persist after each so an interrupted run is resumable.
            // Signature-less (bulk) units are never recorded: a re-run re-reads and dedups to a no-op.
            if let Some(sig) = &unit.signature {
                state.insert(unit.key.clone(), sig.clone());
                std::fs::write(&state_path, serde_json::to_string_pretty(&state)?)?;
            }
        }
    }

    // Build the FTS + IVF_PQ indexes (best-effort). The vector index bounds how much a query reads,
    // which is what makes recall over a remote (hf://) tier lazy instead of a full-column scan; lance
    // enforces its own training minimum (256 rows for default IVF_PQ) and skips below it, so recall
    // falls back to brute-force vector search.
    if let Some(d) = &mut indexer.ds {
        dataset::build_indexes(d, |phase| eprintln!("building {phase}…")).await;

        // Reap superseded versions — best-effort; on failure the reap waits for next run.
        match d.cleanup_old_versions(chrono::Duration::minutes(10), None, None).await {
            Ok(stats) if stats.bytes_removed > 0 => eprintln!(
                "reclaimed {:.1} MB from {} old version(s)",
                stats.bytes_removed as f64 / 1e6,
                stats.old_versions
            ),
            Ok(_) => {}
            Err(e) => eprintln!("note: version cleanup skipped — {e}"),
        }
    }

    println!("indexed sessions={n_sessions} skipped={n_skipped} chunks={n_chunks}");

    Ok(())
}

/// Build/update the local index from a single source root, auto-detecting its harness — a thin
/// convenience over [`run_index_roots`] for a single path (tests, benchmarks, one explicit path).
pub async fn run_index(path: &Path, no_thinking: bool, max_sessions: Option<usize>) -> Result<()> {
    run_index_roots(&[(path.to_path_buf(), None)], no_thinking, max_sessions).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_column_order_is_load_bearing() {
        // Column order must match build_batch's array order exactly, or Lance writes the
        // wrong column. Pin it so a reorder can't slip through.
        let s = schema();
        let names: Vec<&str> = s.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(
            names,
            vec![
                "id",
                "text",
                "session_id",
                "project",
                "turn_uuid",
                "parent_uuid",
                "seq",
                "ts",
                "role",
                "block_type",
                "tool_name",
                "source_path",
                "block_idx",
                "split_idx",
                "vector",
                "harness",
            ]
        );
    }

    #[test]
    fn redact_turns_replaces_secrets_in_block_text() {
        struct Fake(Vec<scan::Finding>);
        impl scan::SecretScanner for Fake {
            fn scan(&self, _blob: &str) -> Result<Vec<scan::Finding>> {
                Ok(self.0.clone())
            }
        }
        let scanner = Fake(vec![
            scan::Finding {
                detector: "PrivateKey".into(),
                raw: "TOPSECRET".into(),
                line: None,
                decoder: "PLAIN".into(),
            },
            scan::Finding {
                detector: "VirusTotal".into(),
                raw: "cafef00d".into(),
                line: None,
                decoder: "PLAIN".into(),
            },
        ]);
        let mut turns = vec![trace::Turn {
            session_id: "sess".into(),
            project: "proj".into(),
            turn_uuid: "turn".into(),
            parent_uuid: None,
            seq: 0,
            ts: String::new(),
            role: "user".into(),
            blocks: vec![trace::Block {
                block_type: "text".into(),
                text: "key=TOPSECRET hash=cafef00d".into(),
                tool_name: None,
                tool_use_id: None,
            }],
            source_path: String::new(),
            harness: "claude_code".into(),
        }];
        redact_turns(&mut turns, &scanner).unwrap();
        assert_eq!(
            turns[0].blocks[0].text,
            "key=[REDACTED:PrivateKey] hash=[REDACTED:VirusTotal]"
        );
    }
}
