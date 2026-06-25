//! The `index` command: walk transcripts → parse → chunk → embed → write to a local Lance dataset.
//! Incremental on two levels: skip unchanged files (size:mtime in state.json), and within a
//! changed session add only chunks whose id is new — a grown session (the same memory)
//! contributes just its new turns, nothing is re-embedded or deleted.

use crate::{chunk, config, dataset, hub, parse, preprocess, push, scan};
use anyhow::{anyhow, Result};
use arrow_array::types::Float32Type;
use arrow_array::{Array, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use lance::dataset::{Dataset, WriteParams};
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
fn build_preprocessors() -> Vec<Box<dyn preprocess::Preprocessor>> {
    match scan::Trufflehog::find() {
        Ok(scanner) => vec![Box::new(preprocess::RedactSecrets::new(Box::new(scanner)))],
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
                let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(n);
                for group in texts.chunks(EMBED_BATCH) {
                    vectors.extend(embedder.embed(group, None)?);
                    let secs = t0.elapsed().as_secs_f64().max(0.001);
                    eprint!(
                        "\r    embedded {}/{n}  ({:.0}/s)   ",
                        vectors.len(),
                        vectors.len() as f64 / secs
                    );
                    let _ = std::io::stderr().flush();
                }
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

/// Redact secrets from the existing store in place. Scans every stored row; for any whose text
/// still holds a secret, redacts the text, re-embeds it, and rewrites the row (delete + append).
/// Works on the rows themselves — so it cleans sessions whose source transcripts are already gone,
/// which re-indexing cannot. Fail-closed on the scanner: scrubbing is the whole point.
pub async fn run_scrub() -> Result<()> {
    let uri = dataset::table_uri(&dataset::local_store_dir());
    let Ok(mut ds) = dataset::open(&uri, HashMap::new()).await else {
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

    // Work block by block: reconstruct each block's contiguous text (so a secret split() cut across
    // chunks is whole), then redact across all blocks in one scan.
    let blocks = chunk::group_blocks(&chunks);
    let original: Vec<String> = blocks
        .iter()
        .map(|idxs| {
            let pieces: Vec<&str> = idxs.iter().map(|&i| chunks[i].text.as_str()).collect();
            chunk::reconstruct(&pieces)
        })
        .collect();
    let mut redacted = original.clone();
    let report = scan::redact(&mut redacted, &scanner)?;

    // Verify: a secret whose bytes differ from trufflehog's canonical form (e.g. escaped `\n`) can't
    // be excised by `redact`'s value match, so it survives. `locate` finds it by line, not value;
    // any block still flagged can't be safely redacted, so it's dropped whole.
    let refs: Vec<&str> = redacted.iter().map(String::as_str).collect();
    let unredactable = scan::locate(&refs, &scanner)?;

    // delete_ids: rows to remove (a block that changed, or one dropped whole). replacements: the
    // re-chunked clean blocks to re-embed and append in their place.
    let mut delete_ids: Vec<String> = Vec::new();
    let mut replacements: Vec<chunk::Chunk> = Vec::new();
    let (mut redacted_blocks, mut dropped_blocks, mut dropped_rows) = (0usize, 0usize, 0usize);
    let mut dropped_detectors: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (b, idxs) in blocks.iter().enumerate() {
        if !unredactable[b].is_empty() {
            for &i in idxs {
                delete_ids.push(chunks[i].id.clone());
            }
            for d in &unredactable[b] {
                *dropped_detectors.entry(d.clone()).or_default() += 1;
            }
            dropped_blocks += 1;
            dropped_rows += idxs.len();
        } else if redacted[b] != original[b] {
            for &i in idxs {
                delete_ids.push(chunks[i].id.clone());
            }
            replacements.extend(chunk::resplit(&chunks[idxs[0]], &redacted[b]));
            redacted_blocks += 1;
        }
    }
    if delete_ids.is_empty() {
        println!("store is already clean ({total} chunks)");
        return Ok(());
    }

    // Re-embed the replacement rows — their stored vectors were computed from secret-bearing text.
    let new_batch = if replacements.is_empty() {
        None
    } else {
        let mut embedder = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))?;
        let texts: Vec<&str> = replacements.iter().map(|c| c.text.as_str()).collect();
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(replacements.len());
        for group in texts.chunks(EMBED_BATCH) {
            vectors.extend(embedder.embed(group, None)?);
        }
        Some(build_batch(&replacements, &vectors)?)
    };

    // Delete by id (coordinate-based, stable), append the redacted re-chunked rows, rebuild indexes.
    let ids = delete_ids
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(", ");
    ds.delete(&format!("id IN ({ids})")).await?;
    if let Some(batch) = new_batch {
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema());
        ds.append(reader, None).await?;
    }
    dataset::build_indexes(&mut ds).await;

    let mut msg = format!(
        "scrubbed {total} rows: redacted {} secret(s) in {redacted_blocks} block(s)",
        report.len()
    );
    if dropped_blocks > 0 {
        let summary = dropped_detectors
            .iter()
            .map(|(d, n)| format!("{d}×{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(
            msg,
            "; dropped {dropped_rows} row(s) in {dropped_blocks} block(s) that couldn't be safely redacted ({summary})"
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
