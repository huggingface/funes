//! Recall over a memory embedded with multilingual-e5-small, through the real models.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{ArrayRef, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{Field, Schema};
use funes::commands::recall;
use funes::inference::{self, EmbeddingModel};
use funes::memory::{dataset, Memory};
use lance::Dataset;
use tempfile::TempDir;

async fn memory_of(model: EmbeddingModel, texts: &[&str]) -> (TempDir, Memory) {
    let home = tempfile::tempdir().unwrap();
    let memory = home.path().join("memory").to_string_lossy().into_owned();
    let n = texts.len();
    let embedded = inference::embedder(model).unwrap().embed(texts).unwrap();
    let ids: Vec<String> = (0..n).map(|i| format!("row-{i}")).collect();
    let sessions: Vec<String> = (0..n).map(|i| format!("s-{i}")).collect();
    let string = |values: Vec<Option<&str>>| -> ArrayRef { Arc::new(StringArray::from(values)) };
    let number = |values: Vec<i64>| -> ArrayRef { Arc::new(Int64Array::from(values)) };
    let vectors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        embedded.into_iter().map(|v| Some(v.into_iter().map(Some))),
        dataset::DIM,
    );
    let columns: Vec<(&str, ArrayRef)> = vec![
        ("id", string(ids.iter().map(|s| Some(s.as_str())).collect())),
        ("text", string(texts.iter().copied().map(Some).collect())),
        (
            "session_id",
            string(sessions.iter().map(|s| Some(s.as_str())).collect()),
        ),
        ("workdir", string(vec![Some("demo"); n])),
        ("turn_uuid", string(vec![Some("turn-0"); n])),
        ("parent_uuid", string(vec![None; n])),
        ("seq", number(vec![0; n])),
        ("ts", string(vec![Some("2026-01-01T00:00:00Z"); n])),
        ("role", string(vec![Some("user"); n])),
        ("block_type", string(vec![Some("text"); n])),
        ("tool_name", string(vec![None; n])),
        ("source_path", string(vec![Some("zh.funes.jsonl"); n])),
        ("block_idx", number(vec![0; n])),
        ("split_idx", number(vec![0; n])),
        ("vector", Arc::new(vectors)),
        ("harness", string(vec![Some("codex"); n])),
        ("repo", string(vec![Some(""); n])),
    ];
    let schema = Arc::new(Schema::new_with_metadata(
        columns
            .iter()
            .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
            .collect::<Vec<_>>(),
        HashMap::from([("embedding_model".to_string(), model.id().to_string())]),
    ));
    let batch = RecordBatch::try_new(schema.clone(), columns.into_iter().map(|(_, array)| array).collect()).unwrap();
    let reader = RecordBatchIterator::new([Ok(batch)], schema);
    let mut ds = Dataset::write(reader, &dataset::table_uri(&memory), None)
        .await
        .unwrap();
    dataset::build_indexes(&mut ds, |_| {}).await.unwrap();
    (home, Memory::parse(&memory))
}

#[tokio::test]
async fn recall_embeds_the_query_with_the_memorys_model() {
    let (_home, memory) = memory_of(
        EmbeddingModel::MultilingualE5Small,
        &[
            "给表加上索引",
            "周末我们和几个老朋友一起开车去郊外的山上徒步露营",
            "这只橘色的猫每天早上六点准时跑到厨房门口等着吃鱼",
            "奶奶在厨房里一边听收音机一边包了两百多个饺子",
            "火车因为大雪晚点了两个多小时，站台上挤满了人",
            "孩子们放学后在学校的操场上踢足球一直踢到天黑",
            "他把用了五年的旧手机擦干净送给了上大学的弟弟",
            "今年夏天雨水特别多，河边的路被淹了好几次",
        ],
    )
    .await;

    // No query token is a chunk token and the pool is too small to rerank: only the vectors decide.
    let (_, hits) = recall::recall_hits(memory, "怎样让查询变快".into(), 1, 3, 0, Default::default(), &|_| {})
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.session_id, "s-0", "{}", hits[0].0.text);
}

#[tokio::test]
async fn a_search_reads_memories_of_one_model() {
    let (_e5_home, e5) = memory_of(EmbeddingModel::MultilingualE5Small, &["给表加上索引"]).await;
    let (_bge_home, bge) = memory_of(EmbeddingModel::BgeSmallEn, &["add an index to the table"]).await;
    let quiet = |_: &str| ();
    let search = recall::Search::new("怎样让查询变快".into(), 3, Default::default()).unwrap();
    search.candidates(&e5, &quiet).await.unwrap();
    let err = search.candidates(&bge, &quiet).await.err().unwrap().to_string();
    assert!(
        err.contains(EmbeddingModel::BgeSmallEn.id()) && err.contains(EmbeddingModel::MultilingualE5Small.id()),
        "{err}"
    );
}
