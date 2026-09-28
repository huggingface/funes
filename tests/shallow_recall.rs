//! Recall over rows whose embeddings are all pending, through the real reranker.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{ArrayRef, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{Field, Schema};
use funes::commands::recall;
use funes::memory::{dataset, Memory};
use lance::Dataset;

#[tokio::test]
async fn shallow_recall_returns_ranked_passages_and_neighbors() {
    let home = tempfile::tempdir().unwrap();
    let memory = home.path().join("memory").to_string_lossy().into_owned();
    let texts = [
        "Keep the storage append only.",
        "The narwhal parser reads each JSONL line into a typed turn.",
        "Keep stable chunk identities when rerunning ingestion.",
    ];
    let string = |values: Vec<Option<&str>>| -> ArrayRef { Arc::new(StringArray::from(values)) };
    let number = |values: Vec<i64>| -> ArrayRef { Arc::new(Int64Array::from(values)) };
    let vectors =
        FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(vec![None::<Vec<Option<f32>>>; 3], dataset::DIM);
    let columns: Vec<(&str, ArrayRef)> = vec![
        ("id", string(vec![Some("row-0"), Some("row-1"), Some("row-2")])),
        ("text", string(texts.iter().copied().map(Some).collect())),
        ("session_id", string(vec![Some("shallow-session"); 3])),
        ("workdir", string(vec![Some("demo"); 3])),
        (
            "turn_uuid",
            string(vec![Some("turn-0"), Some("turn-1"), Some("turn-2")]),
        ),
        ("parent_uuid", string(vec![None; 3])),
        ("seq", number(vec![0, 1, 2])),
        ("ts", string(vec![Some("2026-01-01T00:00:00Z"); 3])),
        ("role", string(vec![Some("user"); 3])),
        ("block_type", string(vec![Some("text"); 3])),
        ("tool_name", string(vec![None; 3])),
        ("source_path", string(vec![Some("shallow.funes.jsonl"); 3])),
        ("block_idx", number(vec![0; 3])),
        ("split_idx", number(vec![0; 3])),
        ("vector", Arc::new(vectors)),
        ("harness", string(vec![Some("codex"); 3])),
        ("repo", string(vec![Some(""); 3])),
    ];
    let schema = Arc::new(Schema::new_with_metadata(
        columns
            .iter()
            .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
            .collect::<Vec<_>>(),
        HashMap::from([("embedding_model".to_string(), dataset::MODEL.to_string())]),
    ));
    let batch = RecordBatch::try_new(schema.clone(), columns.into_iter().map(|(_, array)| array).collect()).unwrap();
    let reader = RecordBatchIterator::new([Ok(batch)], schema);
    let mut ds = Dataset::write(reader, &dataset::table_uri(&memory), None)
        .await
        .unwrap();
    dataset::build_indexes(&mut ds, |_| {}).await.unwrap();
    assert_eq!(ds.count_rows(None).await.unwrap(), 3);
    assert_eq!(ds.count_rows(Some("vector IS NOT NULL".into())).await.unwrap(), 0);

    let (_, _, hits) = recall::recall_hits(
        Memory::parse(&memory),
        "narwhal".into(),
        5,
        30,
        30.0,
        1,
        Some("text".into()),
        Some("codex".into()),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(hits.len(), 1);
    let (hit, score) = &hits[0];
    assert_eq!(hit.session_id, "shallow-session");
    assert_eq!(hit.turn_uuid, "turn-1");
    assert_eq!(hit.text, texts[1]);
    assert!(score.is_finite() && *score > 0.0 && *score <= 1.0);
    assert_eq!(
        hit.neighbors
            .iter()
            .map(|neighbor| neighbor.text.as_str())
            .collect::<Vec<_>>(),
        [texts[0], texts[2]]
    );

    let empty = recall::recall(Memory::parse(&memory), "quasar".into(), 5, 30, 30.0, 1, None, None)
        .await
        .unwrap();
    assert_eq!(empty, "no results");
}
