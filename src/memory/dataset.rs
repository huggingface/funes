//! Shared memory helpers: the local memory location, the `chunks` table's schema and rows, opening a
//! dataset, plain scans, and building the FTS/IVF indexes. funes's home is `$FUNES_HOME`/`~/.funes` —
//! it holds the incremental state and the local memory at `…/memory` (the `chunks` Lance dataset).

use crate::chunk;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::types::Float32Type;
use arrow_array::{FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::{Dataset, MergeInsertBuilder, MergeInsertWriteMode, WhenMatched, WhenNotMatched};
use lance::index::vector::VectorIndexParams;
use lance::index::DatasetIndexExt;
use lance_index::optimize::OptimizeOptions;
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

/// lance's default `<column>_idx` names, so an index lance named is refreshed, not rebuilt beside.
pub(crate) const FTS_INDEX: &str = "text_idx";
pub(crate) const VECTOR_INDEX: &str = "vector_idx";

/// Best-effort: build the FTS index on `text` and the IVF_PQ index on `vector`, or refresh one the
/// dataset already has ([`optimize_index`]: milliseconds, against ~30 s for a full rebuild). A small
/// corpus can't train IVF (lance needs ~256 rows) — that's fine, recall falls back to brute force.
///
/// `on_phase` is called with a human label before an index is built whole, so a caller can report
/// progress around these opaque (no incremental hook), potentially slow Lance calls. Pass `|_| {}`
/// to stay silent.
pub async fn build_indexes(ds: &mut Dataset, on_phase: impl Fn(&str)) {
    let existing = sub_index_counts(ds).await.unwrap_or_default();
    match existing.get(FTS_INDEX) {
        Some(&subs) => {
            let _ = optimize_index(ds, FTS_INDEX, subs).await;
        }
        None => {
            on_phase("text search index");
            let _ = ds
                .create_index(
                    &["text"],
                    IndexType::Inverted,
                    Some(FTS_INDEX.to_string()),
                    &InvertedIndexParams::default(),
                    true,
                )
                .await;
        }
    }
    if let Some(params) = ivf_pq_params(ds) {
        match existing.get(VECTOR_INDEX) {
            Some(&subs) => {
                let _ = optimize_index(ds, VECTOR_INDEX, subs).await;
            }
            None => {
                on_phase("vector index");
                let _ = ds
                    .create_index(
                        &["vector"],
                        IndexType::Vector,
                        Some(VECTOR_INDEX.to_string()),
                        &params,
                        true,
                    )
                    .await;
            }
        }
    }
}

/// Fold an index's delta sub-indexes back into one once this many pile up. Queries fan out across
/// every delta (and per-segment BM25 stats drift), so the pile must stay bounded. Only the deltas
/// are merged — the base is never re-read, which would be the full-index rewrite [`optimize_index`]
/// exists to avoid.
pub(crate) const COMPACT_DELTAS: usize = 8;

/// Sub-index count per index name (the base plus its deltas, which share the index's name), from
/// the index metadata — not `index_statistics`, which can write a stats migration through a remote's
/// capture wrapper.
pub(crate) async fn sub_index_counts(ds: &Dataset) -> Result<BTreeMap<String, usize>> {
    let indices = ds.load_indices().await.context("listing the indexes")?;
    let mut counts = BTreeMap::new();
    for idx in indices.iter() {
        *counts.entry(idx.name.clone()).or_default() += 1;
    }
    Ok(counts)
}

/// Add a delta sub-index over the rows appended since `name` was last built, or at
/// [`COMPACT_DELTAS`] deltas merge them into one, sparing the base. `subs` is base + deltas.
/// Returns the deltas folded.
pub(crate) async fn optimize_index(ds: &mut Dataset, name: &str, subs: usize) -> Result<usize> {
    let deltas = subs.saturating_sub(1);
    let (opts, folded) = if deltas >= COMPACT_DELTAS {
        (OptimizeOptions::merge(deltas), deltas)
    } else {
        (OptimizeOptions::append(), 0)
    };
    ds.optimize_indices(&opts.index_names(vec![name.to_string()]))
        .await
        .with_context(|| format!("optimizing {name}"))?;
    Ok(folded)
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

/// `None` writes every row with a null `vector`, for [`fill_vectors`] later.
pub(crate) fn build_batch(chunks: &[chunk::Chunk], vectors: Option<&[Vec<f32>]>) -> Result<RecordBatch> {
    let s = |f: &dyn Fn(&chunk::Chunk) -> Option<String>| -> StringArray { chunks.iter().map(f).collect() };
    let i = |f: &dyn Fn(&chunk::Chunk) -> i64| -> Int64Array { chunks.iter().map(|c| Some(f(c))).collect() };
    let vector = match vectors {
        Some(vectors) => vector_array(vectors),
        None => FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            chunks.iter().map(|_| None::<Vec<Option<f32>>>),
            DIM,
        ),
    };
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

fn vector_array(vectors: &[Vec<f32>]) -> FixedSizeListArray {
    FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        vectors
            .iter()
            .map(|v| Some(v.iter().map(|&x| Some(x)).collect::<Vec<_>>())),
        DIM,
    )
}

/// Fill the vectors of rows written unembedded, in place: `RewriteColumns` rewrites only the vector
/// column of the touched fragments, so no row moves. Fail-closed on an id the dataset doesn't hold —
/// that is a lost row, not a no-op.
pub(crate) async fn fill_vectors(ds: &Dataset, ids: &[&str], vectors: &[Vec<f32>]) -> Result<Dataset> {
    let source_schema = Arc::new(Schema::new(vec![
        schema().field_with_name("id")?.clone(),
        schema().field_with_name("vector")?.clone(),
    ]));
    let source = RecordBatch::try_new(
        source_schema.clone(),
        vec![
            Arc::new(StringArray::from(ids.to_vec())),
            Arc::new(vector_array(vectors)),
        ],
    )?;
    let reader = RecordBatchIterator::new(vec![Ok(source)], source_schema);
    let (ds, stats) = MergeInsertBuilder::try_new(Arc::new(ds.clone()), vec!["id".to_string()])?
        .when_matched(WhenMatched::UpdateAll)
        .when_not_matched(WhenNotMatched::DoNothing)
        .write_mode(MergeInsertWriteMode::RewriteColumns)
        .try_build()?
        .execute_reader(reader)
        .await
        .context("filling vectors")?;
    if stats.num_updated_rows != ids.len() as u64 {
        anyhow::bail!(
            "filled {} vector(s) but {} were given — the memory has lost the other rows",
            stats.num_updated_rows,
            ids.len()
        );
    }
    Ok(Arc::try_unwrap(ds).unwrap_or_else(|shared| (*shared).clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traces::{Block, Turn, FORMAT_VERSION};
    use arrow_array::RecordBatchIterator;
    use lance::dataset::WriteParams;

    /// `n` one-block turns with distinct text, so each is its own chunk.
    fn turns(from: usize, n: usize) -> Vec<Turn> {
        (from..from + n)
            .map(|i| Turn {
                format: FORMAT_VERSION,
                session_id: "sess".into(),
                cwd: None,
                workdir: "proj".into(),
                turn_uuid: format!("turn{i}"),
                parent_uuid: None,
                seq: i as i64,
                ts: "2026-01-01T00:00:00Z".into(),
                role: "assistant".into(),
                blocks: vec![Block {
                    block_type: "text".into(),
                    text: format!("turn {i} about parsing transcripts and lance indexing"),
                    tool_name: None,
                    tool_use_id: None,
                }],
                source_path: "/x.jsonl".into(),
                harness: "claude_code".into(),
            })
            .collect()
    }

    /// Pseudo-random vectors: IVF_PQ can't train on identical ones.
    fn embedded(turns: &[Turn]) -> RecordBatch {
        let chunks = chunk::chunks_from_turns(turns, &chunk::Tier::ALL, true);
        let mut seed = 0x9e37_79b9u32;
        let vectors: Vec<Vec<f32>> = chunks
            .iter()
            .map(|_| {
                (0..DIM)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed >> 8) as f32 / (1u32 << 24) as f32
                    })
                    .collect()
            })
            .collect();
        build_batch(&chunks, Some(&vectors)).unwrap()
    }

    fn reader(batch: RecordBatch) -> impl arrow_array::RecordBatchReader + Send + 'static {
        RecordBatchIterator::new(vec![Ok(batch)], schema())
    }

    /// Enough rows to train IVF_PQ (lance wants 256 per PQ codebook).
    const TRAINABLE: usize = 300;

    #[tokio::test]
    async fn build_indexes_refreshes_an_existing_index_instead_of_rebuilding_it() {
        let dir = tempfile::tempdir().unwrap();
        let uri = table_uri(&dir.path().to_string_lossy());
        let mut ds = Dataset::write(
            reader(embedded(&turns(0, TRAINABLE))),
            &uri,
            Some(WriteParams::default()),
        )
        .await
        .unwrap();
        build_indexes(&mut ds, |_| {}).await;
        let built = sub_index_counts(&ds).await.unwrap();
        assert_eq!(built[FTS_INDEX], 1);
        assert_eq!(built[VECTOR_INDEX], 1, "300 rows train an IVF_PQ index");
        let base: Vec<_> = ds.load_indices().await.unwrap().iter().map(|i| i.uuid).collect();
        assert_eq!(base.len(), 2);

        ds.append(reader(embedded(&turns(TRAINABLE, 5))), None).await.unwrap();
        build_indexes(&mut ds, |phase| panic!("built {phase} whole instead of refreshing it")).await;
        let refreshed = sub_index_counts(&ds).await.unwrap();
        assert_eq!(refreshed[FTS_INDEX], 2, "one delta over the appended rows");
        assert_eq!(refreshed[VECTOR_INDEX], 2);
        let after = ds.load_indices().await.unwrap();
        for uuid in base {
            assert!(after.iter().any(|i| i.uuid == uuid), "base index {uuid} was rebuilt");
        }
    }

    /// Pins the Lance behavior [`optimize_index`] relies on: `append()` adds one delta sub-index per
    /// backlog, and `merge(deltas)` folds the deltas back into one without touching the base.
    #[tokio::test]
    async fn append_optimize_stacks_deltas_and_merge_spares_the_base() {
        let batch = |texts: &[&str]| {
            let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
            let rows = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(texts.to_vec()))]);
            RecordBatchIterator::new([rows], schema)
        };
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().join("t.lance");
        let mut ds = Dataset::write(batch(&["alpha bravo"]), uri.to_str().unwrap(), None)
            .await
            .unwrap();
        ds.create_index(
            &["text"],
            IndexType::Inverted,
            Some(FTS_INDEX.to_string()),
            &InvertedIndexParams::default(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(sub_index_counts(&ds).await.unwrap()[FTS_INDEX], 1);
        let base_uuid = ds.load_indices().await.unwrap()[0].uuid;

        for i in 0..3 {
            ds.append(batch(&[&format!("charlie delta {i}")]), None).await.unwrap();
            ds.optimize_indices(&OptimizeOptions::append()).await.unwrap();
        }
        assert_eq!(sub_index_counts(&ds).await.unwrap()[FTS_INDEX], 4);

        ds.optimize_indices(&OptimizeOptions::merge(3)).await.unwrap();
        assert_eq!(sub_index_counts(&ds).await.unwrap()[FTS_INDEX], 2);
        let after = ds.load_indices().await.unwrap();
        assert!(
            after.iter().any(|i| i.uuid == base_uuid),
            "the base index must survive untouched"
        );
    }

    #[tokio::test]
    async fn fill_vectors_lands_the_embeddings_without_moving_rows() {
        let dir = tempfile::tempdir().unwrap();
        let uri = table_uri(&dir.path().to_string_lossy());
        let chunks = chunk::chunks_from_turns(&turns(0, 3), &chunk::Tier::ALL, true);
        let unembedded = build_batch(&chunks, None).unwrap();
        let ds = Dataset::write(reader(unembedded), &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        assert_eq!(ds.count_rows(Some("vector IS NULL".into())).await.unwrap(), 3);

        let ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
        let vectors: Vec<Vec<f32>> = (0..3).map(|i| vec![i as f32 + 1.0; DIM as usize]).collect();
        let ds = fill_vectors(&ds, &ids, &vectors).await.unwrap();

        assert_eq!(ds.count_rows(None).await.unwrap(), 3, "a fill adds no row");
        assert_eq!(ds.count_rows(Some("vector IS NULL".into())).await.unwrap(), 0);
        let rows = scan_rows(&ds, &["id", "vector"], None, None).await.unwrap();
        for batch in rows {
            let id = batch
                .column_by_name("id")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let vec = batch
                .column_by_name("vector")
                .unwrap()
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .unwrap();
            for row in 0..batch.num_rows() {
                let i = ids.iter().position(|x| *x == id.value(row)).expect("a stored id");
                let first = vec
                    .value(row)
                    .as_any()
                    .downcast_ref::<arrow_array::Float32Array>()
                    .unwrap()
                    .value(0);
                assert_eq!(first, i as f32 + 1.0, "row {} got another row's vector", id.value(row));
            }
        }

        let err = fill_vectors(&ds, &["not-a-row"], &vectors[..1]).await.unwrap_err();
        assert!(err.to_string().contains("lost"), "{err}");
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
