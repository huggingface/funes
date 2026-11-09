//! The exchange frame recall attaches to each hit, and what it leaves out.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{ArrayRef, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{Field, Schema};
use funes::commands::recall;
use funes::memory::{dataset, Memory};
use lance::Dataset;

/// One chunk row of session `s`: (seq, role, block_type, block_idx, split_idx, text).
type Row = (i64, &'static str, &'static str, i64, i64, &'static str);

/// A memory of the one session `s` made of `rows`, text-indexed, its embeddings all pending.
async fn memory_of(rows: &[Row], memory: &str) {
    let n = rows.len();
    let string = |values: Vec<Option<String>>| -> ArrayRef { Arc::new(StringArray::from(values)) };
    let number = |values: Vec<i64>| -> ArrayRef { Arc::new(Int64Array::from(values)) };
    let vectors =
        FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(vec![None::<Vec<Option<f32>>>; n], dataset::DIM);
    let columns: Vec<(&str, ArrayRef)> = vec![
        ("id", string((0..n).map(|i| Some(format!("row-{i}"))).collect())),
        ("text", string(rows.iter().map(|r| Some(r.5.to_string())).collect())),
        ("session_id", string(vec![Some("s".to_string()); n])),
        ("workdir", string(vec![Some("demo".to_string()); n])),
        (
            "turn_uuid",
            string(rows.iter().map(|r| Some(format!("t{}", r.0))).collect()),
        ),
        ("parent_uuid", string(vec![None; n])),
        ("seq", number(rows.iter().map(|r| r.0).collect())),
        ("ts", string(vec![Some("2026-01-01T00:00:00Z".to_string()); n])),
        ("role", string(rows.iter().map(|r| Some(r.1.to_string())).collect())),
        (
            "block_type",
            string(rows.iter().map(|r| Some(r.2.to_string())).collect()),
        ),
        ("tool_name", string(vec![None; n])),
        ("source_path", string(vec![Some("frame.funes.jsonl".to_string()); n])),
        ("block_idx", number(rows.iter().map(|r| r.3).collect())),
        ("split_idx", number(rows.iter().map(|r| r.4).collect())),
        ("vector", Arc::new(vectors)),
        ("harness", string(vec![Some("codex".to_string()); n])),
        ("repo", string(vec![Some(String::new()); n])),
    ];
    let schema = Arc::new(Schema::new_with_metadata(
        columns
            .iter()
            .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
            .collect::<Vec<_>>(),
        HashMap::from([("embedding_model".to_string(), dataset::model::ENGLISH.id().to_string())]),
    ));
    let batch = RecordBatch::try_new(schema.clone(), columns.into_iter().map(|(_, array)| array).collect()).unwrap();
    let reader = RecordBatchIterator::new([Ok(batch)], schema);
    let mut ds = Dataset::write(reader, &dataset::table_uri(memory), None).await.unwrap();
    dataset::build_indexes(&mut ds, |_| {}).await.unwrap();
}

/// The hits a query with `block_type` has in `memory`, best first.
async fn hits_of(memory: &str, query: &str, block_type: &str) -> Vec<recall::Hit> {
    let (_, hits) = recall::recall_hits(
        Memory::parse(memory),
        query.into(),
        5,
        30,
        recall::RecallFilter {
            block_type: Some(block_type.into()),
            ..Default::default()
        },
        &|_| {},
    )
    .await
    .unwrap();
    hits.into_iter().map(|(hit, _)| hit).collect()
}

/// The one hit a query with `block_type` has in `memory`.
async fn hit_of(memory: &str, query: &str, block_type: &str) -> recall::Hit {
    let mut hits = hits_of(memory, query, block_type).await;
    assert_eq!(hits.len(), 1, "one hit for {query}");
    hits.remove(0)
}

/// The frame as (seq, role, text) rows.
fn frame(hit: &recall::Hit) -> Vec<(i64, &str, &str)> {
    hit.exchange
        .iter()
        .map(|t| (t.seq, t.role.as_str(), t.text.as_str()))
        .collect()
}

const SUMMARY: &str = "The parser reads each narwhal line into a turn.";

const ROWS: &[Row] = &[
    (
        0,
        "user",
        "text",
        0,
        0,
        "<system-reminder>a skill preamble</system-reminder>",
    ),
    (0, "user", "text", 1, 0, "please fix the parser"),
    (1, "assistant", "tool_use", 0, 0, "[tool_use Bash] grep walrus src"),
    (1, "assistant", "text", 1, 0, SUMMARY),
    (2, "user", "tool_result", 0, 0, "[tool_result Bash] src/parser.rs"),
    (
        3,
        "user",
        "text",
        0,
        0,
        "<system-reminder>a reminder on its own</system-reminder>",
    ),
    (4, "user", "text", 0, 0, "ship it"),
];

#[tokio::test]
async fn a_hit_is_framed_by_its_exchange_including_its_own_turn() {
    let home = tempfile::tempdir().unwrap();
    let memory = home.path().join("memory").to_string_lossy().into_owned();
    memory_of(ROWS, &memory).await;

    let hit = hit_of(&memory, "walrus", "tool_use").await;
    assert_eq!(hit.seq, 1);
    assert_eq!(
        frame(&hit),
        [
            (0, "user", "please fix the parser"),
            (1, "assistant", SUMMARY),
            (4, "user", "ship it")
        ]
    );

    let hit = hit_of(&memory, "narwhal", "text").await;
    assert_eq!(hit.text, SUMMARY);
    assert_eq!(
        frame(&hit),
        [(0, "user", "please fix the parser"), (4, "user", "ship it")]
    );
}

#[tokio::test]
async fn a_turn_shown_as_a_hit_frames_no_other_hit() {
    let home = tempfile::tempdir().unwrap();
    let memory = home.path().join("memory").to_string_lossy().into_owned();
    memory_of(ROWS, &memory).await;

    let hits = hits_of(&memory, "parser", "text").await;
    let mut seqs: Vec<i64> = hits.iter().map(|h| h.seq).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, [0, 1], "the prompt and the summary are both hits");
    let attached: Vec<Vec<i64>> = hits
        .iter()
        .map(|h| h.exchange.iter().map(|t| t.seq).collect())
        .collect();
    assert_eq!(
        attached.concat(),
        [4],
        "the next prompt is attached once and the hits frame each other with nothing: {attached:?}"
    );
}
