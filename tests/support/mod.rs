// Shared by the test binaries that drive `funes`; each uses the subset it needs.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{ArrayRef, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{Field, Schema};
use funes::inference::{self, EmbeddingModel};
use funes::memory::dataset;
use lance::Dataset;

/// The model cache the embedder and reranker read, for a run whose `$HOME` is fake.
pub fn hf_home() -> PathBuf {
    std::env::var_os("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("a home")).join(".cache/huggingface"))
}

pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .map(|e| e.unwrap().into_path())
        .filter(|p| p.is_file())
        .filter(|p| {
            std::fs::read(p)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle.as_bytes())
        })
        .collect()
}

/// Sessions are `s-0`, `s-1`, …, one per text.
pub async fn memory_of(model: EmbeddingModel, texts: &[&str], memory: &Path) {
    let memory = memory.to_string_lossy().into_owned();
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
}
