//! Shared store helpers: the local store location, opening a dataset, plain scans, and building the
//! FTS/IVF indexes. The local store lives at `$FUNES_DB`/`~/.funes` → `…/lancedb`, holding the
//! `chunks` Lance dataset.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use arrow_array::RecordBatch;
use futures::TryStreamExt;
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::Dataset;
use lance::index::vector::VectorIndexParams;
use lance::index::DatasetIndexExt;
use lance_index::scalar::InvertedIndexParams;
use lance_index::vector::ivf::IvfBuildParams;
use lance_index::vector::pq::PQBuildParams;
use lance_index::IndexType;
use lance_linalg::distance::MetricType;

/// The table (Lance dataset) name within a store.
pub const TABLE: &str = "chunks";

/// Base directory for the local store: `$FUNES_DB` if set, else `~/.funes`.
pub fn funes_dir() -> PathBuf {
    if let Ok(d) = std::env::var("FUNES_DB") {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".funes")
}

/// Directory holding the local store (the `chunks` dataset is at `<dir>/chunks.lance`).
pub fn local_store_dir() -> String {
    funes_dir().join("lancedb").to_string_lossy().into_owned()
}

/// The `chunks` dataset URI under a store base (a local directory or an `hf://…` prefix).
pub fn table_uri(base: &str) -> String {
    format!("{base}/{TABLE}.lance")
}

/// Open the `chunks` dataset at `uri`; `storage_options` carries the hf token/revision for a remote.
pub async fn open(uri: &str, storage_options: HashMap<String, String>) -> Result<Dataset> {
    DatasetBuilder::from_uri(uri)
        .with_storage_options(storage_options)
        .load()
        .await
        .context("opening the dataset")
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
pub async fn build_indexes(ds: &mut Dataset) {
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
        let _ = ds
            .create_index(&["vector"], IndexType::Vector, None, &params, true)
            .await;
    }
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
