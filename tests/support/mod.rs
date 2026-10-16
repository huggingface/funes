// Shared by the test binaries that drive `funes`; each uses the subset it needs.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{ArrayRef, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{Field, Schema};
use funes::commands::recall;
use funes::home;
use funes::inference::{self, EmbeddingModel};
use funes::memory::{dataset, Memory};
use hf_hub::{HFError, HFRepository, RepoTypeDataset};
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

/// Whether `bin arg` runs and succeeds: a tool a gated test needs is on `PATH`.
pub fn tool_ok(bin: &str, arg: &str) -> bool {
    Command::new(bin)
        .arg(arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A turns file under `tests/fixtures/funes_jsonl`.
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/funes_jsonl")
        .join(name)
}

/// Write `session`'s turns file under `source`: one user text turn a second per text, the n-th
/// turn `t<n>`, so a longer list grows the same session.
pub fn write_session(source: &Path, session: &str, texts: &[impl AsRef<str>]) {
    let mut f = std::fs::File::create(source.join(format!("{session}.funes.jsonl"))).unwrap();
    for (i, text) in texts.iter().enumerate() {
        let turn = serde_json::json!({
            "format": 1,
            "session_id": session,
            "cwd": "/home/u/dev/demo",
            "turn_uuid": format!("t{i}"),
            "seq": i,
            "ts": format!("2026-01-01T00:00:{i:02}Z"),
            "role": "user",
            "blocks": [{"block_type": "text", "text": text.as_ref()}],
            "harness": "claude",
        });
        writeln!(f, "{turn}").unwrap();
    }
}

/// Write `test-session-0001` under `source`: a question, an answer that runs `cargo test`, and the
/// run's result. Returns the session id and the `workdir` facet its munged `cwd` gives.
pub fn write_transcript(source: &Path) -> (String, String) {
    let workdir = "-home-u-dev-demo";
    let session = "test-session-0001";
    let mut f = std::fs::File::create(source.join(format!("{session}.funes.jsonl"))).unwrap();
    let lines = [
        r#"{"format":1,"session_id":"test-session-0001","cwd":"/home/u/dev/demo","turn_uuid":"t1","seq":0,"ts":"2026-01-01T00:00:00Z","role":"user","blocks":[{"block_type":"text","text":"how do we parse transcripts into turns"}],"harness":"claude"}"#,
        r#"{"format":1,"session_id":"test-session-0001","cwd":"/home/u/dev/demo","turn_uuid":"t2","parent_uuid":"t1","seq":1,"ts":"2026-01-01T00:00:05Z","role":"assistant","blocks":[{"block_type":"text","text":"We parse each JSONL line into a turn with typed blocks."},{"block_type":"tool_use","text":"{\"command\":\"cargo test\"}","tool_name":"Bash","tool_use_id":"c1"}],"harness":"claude"}"#,
        r#"{"format":1,"session_id":"test-session-0001","cwd":"/home/u/dev/demo","turn_uuid":"t3","parent_uuid":"t2","seq":2,"ts":"2026-01-01T00:00:10Z","role":"user","blocks":[{"block_type":"tool_result","text":"22 passed","tool_name":"Bash","tool_use_id":"c1"}],"harness":"claude"}"#,
    ];
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
    (session.to_string(), workdir.to_string())
}

/// The rows the local memory holds.
pub async fn stored_rows() -> usize {
    let ds = dataset::open(&dataset::table_uri(home::memory_dir()), Default::default())
        .await
        .expect("the memory exists");
    dataset::scan_rows(&ds, &["id"], None, None)
        .await
        .unwrap()
        .iter()
        .map(|b| b.num_rows())
        .sum()
}

/// The chunk count `status` reports for the local memory.
pub async fn chunk_count() -> usize {
    let s = recall::status(Memory::local()).await.unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix("chunks: "))
        .and_then(|n| n.trim().parse().ok())
        .expect("status reports a chunk count")
}

/// The fragments the local memory spans.
pub async fn fragments() -> usize {
    Memory::local().open().await.unwrap().get_fragments().len()
}

/// Index a one-turn session `s1` from `source` into the local memory, then append its rows 257
/// more times, each its own fragment, and index them all: 258 small indexed fragments.
pub async fn index_fragmented(source: &Path) {
    write_session(source, "s1", &["a memory split into many small fragments"]);
    funes::commands::index::run_index(source, false, None).await.unwrap();
    let mut ds = Memory::local().open().await.unwrap();
    let rows = dataset::scan_rows(&ds, &[], None, None).await.unwrap();
    let schema = Arc::new(Schema::from(ds.schema()));
    for _ in 0..257 {
        let reader = RecordBatchIterator::new(rows.clone().into_iter().map(Ok), schema.clone());
        ds.append(reader, None).await.unwrap();
    }
    dataset::build_indexes(&mut ds, |_| {}).await.unwrap();
    assert_eq!(fragments().await, 258);
}

/// A recall of the remote memory at `uri`, or the error it gave as the reply.
pub async fn recall_remote(uri: &str, query: &str) -> String {
    recall::recall(Memory::parse(uri), query.into(), 5, 30, 0, Default::default())
        .await
        .unwrap_or_else(|e| format!("<recall error: {e}>"))
}

/// The repo-root README, or None when the repo has none. Any other failure panics: a transport
/// error must fail the test loudly, not masquerade as "no README".
pub async fn root_readme(repo: &HFRepository<RepoTypeDataset>) -> Option<String> {
    match repo.download_file_to_bytes().filename("README.md").send().await {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(HFError::EntryNotFound { .. }) => None,
        Err(e) => panic!("querying the repo-root README: {e}"),
    }
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
