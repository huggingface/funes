//! Every Lance call funes makes: the local memory location, the `chunks` table's schema and rows,
//! opening a dataset, scans and searches, the columns of what they return, writes, and building
//! the FTS/IVF indexes. A command holds the [`Dataset`] and [`RecordBatch`] values these hand out
//! and hands them back; it names no Lance or arrow API itself.

use crate::chunk::{self, Chunk};
use crate::inference::EmbeddingModel;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use arrow_array::types::Float32Type;
use arrow_array::{
    Array, BooleanArray, FixedSizeListArray, Float32Array, Int64Array, RecordBatchIterator, StringArray, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use arrow_select::filter::filter_record_batch;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::optimize::{compact_files, plan_compaction, CompactionOptions};
use lance::dataset::scanner::Scanner;
use lance::dataset::{
    Dataset, MergeInsertBuilder, MergeInsertWriteMode, WhenMatched, WhenNotMatched, WriteMode, WriteParams, ROW_ID,
};
use lance::index::vector::VectorIndexParams;
use lance::index::{DatasetIndexExt, DatasetIndexInternalExt};
use lance_index::optimize::OptimizeOptions;
use lance_index::scalar::{FullTextSearchQuery, InvertedIndexParams};
use lance_index::vector::ivf::IvfBuildParams;
use lance_index::vector::pq::PQBuildParams;
use lance_index::{IndexParams, IndexType};
use lance_io::object_store::{ObjectStoreParams, WrappingObjectStore};
use lance_linalg::distance::MetricType;
use serde::{Deserialize, Serialize};

/// The rows a scan hands out and a write takes back, as a command holds them.
pub use arrow_array::RecordBatch;

/// The table (Lance dataset) name within a memory.
pub const TABLE: &str = "chunks";

/// The embedding models a new memory's vectors are built with, each by the id pinned in the schema
/// metadata and enforced on open ([`super::Memory::open`]): a memory built with a model funes
/// doesn't run can't be queried.
pub mod model {
    use super::stored;
    use crate::inference::EmbeddingModel;

    pub const ENGLISH: EmbeddingModel = stored("BAAI/bge-small-en-v1.5");
    pub const MULTILINGUAL: EmbeddingModel = stored("intfloat/multilingual-e5-small");
}

/// The width of every model's vectors.
pub const DIM: i32 = 384;

const fn stored(id: &str) -> EmbeddingModel {
    match EmbeddingModel::from_id(id) {
        Some(model) => model,
        None => panic!("a memory stores an embedding model funes does not run"),
    }
}

/// The `chunks` dataset URI under a memory base (a local directory or a remote URI prefix).
pub fn table_uri(base: impl AsRef<Path>) -> String {
    format!("{}/{TABLE}.lance", base.as_ref().display())
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
    collect(scan).await
}

/// [`scan_rows`], every row with its Lance row id ([`row_ids`]), the handle [`take_rows`] and
/// [`fill_vectors`] take.
pub async fn scan_rows_with_ids(ds: &Dataset, columns: &[&str], filter: Option<&str>) -> Result<Vec<RecordBatch>> {
    let mut scan = ds.scan();
    scan.project(columns)?;
    scan.with_row_id();
    if let Some(f) = filter {
        scan.filter(f)?;
    }
    collect(scan).await
}

/// The `limit` rows nearest to `query` by vector distance, nearest first, each with its row id.
/// `filter` is applied before the search, not to its result: a selective filter would otherwise
/// drop most of a globally-nearest pool and return far fewer than `limit` rows when matches exist.
pub async fn nearest(
    ds: &Dataset,
    query: &[f32],
    limit: usize,
    filter: Option<&str>,
    columns: &[&str],
) -> Result<Vec<RecordBatch>> {
    let query = Float32Array::from(query.to_vec());
    let mut scan = ds.scan();
    scan.nearest("vector", &query, limit)?;
    if let Some(f) = filter {
        scan.prefilter(true);
        scan.filter(f)?;
    }
    scan.project(columns)?;
    scan.with_row_id();
    collect(scan).await
}

/// The `limit` rows best matching `query` by BM25, best first, each with its row id. `filter` is
/// applied before the search, as in [`nearest`]. Errors on a memory with no text index.
pub async fn full_text(
    ds: &Dataset,
    query: &str,
    limit: usize,
    filter: Option<&str>,
    columns: &[&str],
) -> Result<Vec<RecordBatch>> {
    let mut scan = ds.scan();
    scan.full_text_search(FullTextSearchQuery::new(query.to_string()))?;
    if let Some(f) = filter {
        scan.prefilter(true);
        scan.filter(f)?;
    }
    scan.project(columns)?;
    scan.with_row_id();
    scan.limit(Some(limit as i64), None)?;
    collect(scan).await
}

/// Drain `scan` in its order.
async fn collect(scan: Scanner) -> Result<Vec<RecordBatch>> {
    let mut stream = scan.try_into_stream().await?;
    let mut batches = Vec::new();
    while let Some(batch) = stream.try_next().await? {
        batches.push(batch);
    }
    Ok(batches)
}

/// [`scan_rows`] with the batches folded in as they stream — the caller's accumulator sees one
/// batch at a time instead of a collected `Vec`, so a whole-table scan peaks at the in-flight
/// batches plus the fold state rather than the full result.
pub async fn scan_fold<T>(
    ds: &Dataset,
    columns: &[&str],
    filter: Option<&str>,
    limit: Option<i64>,
    init: T,
    mut fold: impl FnMut(T, RecordBatch) -> T,
) -> Result<T> {
    let mut scan = ds.scan();
    if !columns.is_empty() {
        scan.project(columns)?;
    }
    if let Some(f) = filter {
        scan.filter(f)?;
    }
    scan.limit(limit, None)?;
    let mut stream = scan.try_into_stream().await?;
    let mut acc = init;
    while let Some(batch) = stream.try_next().await? {
        acc = fold(acc, batch);
    }
    Ok(acc)
}

/// The rows at `row_ids`, projected to `columns` (every column for `&[]`).
pub async fn take_rows(ds: &Dataset, row_ids: &[u64], columns: &[&str]) -> Result<RecordBatch> {
    let projection = if columns.is_empty() {
        ds.schema().clone()
    } else {
        ds.schema().project(columns)?
    };
    ds.take_rows(row_ids, projection).await.context("reading rows by id")
}

/// How many rows `filter` keeps (all rows for `None`).
pub async fn count_rows(ds: &Dataset, filter: Option<&str>) -> Result<usize> {
    ds.count_rows(filter.map(str::to_string)).await.context("counting rows")
}

/// When the memory was last written, from its head version; `None` when the version records no
/// time.
pub fn last_write(ds: &Dataset) -> Option<DateTime<Utc>> {
    let t = ds.version().timestamp;
    (t.timestamp() > 0).then_some(t)
}

/// Whether the memory carries `column`: one from before a migration lacks it, and projecting it
/// would error, so every migrated column is asked for before it is read.
pub fn has_column(ds: &Dataset, column: &str) -> bool {
    Schema::from(ds.schema()).column_with_name(column).is_some()
}

/// The memory's schema, metadata included, as a value to hand back to a write.
pub fn schema_of(ds: &Dataset) -> Arc<Schema> {
    Arc::new(Schema::from(ds.schema()))
}

/// The most rows any index leaves unindexed, from the index stats; 0 when they cannot be read.
pub async fn max_unindexed_rows(ds: &Dataset) -> u64 {
    let Ok(indices) = ds.load_indices().await else {
        return 0;
    };
    let mut max = 0u64;
    for idx in indices.iter() {
        if let Ok(json) = ds.index_statistics(&idx.name).await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(n) = v.get("num_unindexed_rows").and_then(|x| x.as_u64()) {
                    max = max.max(n);
                }
            }
        }
    }
    max
}

// ---- filters ----------------------------------------------------------------------------------

/// Rows awaiting a vector.
pub const UNEMBEDDED: &str = "vector IS NULL";
/// Rows holding a vector.
pub const EMBEDDED: &str = "vector IS NOT NULL";

/// `value` as a filter literal: quoted, a quote in it doubled.
pub fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The filter clause `column = 'value'`.
pub fn eq(column: &str, value: &str) -> String {
    format!("{column} = {}", quoted(value))
}

/// The filter clause `column IN ('…', …)`. `values` must not be empty.
pub fn in_list<'a>(column: &str, values: impl IntoIterator<Item = &'a str>) -> String {
    let list: Vec<String> = values.into_iter().map(quoted).collect();
    debug_assert!(!list.is_empty(), "an IN list needs a value");
    format!("{column} IN ({})", list.join(", "))
}

/// The filter clause keeping rows whose `column`, words joined by single spaces, holds `word` as
/// one of them. `word` must be non-empty and spaceless. A `%`, `_` or `\` in it matches only
/// itself (`\` is the escape of Lance's `LIKE`).
pub fn has_word(column: &str, word: &str) -> String {
    debug_assert!(
        !word.is_empty() && !word.contains(char::is_whitespace),
        "a word is non-empty and spaceless"
    );
    let literal = word.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("concat(' ', {column}, ' ') LIKE {}", quoted(&format!("% {literal} %")))
}

// ---- columns ----------------------------------------------------------------------------------
//
// A column a batch lacks reads as all-null: a memory from before a column existed reads like one
// where the column is unset.

/// A string column of a scanned batch, read by row.
pub struct Strs<'a>(Option<&'a StringArray>);

/// The string column `name` of `batch`.
pub fn strs<'a>(batch: &'a RecordBatch, name: &str) -> Strs<'a> {
    Strs(batch.column_by_name(name).and_then(|c| c.as_any().downcast_ref()))
}

impl<'a> Strs<'a> {
    /// The value at `i`, `""` for a null.
    pub fn get(&self, i: usize) -> &'a str {
        self.opt(i).unwrap_or_default()
    }

    /// The value at `i`, `None` for a null.
    pub fn opt(&self, i: usize) -> Option<&'a str> {
        self.0.filter(|c| !c.is_null(i)).map(|c| c.value(i))
    }
}

/// An integer column of a scanned batch, read by row.
pub struct Ints<'a>(Option<&'a Int64Array>);

/// The integer column `name` of `batch`.
pub fn ints<'a>(batch: &'a RecordBatch, name: &str) -> Ints<'a> {
    Ints(batch.column_by_name(name).and_then(|c| c.as_any().downcast_ref()))
}

impl Ints<'_> {
    /// The value at `i`, 0 for a null.
    pub fn get(&self, i: usize) -> i64 {
        self.0.filter(|c| !c.is_null(i)).map(|c| c.value(i)).unwrap_or(0)
    }
}

/// The vector column of a scanned batch, read by row.
pub struct Vectors<'a>(Option<&'a FixedSizeListArray>);

/// The vector column `name` of `batch`.
pub fn vectors<'a>(batch: &'a RecordBatch, name: &str) -> Vectors<'a> {
    Vectors(batch.column_by_name(name).and_then(|c| c.as_any().downcast_ref()))
}

impl Vectors<'_> {
    /// The vector at `i`, `None` for a null or one with a null element.
    pub fn get(&self, i: usize) -> Option<Vec<f32>> {
        let col = self.0.filter(|c| !c.is_null(i))?;
        let value = col.value(i);
        let floats = value.as_any().downcast_ref::<Float32Array>()?;
        if floats.null_count() > 0 {
            return None;
        }
        Some(floats.values().to_vec())
    }
}

/// The row ids of a batch scanned with them.
pub struct RowIds<'a>(&'a UInt64Array);

/// The row ids of `batch`; an error for a batch scanned without them.
pub fn row_ids(batch: &RecordBatch) -> Result<RowIds<'_>> {
    batch
        .column_by_name(ROW_ID)
        .and_then(|c| c.as_any().downcast_ref())
        .map(RowIds)
        .context("a scan without row ids")
}

impl RowIds<'_> {
    /// The row id at `i`.
    pub fn get(&self, i: usize) -> u64 {
        self.0.value(i)
    }
}

/// The `id` of every row of `batches`.
pub fn ids_in(batches: &[RecordBatch]) -> HashSet<String> {
    let mut out = HashSet::new();
    for batch in batches {
        let ids = strs(batch, "id");
        out.extend((0..batch.num_rows()).map(|i| ids.get(i).to_string()));
    }
    out
}

/// The `id` of every row `filter` keeps (all rows for `None`).
pub async fn ids(ds: &Dataset, filter: Option<&str>) -> Result<HashSet<String>> {
    Ok(ids_in(&scan_rows(ds, &["id"], filter, None).await?))
}

/// Every distinct value of `column` among the rows `filter` keeps.
pub async fn distinct(ds: &Dataset, column: &str, filter: Option<&str>) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for batch in scan_rows(ds, &[column], filter, None).await? {
        let values = strs(&batch, column);
        out.extend((0..batch.num_rows()).map(|i| values.get(i).to_string()));
    }
    Ok(out)
}

/// Every row's `id`, grouped by its session, in scan order.
pub async fn ids_by_session(ds: &Dataset) -> Result<HashMap<String, Vec<String>>> {
    let mut by_session: HashMap<String, Vec<String>> = HashMap::new();
    for batch in scan_rows(ds, &["id", "session_id"], None, None).await? {
        let (ids, sessions) = (strs(&batch, "id"), strs(&batch, "session_id"));
        for i in 0..batch.num_rows() {
            by_session
                .entry(sessions.get(i).to_string())
                .or_default()
                .push(ids.get(i).to_string());
        }
    }
    Ok(by_session)
}

/// The [`Chunk`]s of `batches` (all columns), so a memory can be rewritten without its source.
/// The `vector` column is dropped — rewritten rows are re-embedded.
pub fn chunks(batches: &[RecordBatch]) -> Vec<Chunk> {
    let mut out = Vec::new();
    for b in batches {
        let s = |name: &str| strs(b, name);
        let n = |name: &str| ints(b, name);
        let (
            id,
            text,
            session_id,
            workdir,
            turn_uuid,
            parent_uuid,
            ts,
            role,
            block_type,
            tool_name,
            source_path,
            harness,
            repo,
        ) = (
            s("id"),
            s("text"),
            s("session_id"),
            s("workdir"),
            s("turn_uuid"),
            s("parent_uuid"),
            s("ts"),
            s("role"),
            s("block_type"),
            s("tool_name"),
            s("source_path"),
            s("harness"),
            s("repo"),
        );
        let (seq, block_idx, split_idx) = (n("seq"), n("block_idx"), n("split_idx"));
        for i in 0..b.num_rows() {
            out.push(Chunk {
                id: id.get(i).to_string(),
                text: text.get(i).to_string(),
                session_id: session_id.get(i).to_string(),
                workdir: workdir.get(i).to_string(),
                turn_uuid: turn_uuid.get(i).to_string(),
                parent_uuid: parent_uuid.opt(i).map(str::to_string),
                seq: seq.get(i),
                ts: ts.get(i).to_string(),
                role: role.get(i).to_string(),
                block_type: block_type.get(i).to_string(),
                tool_name: tool_name.opt(i).map(str::to_string),
                source_path: source_path.get(i).to_string(),
                block_idx: block_idx.get(i),
                split_idx: split_idx.get(i),
                harness: harness.get(i).to_string(),
                repo: repo.get(i).to_string(),
            });
        }
    }
    out
}

// ---- writes -----------------------------------------------------------------------------------

/// Append `batch`'s rows.
pub async fn append(ds: &mut Dataset, batch: RecordBatch) -> Result<()> {
    let schema = batch.schema();
    ds.append(RecordBatchIterator::new([Ok(batch)], schema), None)
        .await
        .context("appending rows")?;
    Ok(())
}

/// Create the dataset at `uri` holding `batch`'s rows.
pub async fn create(uri: &str, batch: RecordBatch) -> Result<Dataset> {
    let schema = batch.schema();
    Dataset::write(RecordBatchIterator::new([Ok(batch)], schema), uri, None)
        .await
        .context("creating the memory")
}

/// Rewrite the memory at `uri` as `batches`, every row re-stamped with `model`'s schema, in one
/// commit.
pub async fn overwrite(uri: &str, batches: Vec<RecordBatch>, model: EmbeddingModel) -> Result<Dataset> {
    let schema = schema(model);
    let batches = restamped(&schema, batches)?;
    let params = WriteParams {
        mode: WriteMode::Overwrite,
        ..Default::default()
    };
    Dataset::write(
        RecordBatchIterator::new(batches.into_iter().map(Ok), schema),
        uri,
        Some(params),
    )
    .await
    .context("rewriting the memory")
}

/// `batches` carrying `schema`, metadata included: a scan's batches drop it.
pub fn restamped(schema: &Arc<Schema>, batches: Vec<RecordBatch>) -> Result<Vec<RecordBatch>> {
    batches
        .into_iter()
        .map(|b| RecordBatch::try_new(schema.clone(), b.columns().to_vec()).context("re-stamping rows"))
        .collect()
}

/// The rows of `batch` that `keep` accepts, by position.
pub fn keep_rows(batch: &RecordBatch, keep: impl Fn(usize) -> bool) -> Result<RecordBatch> {
    let mask: BooleanArray = (0..batch.num_rows()).map(keep).collect();
    filter_record_batch(batch, &mask).context("filtering rows")
}

/// What a version cleanup freed.
pub struct Reclaimed {
    pub bytes: u64,
    pub versions: u64,
}

/// Delete the versions older than `age` and the files only they reference.
pub async fn delete_versions_before(ds: &Dataset, age: chrono::Duration) -> Result<Reclaimed> {
    let stats = ds
        .cleanup_old_versions(age, None, None)
        .await
        .context("deleting old versions")?;
    Ok(Reclaimed {
        bytes: stats.bytes_removed,
        versions: stats.old_versions,
    })
}

/// lance's default `<column>_idx` names, so an index lance named is refreshed, not rebuilt beside.
pub(crate) const FTS_INDEX: &str = "text_idx";
pub(crate) const VECTOR_INDEX: &str = "vector_idx";

/// Progress from an index build, and a failure of its optional vector index.
#[derive(Debug)]
pub enum IndexBuildEvent {
    Building(&'static str),
    MergingFragments(usize),
    MergingDeltas { index: String, deltas: usize },
    VectorIndexFailed(anyhow::Error),
}

const UNSTAMPED: EmbeddingModel = stored("BAAI/bge-small-en-v1.5");

/// A memory from before the model was recorded holds bge-small-en vectors.
pub fn embedding_model(ds: &Dataset) -> Result<EmbeddingModel> {
    stamped_model(&Schema::from(ds.schema()))
}

/// The model a memory's `schema` records, as [`embedding_model`] reads it.
pub(crate) fn stamped_model(schema: &Schema) -> Result<EmbeddingModel> {
    match schema.metadata().get("embedding_model") {
        None => Ok(UNSTAMPED),
        Some(id) => EmbeddingModel::from_id(id)
            .ok_or_else(|| anyhow!("memory built with embedding model {id:?}, which funes does not run")),
    }
}

/// A memory's text counted per language.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Languages {
    /// How many of the memory's rows the counts cover.
    pub rows: u64,
    /// Text blocks per ISO 639-3 code.
    pub counts: BTreeMap<String, u64>,
}

impl Languages {
    /// Count `batch`'s rows in.
    pub fn add(&mut self, batch: &RecordBatch) {
        self.rows += batch.num_rows() as u64;
        count_blocks(&mut self.counts, batch);
    }
}

const LANGUAGES: &str = "languages";

/// The language counts `ds` records.
pub fn languages(ds: &Dataset) -> Result<Option<Languages>> {
    Schema::from(ds.schema())
        .metadata()
        .get(LANGUAGES)
        .map(|counts| serde_json::from_str(counts).context("reading the memory's language counts"))
        .transpose()
}

/// Record `counts` as the memory's language counts.
pub async fn stamp_languages(ds: &mut Dataset, counts: &Languages) -> Result<()> {
    let counts = serde_json::to_string(counts)?;
    ds.update_schema_metadata([(LANGUAGES, counts.as_str())])
        .await
        .context("recording the memory's language counts")?;
    Ok(())
}

fn count_blocks(counts: &mut BTreeMap<String, u64>, batch: &RecordBatch) {
    let (role, block_type, text) = (strs(batch, "role"), strs(batch, "block_type"), strs(batch, "text"));
    let split_idx = ints(batch, "split_idx");
    for i in 0..batch.num_rows() {
        if let Some(code) = chunk::language(role.get(i), block_type.get(i), split_idx.get(i), text.get(i)) {
            *counts.entry(code.to_string()).or_default() += 1;
        }
    }
}

/// The language counts of every row of `ds`.
pub async fn scan_languages(ds: &Dataset) -> Result<Languages> {
    // The only rows `chunk::language` reads, so no other row's text is fetched.
    let filter = "role IN ('user', 'assistant') AND block_type = 'text' AND split_idx = 0";
    let mut counts = BTreeMap::new();
    for batch in scan_rows(ds, &["role", "block_type", "split_idx", "text"], Some(filter), None).await? {
        count_blocks(&mut counts, &batch);
    }
    Ok(Languages {
        rows: count_rows(ds, None).await? as u64,
        counts,
    })
}

/// The default analyzer splits text only on spaces and punctuation, so a Chinese clause is one token.
fn fts_params(model: EmbeddingModel) -> InvertedIndexParams {
    match model {
        EmbeddingModel::BgeSmallEn => InvertedIndexParams::default(),
        EmbeddingModel::MultilingualE5Small => InvertedIndexParams::default().base_tokenizer("icu".to_string()),
    }
}

/// Build or refresh the required text index and optional vector index.
/// Fewer than 256 non-null vectors cannot train IVF_PQ and use brute-force search.
pub async fn build_indexes(ds: &mut Dataset, on_event: impl Fn(IndexBuildEvent)) -> Result<()> {
    sweep_shuffle_leftovers(&std::env::temp_dir());
    let existing = sub_index_counts(ds).await?;
    let fts = fts_params(embedding_model(ds)?);
    refresh_or_build(ds, &FTS, &existing, &fts, &on_event).await?;
    if let Some(ivf_pq) = ivf_pq_params(ds) {
        let result: Result<()> = async {
            let embedded = ds
                .count_rows(Some("vector IS NOT NULL".into()))
                .await
                .context("counting embedded rows for the vector index")?;
            if embedded < 256 {
                return Ok(());
            }
            refresh_or_build(ds, &VECTOR, &existing, &ivf_pq, &on_event).await
        }
        .await;
        if let Err(error) = result {
            on_event(IndexBuildEvent::VectorIndexFailed(error));
        }
    }
    Ok(())
}

/// One of the two indexes a memory carries.
struct IndexSpec {
    name: &'static str,
    column: &'static str,
    index_type: IndexType,
    label: &'static str,
}

const FTS: IndexSpec = IndexSpec {
    name: FTS_INDEX,
    column: "text",
    index_type: IndexType::Inverted,
    label: "text search index",
};

const VECTOR: IndexSpec = IndexSpec {
    name: VECTOR_INDEX,
    column: "vector",
    index_type: IndexType::Vector,
    label: "vector index",
};

/// Refresh `index` if `existing` lists it, else build it whole with `params`. A refresh that fails
/// also builds it whole, so the rows it left out don't stay unindexed.
async fn refresh_or_build(
    ds: &mut Dataset,
    index: &IndexSpec,
    existing: &BTreeMap<String, usize>,
    params: &dyn IndexParams,
    on_event: impl Fn(IndexBuildEvent),
) -> Result<()> {
    if let Some(&subs) = existing.get(index.name) {
        if optimize_index(ds, index.name, subs, &on_event).await.is_ok() {
            return Ok(());
        }
    }
    on_event(IndexBuildEvent::Building(index.label));
    ds.create_index(
        &[index.column],
        index.index_type,
        Some(index.name.to_string()),
        params,
        true,
    )
    .await
    .with_context(|| format!("building the {}", index.label))?;
    Ok(())
}

/// Whether any fragments lack text-search index coverage.
pub(crate) async fn fts_needs_refresh(ds: &Dataset) -> Result<bool> {
    Ok(!ds
        .unindexed_fragments(FTS_INDEX)
        .await
        .context("checking text search index coverage")?
        .is_empty())
}

/// The files a lance IVF shuffle directory holds, and nothing else.
const SHUFFLE_FILES: [&str; 4] = [
    "shuffle_data.lance",
    "shuffle_data.spill",
    "shuffle_offsets.lance",
    "shuffle_offsets.spill",
];

/// How long a shuffle directory must have sat untouched before a sweep may call it settled.
const SHUFFLE_LEFTOVER_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Best-effort: remove the shuffle directories a previous IVF build left in `$TMPDIR`.
///
/// lance drops the scratch directory's guard before the shuffler writes to it, so every build leaks
/// one, tens of MB apiece (fixed in lance 13.0.0). Swept on entry, never on exit: age says a
/// directory is settled, not that nothing is still reading it, so the current build's own is left
/// to a later run.
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

/// Fold an index's delta sub-indexes back into one once this many pile up. Queries fan out across
/// every delta (and per-segment BM25 stats drift), so the pile must stay bounded. Only the deltas
/// are merged — the base is never re-read, which would be the full-index rewrite [`optimize_index`]
/// exists to avoid.
pub(crate) const MERGE_DELTAS: usize = 8;

/// Sub-index count per index name (the base plus its deltas, which share the index's name), from
/// the index metadata alone: `index_statistics` can write a stats migration.
pub(crate) async fn sub_index_counts(ds: &Dataset) -> Result<BTreeMap<String, usize>> {
    let indices = ds.load_indices().await.context("listing the indexes")?;
    let mut counts = BTreeMap::new();
    for idx in indices.iter() {
        *counts.entry(idx.name.clone()).or_default() += 1;
    }
    Ok(counts)
}

/// Add a delta sub-index over the rows appended since `name` was last built, or at
/// [`MERGE_DELTAS`] deltas merge them into one, sparing the base. `subs` is base + deltas.
/// `on_event` gets [`IndexBuildEvent::MergingDeltas`] before a merge.
pub(crate) async fn optimize_index(
    ds: &mut Dataset,
    name: &str,
    subs: usize,
    on_event: impl Fn(IndexBuildEvent),
) -> Result<()> {
    let deltas = subs.saturating_sub(1);
    let opts = if deltas >= MERGE_DELTAS {
        on_event(IndexBuildEvent::MergingDeltas {
            index: name.to_string(),
            deltas,
        });
        OptimizeOptions::merge(deltas)
    } else {
        OptimizeOptions::append()
    };
    ds.optimize_indices(&opts.index_names(vec![name.to_string()]))
        .await
        .with_context(|| format!("optimizing {name}"))?;
    Ok(())
}

/// Rows per fragment a compaction writes. A remote read fetches whole data files, so a fragment
/// stays a few MB.
const FRAGMENT_ROWS: usize = 4096;

/// Past this many indexed fragments under [`FRAGMENT_ROWS`] rows, a compaction rewrites every
/// fragment. A recall pays about 0.5 ms per fragment.
const MAX_SMALL_FRAGMENTS: usize = 256;

/// Merge the fragments no index covers yet, or all of them past [`MAX_SMALL_FRAGMENTS`]. Rewriting
/// an indexed fragment rewrites every index covering it.
pub(crate) async fn compact_fragments(ds: &mut Dataset, on_event: impl Fn(IndexBuildEvent)) -> Result<()> {
    compact_fragments_past(ds, MAX_SMALL_FRAGMENTS, on_event).await
}

/// How many fragments [`compact_fragments`] would merge.
pub(crate) async fn fragments_to_compact(ds: &Dataset) -> Result<usize> {
    let options = compaction_options(ds, MAX_SMALL_FRAGMENTS).await?;
    planned_fragments(ds, &options).await
}

/// [`compact_fragments`], rewriting every fragment past `max_small` small indexed ones.
async fn compact_fragments_past(ds: &mut Dataset, max_small: usize, on_event: impl Fn(IndexBuildEvent)) -> Result<()> {
    let options = compaction_options(ds, max_small).await?;
    let fragments = planned_fragments(ds, &options).await?;
    if fragments == 0 {
        return Ok(());
    }
    on_event(IndexBuildEvent::MergingFragments(fragments));
    compact_files(ds, options, None)
        .await
        .context("compacting the fragments")?;
    Ok(())
}

async fn planned_fragments(ds: &Dataset, options: &CompactionOptions) -> Result<usize> {
    let plan = plan_compaction(ds, options).await.context("planning the compaction")?;
    Ok(plan.tasks.iter().map(|task| task.fragments.len()).sum())
}

async fn compaction_options(ds: &Dataset, max_small: usize) -> Result<CompactionOptions> {
    let indices = ds.load_indices().await.context("listing the indexes")?;
    let indexed: Vec<_> = ds
        .get_fragments()
        .into_iter()
        .filter(|f| {
            let id = f.id() as u32;
            indices
                .iter()
                .any(|idx| idx.fragment_bitmap.as_ref().is_none_or(|b| b.contains(id)))
        })
        .collect();
    let small = indexed
        .iter()
        .filter(|f| f.metadata().physical_rows.unwrap_or(0) < FRAGMENT_ROWS)
        .count();
    let excluded_fragment_ids = if small > max_small {
        Vec::new()
    } else {
        indexed.iter().map(|f| f.id() as u32).collect()
    };
    // Only fragments under FRAGMENT_ROWS merge, so a compaction writes no more fragments than it
    // reads.
    Ok(CompactionOptions {
        target_rows_per_fragment: FRAGMENT_ROWS,
        excluded_fragment_ids,
        materialize_deletions: false,
        max_overlays_per_fragment: None,
        ..Default::default()
    })
}

/// Delete every version but the current one, and the files no version references. Needs the memory
/// lock: without it, those files may belong to a write still in progress.
pub(crate) async fn delete_old_versions(ds: &Dataset) -> Result<()> {
    ds.cleanup_old_versions(chrono::Duration::zero(), Some(true), None)
        .await
        .context("deleting the old versions")?;
    Ok(())
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

pub(crate) async fn create_empty(uri: &str, model: EmbeddingModel) -> Result<Dataset> {
    create(uri, build_batch(model, &[], None)?).await
}

/// The table schema (column order is load-bearing for Lance).
pub(crate) fn schema(model: EmbeddingModel) -> Arc<Schema> {
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
        HashMap::from([("embedding_model".to_string(), model.id().to_string())]),
    ))
}

/// `None` writes every row with a null `vector`.
pub(crate) fn build_batch(
    model: EmbeddingModel,
    chunks: &[chunk::Chunk],
    vectors: Option<&[Vec<f32>]>,
) -> Result<RecordBatch> {
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
        schema(model),
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
    let fields = Schema::from(ds.schema());
    let source_schema = Arc::new(Schema::new(vec![
        fields.field_with_name("id")?.clone(),
        fields.field_with_name("vector")?.clone(),
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
    use std::path::PathBuf;

    #[test]
    fn a_filter_literal_doubles_its_quotes() {
        assert_eq!(quoted("o'brien"), "'o''brien'");
        assert_eq!(eq("role", "user"), "role = 'user'");
        assert_eq!(in_list("harness", ["claude", "pi"]), "harness IN ('claude', 'pi')");
    }
    use crate::traces::{Block, Turn, FORMAT_VERSION};
    use arrow_array::RecordBatchIterator;
    use lance::dataset::WriteParams;
    use lance_index::scalar::FullTextSearchQuery;
    use std::cell::RefCell;

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
    fn vectors(n: usize) -> Vec<Vec<f32>> {
        let mut seed = 0x9e37_79b9u32;
        (0..n)
            .map(|_| {
                (0..DIM)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed >> 8) as f32 / (1u32 << 24) as f32
                    })
                    .collect()
            })
            .collect()
    }

    fn embedded(turns: &[Turn]) -> RecordBatch {
        let chunks = chunk::chunks_from_turns(turns, true);
        build_batch(model::ENGLISH, &chunks, Some(&vectors(chunks.len()))).unwrap()
    }

    fn reader(batch: RecordBatch) -> impl arrow_array::RecordBatchReader + Send + 'static {
        RecordBatchIterator::new(vec![Ok(batch)], schema(model::ENGLISH))
    }

    /// Enough rows to train IVF_PQ (lance wants 256 per PQ codebook).
    const TRAINABLE: usize = 300;

    #[tokio::test]
    async fn build_indexes_reports_an_fts_creation_failure() {
        let dir = tempfile::tempdir().unwrap();
        let uri = table_uri(dir.path());
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1]))]).unwrap();
        let mut ds = Dataset::write(RecordBatchIterator::new([Ok(batch)], schema), &uri, None)
            .await
            .unwrap();

        let err = build_indexes(&mut ds, |_| {}).await.unwrap_err();
        assert!(err.to_string().contains("building the text search index"), "{err:#}");
        assert!(sub_index_counts(&ds).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fts_coverage_detects_rows_left_by_an_unfinished_finalization() {
        let dir = tempfile::tempdir().unwrap();
        let uri = table_uri(dir.path());
        let chunks = chunk::chunks_from_turns(&turns(0, 2), true);
        let mut ds = Dataset::write(
            reader(build_batch(model::ENGLISH, &chunks[..1], None).unwrap()),
            &uri,
            None,
        )
        .await
        .unwrap();
        assert!(fts_needs_refresh(&ds).await.unwrap(), "no FTS index was committed");
        build_indexes(&mut ds, |_| {}).await.unwrap();
        assert!(!fts_needs_refresh(&ds).await.unwrap());

        ds.append(reader(build_batch(model::ENGLISH, &chunks[1..], None).unwrap()), None)
            .await
            .unwrap();
        assert!(
            fts_needs_refresh(&ds).await.unwrap(),
            "the appended row still needs FTS"
        );
        build_indexes(&mut ds, |_| {}).await.unwrap();
        assert!(!fts_needs_refresh(&ds).await.unwrap());

        let version = ds.version().version;
        build_indexes(&mut ds, |event| {
            assert!(
                !matches!(event, IndexBuildEvent::Building(_)),
                "a current index must not be rebuilt"
            );
        })
        .await
        .unwrap();
        assert_eq!(ds.version().version, version, "a current index adds no version");
    }

    #[tokio::test]
    async fn vector_training_threshold_counts_embedded_rows() {
        let dir = tempfile::tempdir().unwrap();
        let uri = table_uri(dir.path());
        let mut ds = Dataset::write(reader(embedded(&turns(0, 255))), &uri, None)
            .await
            .unwrap();
        let pending = chunk::chunks_from_turns(&turns(255, 3), true);
        ds.append(reader(build_batch(model::ENGLISH, &pending, None).unwrap()), None)
            .await
            .unwrap();

        let events = RefCell::new(Vec::new());
        build_indexes(&mut ds, |event| events.borrow_mut().push(event))
            .await
            .unwrap();
        assert!(matches!(
            events.borrow().as_slice(),
            [IndexBuildEvent::Building("text search index")]
        ));
        let indexes = sub_index_counts(&ds).await.unwrap();
        assert_eq!(indexes[FTS_INDEX], 1);
        assert!(
            !indexes.contains_key(VECTOR_INDEX),
            "258 total rows include only 255 vectors"
        );

        let vector = vectors(256).pop().unwrap();
        ds = fill_vectors(&ds, &[pending[0].id.as_str()], &[vector]).await.unwrap();
        build_indexes(&mut ds, |_| {}).await.unwrap();
        assert_eq!(ds.count_rows(Some("vector IS NOT NULL".into())).await.unwrap(), 256);
        assert_eq!(ds.count_rows(Some("vector IS NULL".into())).await.unwrap(), 2);
        assert_eq!(sub_index_counts(&ds).await.unwrap()[VECTOR_INDEX], 1);
    }

    #[tokio::test]
    async fn build_indexes_refreshes_an_existing_index_instead_of_rebuilding_it() {
        let dir = tempfile::tempdir().unwrap();
        let uri = table_uri(dir.path());
        let mut ds = Dataset::write(
            reader(embedded(&turns(0, TRAINABLE))),
            &uri,
            Some(WriteParams::default()),
        )
        .await
        .unwrap();
        build_indexes(&mut ds, |_| {}).await.unwrap();
        let built = sub_index_counts(&ds).await.unwrap();
        assert_eq!(built[FTS_INDEX], 1);
        assert_eq!(built[VECTOR_INDEX], 1, "300 rows train an IVF_PQ index");
        let base: Vec<_> = ds.load_indices().await.unwrap().iter().map(|i| i.uuid).collect();
        assert_eq!(base.len(), 2);

        let appended = embedded(&turns(TRAINABLE, 4));
        let probe = appended
            .column_by_name("vector")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap()
            .value(0);
        ds.append(reader(appended), None).await.unwrap();
        let pending = chunk::chunks_from_turns(&turns(TRAINABLE + 4, 1), true);
        ds.append(reader(build_batch(model::ENGLISH, &pending, None).unwrap()), None)
            .await
            .unwrap();
        build_indexes(&mut ds, |event| {
            if let IndexBuildEvent::Building(phase) = event {
                panic!("built {phase} whole instead of refreshing it");
            }
        })
        .await
        .unwrap();
        let refreshed = sub_index_counts(&ds).await.unwrap();
        assert_eq!(refreshed[FTS_INDEX], 2, "one delta over the appended rows");
        assert_eq!(refreshed[VECTOR_INDEX], 2);
        let after = ds.load_indices().await.unwrap();
        for uuid in base {
            assert!(after.iter().any(|i| i.uuid == uuid), "base index {uuid} was rebuilt");
        }

        // fast_search reads the indexes alone, so a hit proves the delta covers the appended rows.
        let first = format!("turn{TRAINABLE}");
        let mut fts = ds.scan();
        fts.full_text_search(FullTextSearchQuery::new(TRAINABLE.to_string()))
            .unwrap()
            .fast_search();
        assert!(turn_uuids(fts).await.contains(&first), "FTS misses the appended rows");
        let mut ann = ds.scan();
        ann.nearest("vector", probe.as_ref(), 10)
            .unwrap()
            .refine(10)
            .fast_search();
        assert!(
            turn_uuids(ann).await.contains(&first),
            "the vector index misses the appended rows"
        );

        let text_indexes: Vec<_> = after
            .iter()
            .filter(|i| i.name == FTS_INDEX)
            .map(|i| (i.uuid, i.fragment_bitmap.clone()))
            .collect();
        assert!(ds.unindexed_fragments(VECTOR_INDEX).await.unwrap().is_empty());
        ds = fill_vectors(&ds, &[pending[0].id.as_str()], &[vec![0.5; DIM as usize]])
            .await
            .unwrap();
        assert_eq!(ds.count_rows(None).await.unwrap(), TRAINABLE + 5, "a fill adds no rows");
        assert!(!fts_needs_refresh(&ds).await.unwrap(), "filling vectors preserves FTS");
        assert_eq!(ds.unindexed_fragments(VECTOR_INDEX).await.unwrap().len(), 1);

        build_indexes(&mut ds, |event| {
            if let IndexBuildEvent::Building(phase) = event {
                panic!("built {phase} whole after only filling vectors");
            }
        })
        .await
        .unwrap();
        assert!(ds.unindexed_fragments(VECTOR_INDEX).await.unwrap().is_empty());
        let refreshed_text: Vec<_> = ds
            .load_indices()
            .await
            .unwrap()
            .iter()
            .filter(|i| i.name == FTS_INDEX)
            .map(|i| (i.uuid, i.fragment_bitmap.clone()))
            .collect();
        assert_eq!(
            refreshed_text, text_indexes,
            "filling vectors preserves every FTS segment"
        );
    }

    /// Two indexed fragments of the same index set, then three unindexed ones.
    async fn indexed_then_appended(dir: &std::path::Path) -> Dataset {
        let params = WriteParams {
            max_rows_per_file: TRAINABLE / 2,
            ..Default::default()
        };
        let uri = table_uri(dir);
        let mut ds = Dataset::write(reader(embedded(&turns(0, TRAINABLE))), &uri, Some(params))
            .await
            .unwrap();
        build_indexes(&mut ds, |_| {}).await.unwrap();
        for i in 0..3 {
            ds.append(reader(embedded(&turns(TRAINABLE + i, 1))), None)
                .await
                .unwrap();
        }
        assert_eq!(ds.get_fragments().len(), 5);
        ds
    }

    fn fragment_ids(ds: &Dataset) -> Vec<usize> {
        ds.get_fragments().iter().map(|f| f.id()).collect()
    }

    #[tokio::test]
    async fn compact_fragments_merges_only_the_unindexed_fragments() {
        let dir = tempfile::tempdir().unwrap();
        let mut ds = indexed_then_appended(dir.path()).await;
        let indexes: Vec<_> = ds.load_indices().await.unwrap().iter().map(|i| i.uuid).collect();

        let merging = RefCell::new(Vec::new());
        compact_fragments(&mut ds, |event| {
            if let IndexBuildEvent::MergingFragments(n) = event {
                merging.borrow_mut().push(n);
            }
        })
        .await
        .unwrap();

        assert_eq!(merging.into_inner(), [3], "the merge is announced once, with its size");
        let ids = fragment_ids(&ds);
        assert_eq!(ids.len(), 3, "the three appended fragments become one: {ids:?}");
        assert_eq!(ids[..2], [0, 1], "the indexed fragments are left alone");
        let after: Vec<_> = ds.load_indices().await.unwrap().iter().map(|i| i.uuid).collect();
        assert_eq!(after, indexes, "no index is rewritten");
        assert_eq!(ds.count_rows(None).await.unwrap(), TRAINABLE + 3);
    }

    #[tokio::test]
    async fn compact_fragments_leaves_a_large_fragment_with_deletions_alone() {
        let dir = tempfile::tempdir().unwrap();
        let uri = table_uri(dir.path());
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let text: Vec<String> = (0..3 * FRAGMENT_ROWS).map(|i| format!("row {i}")).collect();
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(text))]).unwrap();
        let mut ds = Dataset::write(RecordBatchIterator::new([Ok(batch)], schema), &uri, None)
            .await
            .unwrap();
        ds.delete("text LIKE 'row 1%'").await.unwrap();

        assert_eq!(fragments_to_compact(&ds).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn compact_fragments_rewrites_every_fragment_past_the_small_fragment_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut ds = indexed_then_appended(dir.path()).await;

        compact_fragments_past(&mut ds, 0, |_| {}).await.unwrap();

        let ids = fragment_ids(&ds);
        assert_eq!(ids.len(), 2, "one fragment per index set: {ids:?}");
        assert!(!ids.contains(&0), "the indexed fragments are rewritten too");
        let mut fts = ds.scan();
        fts.full_text_search(FullTextSearchQuery::new("7".to_string()))
            .unwrap()
            .fast_search();
        assert!(
            turn_uuids(fts).await.contains(&"turn7".to_string()),
            "the index follows the rewritten fragments"
        );
    }

    #[tokio::test]
    async fn an_e5_memory_indexes_chinese_words() {
        let mut zh = turns(0, 1);
        zh[0].blocks[0].text = "我们修改了向量索引的构建方式".into();
        let chunks = chunk::chunks_from_turns(&zh, true);
        for (model, found) in [(model::ENGLISH, false), (EmbeddingModel::MultilingualE5Small, true)] {
            let dir = tempfile::tempdir().unwrap();
            let uri = table_uri(dir.path());
            let batch = build_batch(model, &chunks, None).unwrap();
            let mut ds = Dataset::write(RecordBatchIterator::new(vec![Ok(batch)], schema(model)), &uri, None)
                .await
                .unwrap();
            build_indexes(&mut ds, |_| {}).await.unwrap();
            let mut fts = ds.scan();
            fts.full_text_search(FullTextSearchQuery::new("向量".to_string()))
                .unwrap();
            assert_eq!(!turn_uuids(fts).await.is_empty(), found, "{}", model.id());
        }
    }

    #[tokio::test]
    async fn a_memory_names_its_embedding_model() {
        let chunks = chunk::chunks_from_turns(&turns(0, 1), true);
        let e5 = EmbeddingModel::MultilingualE5Small;
        let unstamped = Arc::new(Schema::new(schema(model::ENGLISH).fields().clone()));
        for (stamp, model) in [(schema(e5), e5), (unstamped, EmbeddingModel::BgeSmallEn)] {
            let dir = tempfile::tempdir().unwrap();
            let columns = build_batch(model, &chunks, None).unwrap().columns().to_vec();
            let batch = RecordBatch::try_new(stamp.clone(), columns).unwrap();
            let uri = table_uri(dir.path());
            let ds = Dataset::write(RecordBatchIterator::new(vec![Ok(batch)], stamp), &uri, None)
                .await
                .unwrap();
            assert_eq!(embedding_model(&ds).unwrap(), model);
        }
    }

    #[tokio::test]
    async fn an_empty_memory_is_stamped_with_its_model() {
        let dir = tempfile::tempdir().unwrap();
        let e5 = EmbeddingModel::MultilingualE5Small;
        let ds = create_empty(&table_uri(dir.path()), e5).await.unwrap();
        assert_eq!(ds.count_rows(None).await.unwrap(), 0);
        assert_eq!(embedding_model(&ds).unwrap(), e5);
    }

    #[tokio::test]
    async fn language_counts_survive_appends_and_vector_fills() {
        let dir = tempfile::tempdir().unwrap();
        let chunks = chunk::chunks_from_turns(&turns(0, 2), true);
        let first = build_batch(model::ENGLISH, &chunks[..1], None).unwrap();
        let mut ds = create(&table_uri(dir.path()), first).await.unwrap();
        assert_eq!(languages(&ds).unwrap(), None);
        let counts = Languages {
            rows: 1,
            counts: BTreeMap::from([("eng".to_string(), 3), ("por".to_string(), 1)]),
        };
        stamp_languages(&mut ds, &counts).await.unwrap();
        append(&mut ds, build_batch(model::ENGLISH, &chunks[1..], None).unwrap())
            .await
            .unwrap();
        let ds = fill_vectors(&ds, &[chunks[0].id.as_str()], &vectors(1)).await.unwrap();
        assert_eq!(languages(&ds).unwrap(), Some(counts));
    }

    #[tokio::test]
    async fn scanning_languages_agrees_with_counting_every_row() {
        let block = |block_type: &str, text: &str| Block {
            block_type: block_type.into(),
            text: text.into(),
            tool_name: None,
            tool_use_id: None,
        };
        let english = "We decided to roll back the deploy first, then look into the configuration drift. ";
        let mut mixed = turns(0, 3);
        mixed[0].role = "user".into();
        mixed[0].blocks = vec![block(
            "text",
            "Precisamos decidir se o atalho apaga o caractere à frente do cursor no campo de texto.",
        )];
        mixed[1].blocks = vec![block("thinking", english), block("text", &english.repeat(30))];
        mixed[2].blocks = vec![block("tool_use", r#"{"command": "git revert HEAD"}"#)];
        let dir = tempfile::tempdir().unwrap();
        let batch = build_batch(model::ENGLISH, &chunk::chunks_from_turns(&mixed, true), None).unwrap();
        let ds = create(&table_uri(dir.path()), batch).await.unwrap();
        let mut counted = Languages::default();
        for batch in &scan_rows(&ds, &[], None, None).await.unwrap() {
            counted.add(batch);
        }
        let counts = BTreeMap::from([("eng".to_string(), 1), ("por".to_string(), 1)]);
        assert_eq!(counted.counts, counts);
        assert_eq!(scan_languages(&ds).await.unwrap(), counted);
    }

    async fn turn_uuids(mut scan: lance::dataset::scanner::Scanner) -> Vec<String> {
        scan.project(&["turn_uuid"]).unwrap();
        let batches: Vec<RecordBatch> = scan.try_into_stream().await.unwrap().try_collect().await.unwrap();
        batches
            .iter()
            .flat_map(|b| {
                let col = b
                    .column_by_name("turn_uuid")
                    .unwrap()
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();
                col.iter().flatten().map(str::to_string).collect::<Vec<_>>()
            })
            .collect()
    }

    #[tokio::test]
    async fn optimize_index_merges_only_its_own_deltas_once_they_reach_the_threshold() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("text", DataType::Utf8, false),
            Field::new("tag", DataType::Utf8, false),
        ]));
        let batch = |text: &str| {
            let cols: Vec<Arc<dyn arrow_array::Array>> = vec![
                Arc::new(StringArray::from(vec![text])),
                Arc::new(StringArray::from(vec!["t"])),
            ];
            RecordBatchIterator::new([RecordBatch::try_new(schema.clone(), cols)], schema.clone())
        };
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().join("t.lance");
        let mut ds = Dataset::write(batch("alpha bravo"), uri.to_str().unwrap(), None)
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
        ds.create_index(
            &["tag"],
            IndexType::BTree,
            Some("tag_idx".to_string()),
            &lance_index::scalar::ScalarIndexParams::default(),
            true,
        )
        .await
        .unwrap();
        let base = ds
            .load_indices()
            .await
            .unwrap()
            .iter()
            .find(|i| i.name == FTS_INDEX)
            .unwrap()
            .uuid;

        let compactions = std::sync::Mutex::new(Vec::new());
        for i in 0..=MERGE_DELTAS {
            if i == MERGE_DELTAS {
                assert_eq!(sub_index_counts(&ds).await.unwrap()[FTS_INDEX], 1 + MERGE_DELTAS);
                assert!(compactions.lock().unwrap().is_empty(), "merged below the threshold");
            }
            ds.append(batch(&format!("charlie delta {i}")), None).await.unwrap();
            let subs = sub_index_counts(&ds).await.unwrap()[FTS_INDEX];
            optimize_index(&mut ds, FTS_INDEX, subs, |event| {
                if let IndexBuildEvent::MergingDeltas { index, deltas } = event {
                    compactions.lock().unwrap().push((index, deltas));
                }
            })
            .await
            .unwrap();
        }
        let counts = sub_index_counts(&ds).await.unwrap();
        assert_eq!(counts[FTS_INDEX], 2, "the base and one merged delta");
        assert_eq!(counts["tag_idx"], 1, "another index was optimized too");
        assert!(
            ds.load_indices().await.unwrap().iter().any(|i| i.uuid == base),
            "the base was rewritten"
        );
        assert_eq!(
            compactions.into_inner().unwrap(),
            [(FTS_INDEX.to_string(), MERGE_DELTAS)]
        );
    }

    #[tokio::test]
    async fn build_indexes_rebuilds_an_index_whole_when_its_refresh_fails() {
        let dir = tempfile::tempdir().unwrap();
        let uri = table_uri(dir.path());
        let mut ds = Dataset::write(
            reader(embedded(&turns(0, TRAINABLE))),
            &uri,
            Some(WriteParams::default()),
        )
        .await
        .unwrap();
        build_indexes(&mut ds, |_| {}).await.unwrap();
        let base = ds
            .load_indices()
            .await
            .unwrap()
            .iter()
            .find(|i| i.name == FTS_INDEX)
            .unwrap()
            .uuid;
        for run in 0..MERGE_DELTAS {
            ds.append(reader(embedded(&turns(TRAINABLE + run, 1))), None)
                .await
                .unwrap();
            build_indexes(&mut ds, |_| {}).await.unwrap();
        }
        assert_eq!(sub_index_counts(&ds).await.unwrap()[FTS_INDEX], 1 + MERGE_DELTAS);
        // The next refresh merges the deltas, so it has to read this broken one.
        let indices = ds.load_indices().await.unwrap();
        let delta = indices
            .iter()
            .find(|i| i.name == FTS_INDEX && i.uuid != base)
            .unwrap()
            .uuid;
        let delta_dir = std::path::Path::new(&uri).join("_indices").join(delta.to_string());
        for file in std::fs::read_dir(&delta_dir).unwrap() {
            std::fs::write(file.unwrap().path(), b"corrupt").unwrap();
        }

        ds.append(reader(embedded(&turns(TRAINABLE + MERGE_DELTAS, 1))), None)
            .await
            .unwrap();
        let built = std::sync::Mutex::new(Vec::new());
        build_indexes(&mut ds, |event| built.lock().unwrap().push(event))
            .await
            .unwrap();
        assert!(matches!(
            built.into_inner().unwrap().as_slice(),
            [
                IndexBuildEvent::MergingDeltas { index: fts, deltas: MERGE_DELTAS },
                IndexBuildEvent::Building("text search index"),
                IndexBuildEvent::MergingDeltas { index: vector, deltas: MERGE_DELTAS },
            ] if fts == FTS_INDEX && vector == VECTOR_INDEX
        ));
        assert_eq!(
            sub_index_counts(&ds).await.unwrap()[FTS_INDEX],
            1,
            "one whole index, no delta left over"
        );
    }

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
    fn build_batch_preserves_rows_without_embeddings() {
        let chunks = chunk::chunks_from_turns(&turns(0, 3), true);
        let pending = build_batch(model::ENGLISH, &chunks, None).unwrap();
        let embedded = embedded(&turns(0, 3));
        assert_eq!(pending.num_rows(), 3);
        for (i, field) in pending.schema().fields().iter().enumerate() {
            if field.name() == "vector" {
                assert_eq!(pending.column(i).null_count(), 3);
                assert_eq!(embedded.column(i).null_count(), 0);
            } else {
                assert_eq!(pending.column(i), embedded.column(i), "{} changed", field.name());
            }
        }
    }

    #[tokio::test]
    async fn fill_vectors_lands_the_embeddings_without_moving_rows() {
        let dir = tempfile::tempdir().unwrap();
        let uri = table_uri(dir.path());
        let chunks = chunk::chunks_from_turns(&turns(0, 3), true);
        let unembedded = build_batch(model::ENGLISH, &chunks, None).unwrap();
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

    #[tokio::test]
    async fn ids_by_session_groups_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut two = turns(0, 3);
        two[1].session_id = "other".into();
        let chunks = chunk::chunks_from_turns(&two, true);
        let batch = build_batch(model::ENGLISH, &chunks, None).unwrap();
        let ds = Dataset::write(reader(batch), &table_uri(dir.path()), None)
            .await
            .unwrap();
        let by_session = ids_by_session(&ds).await.unwrap();
        assert_eq!(by_session.len(), 2, "one entry per session");
        assert_eq!(by_session["sess"], [chunks[0].id.clone(), chunks[2].id.clone()]);
        assert_eq!(by_session["other"], [chunks[1].id.clone()]);
    }

    #[test]
    fn schema_column_order_is_load_bearing() {
        // Column order must match build_batch's array order exactly, or Lance writes the
        // wrong column. Pin it so a reorder can't slip through.
        let s = schema(model::ENGLISH);
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
