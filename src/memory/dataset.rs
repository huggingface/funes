//! Shared memory helpers: the local memory location, the `chunks` table's schema and rows, opening a
//! dataset, plain scans, and building the FTS/IVF indexes. funes's home is `$FUNES_HOME`/`~/.funes` —
//! it holds the incremental state and the local memory at `…/memory` (the `chunks` Lance dataset).

use crate::chunk;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::types::Float32Type;
use arrow_array::{FixedSizeListArray, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::Dataset;
use lance::index::vector::VectorIndexParams;
use lance::index::DatasetIndexExt;
use lance_index::scalar::InvertedIndexParams;
use lance_index::vector::ivf::IvfBuildParams;
use lance_index::vector::pq::PQBuildParams;
use lance_index::IndexType;
use lance_io::object_store::{ObjectStoreParams, WrappingObjectStore};
use lance_linalg::distance::MetricType;

/// The table (Lance dataset) name within a memory.
pub const TABLE: &str = "chunks";

/// The embedding model a memory's vectors are built with, and their width. Pinned in the schema
/// metadata and enforced on open ([`super::Memory::open`]): a memory built with another model
/// can't be queried with funes's embeddings.
pub const MODEL: &str = "BAAI/bge-small-en-v1.5";
pub const DIM: i32 = 384;

/// funes's home directory: `$FUNES_HOME`, else `~/.funes`. Holds the incremental state and the
/// local memory.
pub fn funes_dir() -> PathBuf {
    if let Ok(d) = std::env::var("FUNES_HOME") {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".funes")
}

/// Directory holding the local memory (the `chunks` dataset is at `<dir>/chunks.lance`).
pub fn local_memory_dir() -> String {
    let dir = funes_dir().join("memory");
    // Migrate a pre-rename layout in place: an earlier funes kept the local memory at `<home>/store`.
    // Rename it once, when the current path doesn't exist yet. Best-effort — a failed rename just
    // leaves the old memory unfound, which reads as "no index yet".
    let legacy = funes_dir().join("store");
    if legacy.is_dir() && !dir.exists() {
        let _ = std::fs::rename(&legacy, &dir);
    }
    dir.to_string_lossy().into_owned()
}

/// The `chunks` dataset URI under a memory base (a local directory or a remote URI prefix).
pub fn table_uri(base: &str) -> String {
    format!("{base}/{TABLE}.lance")
}

/// Open the `chunks` dataset at `uri`; `storage_options` carries the backend credentials/revision a
/// remote needs (empty for a local memory).
pub async fn open(uri: &str, storage_options: HashMap<String, String>) -> Result<Dataset> {
    DatasetBuilder::from_uri(uri)
        .with_storage_options(storage_options)
        .load()
        .await
        .context("opening the dataset")
}

/// Open the `chunks` dataset at `uri` with `wrapper` decorating its object store. It is installed
/// before load, so it sees every read Lance issues, including those during load. `storage_options`
/// carries the backend credentials/revision a remote needs; the caller supplies the wrapper.
pub async fn open_wrapped(
    uri: &str,
    storage_options: HashMap<String, String>,
    wrapper: Arc<dyn WrappingObjectStore>,
) -> Result<Dataset> {
    // Order matters: `with_store_params` replaces the params wholesale, so install the wrapper
    // first, then layer the storage options on top (`with_storage_options` merges into them).
    DatasetBuilder::from_uri(uri)
        .with_store_params(ObjectStoreParams {
            object_store_wrapper: Some(wrapper),
            ..Default::default()
        })
        .with_storage_options(storage_options)
        .load()
        .await
        .context("opening the wrapped dataset")
}

/// Project `columns` (empty = all columns; optionally filtered by a SQL predicate, optionally
/// limited) and collect the matching rows. Plain scans aren't limit-capped, so callers pass `None`
/// to read everything.
pub async fn scan_rows(
    ds: &Dataset,
    columns: &[&str],
    filter: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<RecordBatch>> {
    let mut scan = ds.scan();
    if !columns.is_empty() {
        scan.project(columns)?;
    }
    if let Some(f) = filter {
        scan.filter(f)?;
    }
    scan.limit(limit, None)?;
    let mut stream = scan.try_into_stream().await?;
    let mut batches = Vec::new();
    while let Some(batch) = stream.try_next().await? {
        batches.push(batch);
    }
    Ok(batches)
}

/// Best-effort: build the FTS index on `text` and the IVF_PQ index on `vector`. A small corpus
/// can't train IVF (lance needs ~256 rows) — that's fine, recall falls back to brute force.
///
/// `on_phase` is called with a human label before each index is built, so a caller can report
/// progress around these opaque (no incremental hook), potentially slow Lance calls. Pass `|_| {}`
/// to stay silent.
pub async fn build_indexes(ds: &mut Dataset, on_phase: impl Fn(&str)) {
    sweep_shuffle_leftovers(&std::env::temp_dir());
    on_phase("text search index");
    let _ = ds
        .create_index(
            &["text"],
            IndexType::Inverted,
            None,
            &InvertedIndexParams::default(),
            true,
        )
        .await;
    if let Some(params) = ivf_pq_params(ds) {
        on_phase("vector index");
        let _ = ds
            .create_index(&["vector"], IndexType::Vector, None, &params, true)
            .await;
    }
}

/// The files a lance IVF shuffle directory holds, and nothing else.
const SHUFFLE_FILES: [&str; 4] = [
    "shuffle_data.lance",
    "shuffle_data.spill",
    "shuffle_offsets.lance",
    "shuffle_offsets.spill",
];

/// A shuffle directory is fair game only once no build could still be writing to it.
const SHUFFLE_LEFTOVER_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Best-effort: remove the shuffle directories a previous IVF build left in `$TMPDIR`.
///
/// Building the IVF index hands lance a temp directory whose guard lance drops before the shuffler
/// writes to it (`index/vector.rs::prepare_vector_segment_build`), so the writer recreates the
/// directory and nothing owns it — every build leaks one, tens of MB apiece. lance fixed this in
/// 13.0.0; 11 and 12 both leak, so this sweep goes away when funes moves off them. Sweeping
/// on the way in rather than on the way out is what keeps this safe: a directory another process is
/// still filling is younger than [`SHUFFLE_LEFTOVER_AGE`], and one it has finished with holds
/// nothing but [`SHUFFLE_FILES`] (mid-write, object_store's own staging files sit alongside them).
fn sweep_shuffle_leftovers(tmp_root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(tmp_root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(".tmp") {
            continue;
        }
        let path = entry.path();
        if entry.metadata().is_ok_and(|m| {
            m.is_dir()
                && m.modified()
                    .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > SHUFFLE_LEFTOVER_AGE))
        }) && holds_only_shuffle_files(&path)
        {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Whether `dir` is non-empty and every name in it is one of [`SHUFFLE_FILES`].
fn holds_only_shuffle_files(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut seen = false;
    for entry in entries {
        let Ok(entry) = entry else { return false };
        if !SHUFFLE_FILES.contains(&entry.file_name().to_string_lossy().as_ref()) {
            return false;
        }
        seen = true;
    }
    seen
}

/// IVF_PQ parameters sized from the `vector` column's dimension (matching lancedb's defaults).
/// `None` if there is no fixed-size `vector` column.
fn ivf_pq_params(ds: &Dataset) -> Option<VectorIndexParams> {
    let arrow = arrow_schema::Schema::from(ds.schema());
    let arrow_schema::DataType::FixedSizeList(_, dim) = arrow.field_with_name("vector").ok()?.data_type() else {
        return None;
    };
    let dim = *dim as usize;
    let num_sub_vectors = if dim.is_multiple_of(16) {
        dim / 16
    } else if dim.is_multiple_of(8) {
        dim / 8
    } else {
        1
    };
    let mut pq = PQBuildParams::new(num_sub_vectors, 8);
    pq.max_iters = 50;
    Some(VectorIndexParams::with_ivf_pq_params(
        MetricType::L2,
        IvfBuildParams::default(),
        pq,
    ))
}

/// The table schema (column order is load-bearing for Lance).
pub(crate) fn schema() -> Arc<Schema> {
    let utf8 = |name: &str| Field::new(name, DataType::Utf8, true);
    let i64f = |name: &str| Field::new(name, DataType::Int64, true);
    Arc::new(Schema::new_with_metadata(
        vec![
            utf8("id"),
            utf8("text"),
            utf8("session_id"),
            utf8("workdir"),
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
            // After `vector`: `add_columns` appends a migrated column at the end, so a
            // freshly-built memory must match that order (the tripwire test pins it). `harness`
            // came first, then `repo` — each appended in turn.
            utf8("harness"),
            utf8("repo"),
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
            Arc::new(s(&|c| Some(c.workdir.clone()))),
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
            Arc::new(s(&|c| Some(c.repo.clone()))),
        ],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory under `root` holding `files`, backdated by `age`.
    fn shuffle_dir(root: &std::path::Path, name: &str, files: &[&str], age: std::time::Duration) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let when = std::time::SystemTime::now() - age;
        std::fs::File::open(&dir).unwrap().set_modified(when).unwrap();
        dir
    }

    #[test]
    fn sweep_reclaims_only_settled_shuffle_dirs() {
        let hour = SHUFFLE_LEFTOVER_AGE;
        let root = tempfile::tempdir().unwrap();
        let root = root.path();

        let stale = shuffle_dir(root, ".tmpStale", &SHUFFLE_FILES, hour * 2);
        // A build still writing: its shuffle files are there, but so are object_store's staging files.
        let writing = shuffle_dir(root, ".tmpWriting", &["shuffle_data.lance", ".tmpStaging"], hour * 2);
        let fresh = shuffle_dir(root, ".tmpFresh", &SHUFFLE_FILES, std::time::Duration::ZERO);
        let foreign = shuffle_dir(root, ".tmpForeign", &["notes.txt"], hour * 2);
        let empty = shuffle_dir(root, ".tmpEmpty", &[], hour * 2);
        let named = shuffle_dir(root, "scratch", &SHUFFLE_FILES, hour * 2);

        sweep_shuffle_leftovers(root);

        assert!(!stale.exists(), "a settled shuffle dir must be reclaimed");
        for kept in [&writing, &fresh, &foreign, &empty, &named] {
            assert!(kept.exists(), "{} must be left alone", kept.display());
        }
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
                "workdir",
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
                "repo",
            ]
        );
    }
}
