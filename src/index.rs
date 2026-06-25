//! The `index` command: walk transcripts → parse → chunk → embed → write to a local Lance dataset.
//! Incremental on two levels: skip unchanged files (size:mtime in state.json), and within a
//! changed session add only chunks whose id is new — a grown session (the same memory)
//! contributes just its new turns, nothing is re-embedded or deleted.

use crate::{chunk, config, dataset, hub, parse, push, scan};
use anyhow::{anyhow, Result};
use arrow_array::types::Float32Type;
use arrow_array::{Array, BooleanArray, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema};
use arrow_select::filter::filter_record_batch;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use lance::dataset::{Dataset, WriteMode, WriteParams};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

pub const MODEL: &str = "BAAI/bge-small-en-v1.5";
pub const DIM: i32 = 384;
const EMBED_BATCH: usize = 256;

/// The table schema (column order is load-bearing for Lance).
fn schema() -> Arc<Schema> {
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
        ],
    )?)
}

/// Embed `texts` in batches of [`EMBED_BATCH`], calling `on_batch(embedded_so_far)` after each —
/// shared by `run_index` (which draws a progress line) and `run_scrub` (which passes a no-op).
fn embed_batched(
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

/// The chunk ids already stored for `session_id`. Re-indexing keeps only the chunks whose id
/// isn't here, so a grown session (the same memory) contributes just its new turns — nothing is
/// re-embedded or deleted. (A rewritten turn arrives under new ids, i.e. as another memory.)
async fn existing_ids(ds: &Dataset, session_id: &str) -> Result<HashSet<String>> {
    let filter = format!("session_id = '{}'", session_id.replace('\'', "''"));
    let batches = dataset::scan_rows(ds, &["id"], Some(filter.as_str()), None).await?;
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

/// The index-time preprocessors, run over a session's turns before chunking. Secret redaction is
/// best-effort: if the scanner isn't installed, indexing continues unredacted — the push gate still
/// scans, fail-closed, before any upload, so a secret can't reach the Hub even if it lands in the
/// local store.
fn build_preprocessors() -> Vec<Box<dyn scan::Preprocessor>> {
    match scan::Trufflehog::find() {
        Ok(scanner) => vec![Box::new(scan::RedactSecrets::new(Box::new(scanner)))],
        Err(e) => {
            eprintln!("note: secret redaction disabled — {e}");
            Vec::new()
        }
    }
}

/// "size:mtime_secs" for incremental skip, or None if the file can't be stat'd.
fn file_sig(p: &Path) -> Option<String> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}:{}", md.len(), mtime))
}

pub async fn run_index(source: &Path, no_thinking: bool) -> Result<()> {
    let include_thinking = !no_thinking;
    let dir = dataset::funes_dir();
    std::fs::create_dir_all(&dir)?;

    let uri = dataset::table_uri(&dataset::local_store_dir());
    let mut ds = dataset::open(&uri, HashMap::new()).await.ok();

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

    let files = parse::iter_jsonl_files(source);
    let total = files.len();
    let cached = files
        .iter()
        .filter(|p| file_sig(p).is_some_and(|s| state.get(&p.to_string_lossy().into_owned()) == Some(&s)))
        .count();
    eprintln!(
        "scanning {total} transcripts under {} — {cached} cached, {} to index",
        source.display(),
        total - cached
    );
    let mut embedder = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))?;
    let preprocessors = build_preprocessors();

    let (mut n_files, mut n_skipped, mut n_chunks) = (0u64, 0u64, 0u64);
    for (idx, p) in files.iter().enumerate() {
        let sig = match file_sig(p) {
            Some(s) => s,
            None => continue,
        };
        let key = p.to_string_lossy().into_owned();
        if state.get(&key) == Some(&sig) {
            n_skipped += 1;
            continue;
        }

        let sid = parse::session_id_of(p);
        let project = parse::project_of(p);
        let mut turns = match parse::turns_from_jsonl_file(p, &sid, &project) {
            Ok(t) => t,
            Err(e) => {
                // Don't record state, so a transient read failure is retried next run.
                eprintln!("[{}/{}] {sid} — read failed, skipping: {e}", idx + 1, total);
                continue;
            }
        };
        // Redact secrets (and any other preprocessors) on the contiguous block text before chunking,
        // so a long secret split() would otherwise cut across chunks is whole when scanned — and so
        // it never reaches the embedding, the local store, or, via push, the Hub.
        for pp in &preprocessors {
            pp.process(&mut turns)?;
        }
        let chunks = chunk::chunks_from_turns(&turns, include_thinking);
        let total_chunks = chunks.len();

        if total_chunks == 0 {
            eprintln!("[{}/{}] {sid} — no indexable content", idx + 1, total);
        } else {
            // Add-only-new: keep just the chunks whose id isn't already stored for this session.
            // A grown session is the same memory — embed and add only its new turns, never
            // re-embedding or deleting what's unchanged. (A rewritten turn lands under new ids.)
            let existing = match &ds {
                Some(d) => existing_ids(d, &sid).await?,
                None => HashSet::new(),
            };
            let new_chunks: Vec<chunk::Chunk> = chunks.into_iter().filter(|c| !existing.contains(&c.id)).collect();

            if new_chunks.is_empty() {
                eprintln!(
                    "[{}/{}] {sid} ({project}) — {total_chunks} chunks, all already indexed",
                    idx + 1,
                    total
                );
            } else {
                let n = new_chunks.len();
                eprintln!(
                    "[{}/{}] {sid} ({project}) — {n} new of {total_chunks} chunks",
                    idx + 1,
                    total
                );
                let texts: Vec<&str> = new_chunks.iter().map(|c| c.text.as_str()).collect();
                let t0 = Instant::now();
                let vectors = embed_batched(&mut embedder, &texts, |done| {
                    let secs = t0.elapsed().as_secs_f64().max(0.001);
                    eprint!("\r    embedded {done}/{n}  ({:.0}/s)   ", done as f64 / secs);
                    let _ = std::io::stderr().flush();
                })?;
                eprintln!(
                    "\r    embedded {n} chunks in {:.1}s          ",
                    t0.elapsed().as_secs_f64()
                );

                let batch = build_batch(&new_chunks, &vectors)?;
                let reader = RecordBatchIterator::new(vec![Ok(batch)], schema());
                match &mut ds {
                    Some(d) => {
                        d.append(reader, None).await?;
                    }
                    None => {
                        ds = Some(Dataset::write(reader, &uri, Some(WriteParams::default())).await?);
                    }
                }
                n_chunks += n as u64;
            }
        }
        state.insert(key, sig); // remembered even when empty
                                // Persist after each file so progress survives interruption (a kill is resumable).
        std::fs::write(&state_path, serde_json::to_string_pretty(&state)?)?;
        n_files += 1;
    }

    // Build the FTS + IVF_PQ indexes (best-effort). The vector index bounds how much a query reads,
    // which is what makes recall over a remote (hf://) tier lazy instead of a full-column scan; lance
    // enforces its own training minimum (256 rows for default IVF_PQ) and skips below it, so recall
    // falls back to brute-force vector search.
    if let Some(d) = &mut ds {
        eprintln!("building FTS + IVF_PQ indexes…");
        dataset::build_indexes(d).await;
    }

    println!("indexed files={n_files} skipped={n_skipped} chunks={n_chunks}");

    // Publish to the attached remote, best-effort: a failed push must not fail the local index.
    if let Some(remote) = config::load().remote {
        match push::run_push(hub::Store::parse(&remote), false).await {
            Ok(report) => print!("{report}"),
            Err(e) if push::is_read_only(&e) => {
                eprintln!("indexed locally; {remote} is read-only for your token — recall reads it, but publishing needs write access")
            }
            Err(e) => eprintln!("indexed locally; couldn't publish to {remote}: {e}"),
        }
    }

    Ok(())
}

/// Redact secrets from the existing store. Reconstructs each block, scans them all in one pass, and
/// for any block holding a secret either redacts the matching values in place (re-chunked, re-embedded)
/// or — when a value can't be matched (e.g. a key stored with escaped `\n`) — drops the block whole.
/// Works on the rows themselves — so it cleans sessions whose source transcripts are already gone,
/// which re-indexing cannot. Fail-closed on the scanner: scrubbing is the whole point.
pub async fn run_scrub() -> Result<()> {
    let uri = dataset::table_uri(&dataset::local_store_dir());
    let Ok(ds) = dataset::open(&uri, HashMap::new()).await else {
        println!("no local index to scrub");
        return Ok(());
    };
    let scanner = scan::Trufflehog::find()?;

    let batches = dataset::scan_rows(&ds, &[], None, None).await?;
    let chunks = chunks_from_batches(&batches);
    let total = chunks.len();
    if total == 0 {
        println!("store is empty");
        return Ok(());
    }

    // Reconstruct each block's contiguous text (so a secret split() cut across chunks is whole) and
    // scan them all in one pass.
    let blocks = chunk::reconstruct_blocks(&chunks);
    let texts: Vec<&str> = blocks.iter().map(|(_, text)| text.as_str()).collect();
    let found = scan::scan_blocks(&texts, &scanner)?;

    // From that single scan, decide each block: excise the secrets whose value matches and re-chunk
    // the result; if any can't be matched, the block can't be safely redacted, so drop it whole.
    // `remove[i]` marks an original row to drop (its block had a secret); `replacements` are the
    // re-chunked redacted blocks to re-embed.
    let mut remove = vec![false; chunks.len()];
    let mut replacements: Vec<chunk::Chunk> = Vec::new();
    let mut redacted_detectors: Vec<String> = Vec::new();
    let mut dropped_detectors: Vec<String> = Vec::new();
    let (mut redacted_blocks, mut dropped_blocks, mut dropped_rows) = (0usize, 0usize, 0usize);
    for ((idxs, text), findings) in blocks.iter().zip(&found) {
        if findings.is_empty() {
            continue;
        }
        for &i in idxs {
            remove[i] = true;
        }
        let (redacted_text, removed, all_removed) = scan::excise(text, findings);
        if all_removed {
            replacements.extend(chunk::resplit(&chunks[idxs[0]], &redacted_text));
            redacted_detectors.extend(removed);
            redacted_blocks += 1;
        } else {
            dropped_detectors.extend(scan::detectors(findings));
            dropped_blocks += 1;
            dropped_rows += idxs.len();
        }
    }
    if !remove.iter().any(|&r| r) {
        println!("store is already clean ({total} chunks)");
        return Ok(());
    }

    // Re-embed only the redacted replacement rows — their stored vectors were computed from the
    // secret-bearing text. Clean rows keep their existing vectors (carried below).
    let replacement_batch = if replacements.is_empty() {
        None
    } else {
        let mut embedder = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))?;
        let rtexts: Vec<&str> = replacements.iter().map(|c| c.text.as_str()).collect();
        let vectors = embed_batched(&mut embedder, &rtexts, |_| {})?;
        Some(build_batch(&replacements, &vectors)?)
    };

    // Rewrite the store in a single Overwrite commit: every clean row (with its existing vector) plus
    // the re-chunked redacted blocks. One commit is deliberate — a delete-then-append is two commits,
    // and an interrupt between them would drop the secret rows without writing their replacements,
    // which for a source-gone session is permanent loss. Append-first isn't a safe alternative either:
    // `cid` hashes only coordinates, so a same-piece-count re-split reuses the old ids and a later
    // delete couldn't tell the fresh rows from the stale ones. (Cost: scrub rewrites the whole table,
    // fine for a rare remediation.)
    let schema = schema();
    let mut out: Vec<RecordBatch> = Vec::new();
    let mut base = 0usize;
    for b in &batches {
        let mask: BooleanArray = (0..b.num_rows()).map(|i| !remove[base + i]).collect();
        let kept = filter_record_batch(b, &mask)?;
        if kept.num_rows() > 0 {
            out.push(RecordBatch::try_new(schema.clone(), kept.columns().to_vec())?);
        }
        base += b.num_rows();
    }
    out.extend(replacement_batch);
    let reader = RecordBatchIterator::new(out.into_iter().map(Ok), schema.clone());
    let mut ds = Dataset::write(
        reader,
        &uri,
        Some(WriteParams {
            mode: WriteMode::Overwrite,
            ..Default::default()
        }),
    )
    .await?;
    dataset::build_indexes(&mut ds).await;

    let mut msg = format!(
        "scrubbed {total} rows: redacted {} secret(s) in {redacted_blocks} block(s)",
        redacted_detectors.len()
    );
    if dropped_blocks > 0 {
        let _ = write!(
            msg,
            "; dropped {dropped_rows} row(s) in {dropped_blocks} block(s) that couldn't be safely redacted ({})",
            scan::summary(dropped_detectors.iter().map(String::as_str))
        );
    }
    println!("{msg}");
    Ok(())
}

/// Reconstruct [`chunk::Chunk`]s from stored rows (all columns), so the store can be rewritten
/// without its source. The `vector` column is dropped — redacted rows are re-embedded.
pub(crate) fn chunks_from_batches(batches: &[RecordBatch]) -> Vec<chunk::Chunk> {
    let sv = |b: &RecordBatch, name: &str, i: usize| -> String {
        b.column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .map(|c| c.value(i).to_string())
            .unwrap_or_default()
    };
    let so = |b: &RecordBatch, name: &str, i: usize| -> Option<String> {
        b.column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .filter(|c| !c.is_null(i))
            .map(|c| c.value(i).to_string())
    };
    let iv = |b: &RecordBatch, name: &str, i: usize| -> i64 {
        b.column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
            .map(|c| c.value(i))
            .unwrap_or(0)
    };
    let mut out = Vec::new();
    for b in batches {
        for i in 0..b.num_rows() {
            out.push(chunk::Chunk {
                id: sv(b, "id", i),
                text: sv(b, "text", i),
                session_id: sv(b, "session_id", i),
                project: sv(b, "project", i),
                turn_uuid: sv(b, "turn_uuid", i),
                parent_uuid: so(b, "parent_uuid", i),
                seq: iv(b, "seq", i),
                ts: sv(b, "ts", i),
                role: sv(b, "role", i),
                block_type: sv(b, "block_type", i),
                tool_name: so(b, "tool_name", i),
                source_path: sv(b, "source_path", i),
                block_idx: iv(b, "block_idx", i),
                split_idx: iv(b, "split_idx", i),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn file_sig_is_len_colon_mtime() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"hello").unwrap();
        f.flush().unwrap();
        let sig = file_sig(f.path()).expect("stat-able file has a signature");
        let (len, mtime) = sig.split_once(':').expect("sig is len:mtime");
        assert_eq!(len, "5");
        assert!(mtime.parse::<u64>().is_ok());
    }

    #[test]
    fn file_sig_is_none_for_missing_file() {
        assert!(file_sig(Path::new("/no/such/file")).is_none());
    }

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
            ]
        );
    }
}
