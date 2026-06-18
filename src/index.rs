//! The `index` command: walk transcripts → parse → chunk → embed → write to lancedb.
//! Incremental (size:mtime signature in state.json), idempotent per session
//! (delete-by-session then add).

use crate::{chunk, db, parse};
use anyhow::{anyhow, Result};
use arrow_array::types::Float32Type;
use arrow_array::{FixedSizeListArray, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use lancedb::index::scalar::FtsIndexBuilder;
use lancedb::index::Index;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

const MODEL: &str = "BAAI/bge-small-en-v1.5";
const DIM: i32 = 384;
const EMBED_BATCH: usize = 256;

/// The table schema (column order is load-bearing for Lance).
fn schema() -> Arc<Schema> {
    let utf8 = |name: &str| Field::new(name, DataType::Utf8, true);
    let i64f = |name: &str| Field::new(name, DataType::Int64, true);
    Arc::new(Schema::new(vec![
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
    ]))
}

fn build_batch(chunks: &[chunk::Chunk], vectors: &[Vec<f32>]) -> Result<RecordBatch> {
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

/// "size:mtime_secs" for incremental skip, or None if the file can't be stat'd.
fn file_sig(p: &Path) -> Option<String> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}:{}", md.len(), mtime))
}

pub async fn run_index(source: &Path, no_thinking: bool) -> Result<()> {
    let include_thinking = !no_thinking;
    let dir = db::funes_dir();
    std::fs::create_dir_all(&dir)?;

    // Model-pin guard: refuse to mix embedding models in one index.
    let meta_path = dir.join("meta.json");
    let meta: Option<serde_json::Value> = std::fs::read_to_string(&meta_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    if let Some(em) = meta
        .as_ref()
        .and_then(|m| m.get("embedding_model"))
        .and_then(|x| x.as_str())
    {
        if em != MODEL {
            return Err(anyhow!("index built with model {em:?}, refusing to mix with {MODEL:?}"));
        }
    }

    let conn = db::open_db().await?;
    let mut table_exists = conn.table_names().execute().await?.iter().any(|t| t == db::TABLE);

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
        let turns = match parse::turns_from_jsonl_file(p, &sid, &project) {
            Ok(t) => t,
            Err(e) => {
                // Don't record state, so a transient read failure is retried next run.
                eprintln!("[{}/{}] {sid} — read failed, skipping: {e}", idx + 1, total);
                continue;
            }
        };
        let chunks = chunk::chunks_from_turns(&turns, include_thinking);

        if !chunks.is_empty() {
            let n = chunks.len();
            eprintln!("[{}/{}] {sid} ({project}) — {n} chunks", idx + 1, total);
            let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
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

            let batch = build_batch(&chunks, &vectors)?;
            if table_exists {
                let t = conn.open_table(db::TABLE).execute().await?;
                t.delete(&format!("session_id = '{sid}'")).await?;
                t.add(batch).execute().await?;
            } else {
                conn.create_table(db::TABLE, batch).execute().await?;
                table_exists = true;
            }
            n_chunks += n as u64;
        } else {
            eprintln!("[{}/{}] {sid} — no indexable content", idx + 1, total);
        }
        state.insert(key, sig); // remembered even when empty
                                // Persist after each file so progress survives interruption (a kill is resumable).
        std::fs::write(&state_path, serde_json::to_string_pretty(&state)?)?;
        n_files += 1;
    }

    if meta.is_none() {
        let m = serde_json::json!({"embedding_model": MODEL, "dim": DIM, "metric": "cosine"});
        std::fs::write(&meta_path, serde_json::to_string_pretty(&m)?)?;
    }

    if table_exists {
        eprintln!("building BM25 full-text index…");
        let t = conn.open_table(db::TABLE).execute().await?;
        if let Err(e) = t
            .create_index(&["text"], Index::FTS(FtsIndexBuilder::default()))
            .execute()
            .await
        {
            eprintln!("  (fts index skipped: {e})");
        }
    }

    println!("indexed files={n_files} skipped={n_skipped} chunks={n_chunks}");
    Ok(())
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
