//! An interrupted index build is retried even when no rows or embeddings remain to write.
//! Once the indexes are current, revisiting an empty turns directory leaves the dataset alone.

mod support;

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{ArrayRef, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{Field, Schema};
use arrow_select::concat::concat_batches;
use funes::memory::dataset;
use futures::TryStreamExt;
use lance::index::{DatasetIndexExt, DatasetIndexInternalExt};
use lance::Dataset;
use lance_index::scalar::FullTextSearchQuery;

const ROW_ID: &str = "0123456789abcdef";

/// A complete local-memory row with either a deterministic unit vector or a pending embedding.
fn memory_row(id: &str, seed: u32, embedded: bool) -> RecordBatch {
    let string = |value: Option<&str>| -> ArrayRef { Arc::new(StringArray::from(vec![value])) };
    let number = |value: i64| -> ArrayRef { Arc::new(Int64Array::from(vec![value])) };
    let values = embedded.then(|| {
        let mut state = seed + 1;
        let mut values: Vec<f32> = (0..dataset::DIM)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as f32 / u32::MAX as f32 - 0.5
            })
            .collect();
        let norm = values.iter().map(|x| x * x).sum::<f32>().sqrt();
        values.iter_mut().for_each(|x| *x /= norm);
        values.into_iter().map(Some).collect::<Vec<_>>()
    });
    let vector = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>([values], dataset::DIM);
    let columns: Vec<(&str, ArrayRef)> = vec![
        ("id", string(Some(id))),
        ("text", string(Some("A narwhal remembers the missing text index."))),
        ("session_id", string(Some("finalize-session"))),
        ("workdir", string(Some("demo"))),
        ("turn_uuid", string(Some(id))),
        ("parent_uuid", string(None)),
        ("seq", number(i64::from(seed))),
        ("ts", string(Some("2026-01-01T00:00:00Z"))),
        ("role", string(Some("user"))),
        ("block_type", string(Some("text"))),
        ("tool_name", string(None)),
        ("source_path", string(Some("finalize.funes.jsonl"))),
        ("block_idx", number(0)),
        ("split_idx", number(0)),
        ("vector", Arc::new(vector)),
        ("harness", string(Some("codex"))),
        ("repo", string(Some(""))),
    ];
    let schema = Arc::new(Schema::new_with_metadata(
        columns
            .iter()
            .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
            .collect::<Vec<_>>(),
        HashMap::from([("embedding_model".to_string(), dataset::MODEL.to_string())]),
    ));
    RecordBatch::try_new(schema, columns.into_iter().map(|(_, array)| array).collect()).unwrap()
}

fn index(home: &Path, source: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_funes"))
        .arg("index")
        .arg(source)
        .env("FUNES_HOME", home)
        .output()
        .unwrap();
    support::assert_success(&output);
}

async fn all_rows(ds: &Dataset) -> RecordBatch {
    let batches = dataset::scan_rows(ds, &[], None, None).await.unwrap();
    concat_batches(&Arc::new(Schema::from(ds.schema())), &batches).unwrap()
}

async fn text_matches(ds: &Dataset) -> Vec<String> {
    let mut scan = ds.scan();
    scan.full_text_search(FullTextSearchQuery::new("narwhal".to_string()))
        .unwrap();
    scan.project(&["id"]).unwrap();
    let batches: Vec<RecordBatch> = scan.try_into_stream().await.unwrap().try_collect().await.unwrap();
    batches
        .iter()
        .flat_map(|batch| {
            batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .iter()
                .map(|id| id.unwrap().to_string())
        })
        .collect()
}

#[tokio::test]
async fn an_empty_index_run_repairs_missing_fts_and_then_is_a_noop() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let uri = dataset::table_uri(&home.path().join("memory").to_string_lossy());
    let row = memory_row(ROW_ID, 0, true);
    let reader = RecordBatchIterator::new([Ok(row.clone())], row.schema());
    let before = Dataset::write(reader, &uri, None).await.unwrap();
    assert!(before.load_indices().await.unwrap().is_empty());
    assert_eq!(before.count_rows(Some("vector IS NULL".to_string())).await.unwrap(), 0);
    let before_rows = all_rows(&before).await;

    // The source has no units and the stored row owes no embedding. Only finalization is pending.
    index(home.path(), source.path());
    let repaired = dataset::open(&uri, HashMap::new()).await.unwrap();
    assert_eq!(text_matches(&repaired).await, vec![ROW_ID.to_string()]);
    assert_eq!(all_rows(&repaired).await.columns(), before_rows.columns());
    assert!(repaired.version().version > before.version().version);

    let version = repaired.version().version;
    index(home.path(), source.path());
    let unchanged = dataset::open(&uri, HashMap::new()).await.unwrap();
    assert_eq!(
        unchanged.version().version,
        version,
        "healthy finalization makes no commit"
    );
    assert_eq!(all_rows(&unchanged).await.columns(), before_rows.columns());
    assert_eq!(text_matches(&unchanged).await, vec![ROW_ID.to_string()]);
}

#[tokio::test]
async fn an_embedding_only_run_refreshes_the_existing_vector_index() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let uri = dataset::table_uri(&home.path().join("memory").to_string_lossy());
    // Lance needs at least 256 embedded rows to train PQ. All rows share one fragment, including
    // the null vector, so filling it later must refresh coverage over that existing fragment.
    let mut rows: Vec<_> = (0..300).map(|i| memory_row(&format!("{i:016x}"), i, true)).collect();
    rows.push(memory_row("pending-vector", 300, false));
    let batch = concat_batches(&rows[0].schema(), &rows).unwrap();
    let reader = RecordBatchIterator::new([Ok(batch.clone())], batch.schema());
    let mut before = Dataset::write(reader, &uri, None).await.unwrap();
    dataset::build_indexes(&mut before, |_| {}).await.unwrap();
    let vector_indexes: Vec<_> = before
        .load_indices()
        .await
        .unwrap()
        .iter()
        .filter(|index| index.name == "vector_idx")
        .map(|index| index.uuid)
        .collect();
    assert!(!vector_indexes.is_empty(), "the fixture has a trained IVF index");
    assert!(before.unindexed_fragments("vector_idx").await.unwrap().is_empty());
    assert_eq!(before.count_rows(Some("vector IS NULL".to_string())).await.unwrap(), 1);
    let before_rows = all_rows(&before).await;

    index(home.path(), source.path());
    let refreshed = dataset::open(&uri, HashMap::new()).await.unwrap();
    assert_eq!(
        refreshed.count_rows(Some("vector IS NULL".to_string())).await.unwrap(),
        0
    );
    assert!(
        refreshed.unindexed_fragments("vector_idx").await.unwrap().is_empty(),
        "embedding-only finalization restores vector-index coverage"
    );
    assert!(
        refreshed
            .load_indices()
            .await
            .unwrap()
            .iter()
            .any(|index| index.name == "vector_idx" && !vector_indexes.contains(&index.uuid)),
        "the vector index incorporates the newly filled embedding"
    );
    let after_rows = all_rows(&refreshed).await;
    assert_eq!(after_rows.num_rows(), before_rows.num_rows());
    for (i, field) in before_rows.schema().fields().iter().enumerate() {
        if field.name() != "vector" {
            assert_eq!(after_rows.column(i), before_rows.column(i), "{} changed", field.name());
        }
    }
}
