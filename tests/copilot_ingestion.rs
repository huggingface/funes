//! End-to-end coverage for the native GitHub Copilot `events.jsonl` source.
//!
//! Runs in its own test binary because `FUNES_HOME` is process-global.
//!
//! Fixture shape is based on github/copilot-sdk commit
//! `0cb0050ef4a6206808c7229ee11715f01bc256b0` and the GitHub Copilot CLI
//! streaming-events and CLI config-directory references retrieved 2026-09-11.

use std::io::Write;
use std::path::Path;

use funes::traces::harness::Harness;
use serde_json::{json, Value};

fn event(id: &str, kind: &str, data: Value) -> Value {
    json!({"id":id, "type":kind, "timestamp":"2026-09-11T00:00:00Z", "data":data})
}

fn append_events(path: &Path, events: &[Value]) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for record in events {
        writeln!(file, "{record}").unwrap();
    }
}

async fn index_copilot(root: &Path) {
    funes::commands::index::run_index_roots(&[(root.to_path_buf(), Some(Harness::Copilot))], false, None, true)
        .await
        .unwrap();
}

#[derive(Debug, PartialEq, Eq)]
struct StoredChunk {
    text: String,
    repo: String,
    workdir: String,
    turn_uuid: String,
}

async fn stored_content() -> std::collections::BTreeMap<String, StoredChunk> {
    use arrow_array::Array;
    let ds = funes::memory::Memory::local().open().await.unwrap();
    let batches = funes::memory::dataset::scan_rows(&ds, &["id", "text", "repo", "workdir", "turn_uuid"], None, None)
        .await
        .unwrap();
    let mut rows = std::collections::BTreeMap::new();
    for batch in batches {
        let column = |name: &str| {
            batch
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<arrow_array::StringArray>()
                .unwrap()
        };
        let ids = column("id");
        for i in 0..ids.len() {
            rows.insert(
                ids.value(i).to_owned(),
                StoredChunk {
                    text: column("text").value(i).to_owned(),
                    repo: column("repo").value(i).to_owned(),
                    workdir: column("workdir").value(i).to_owned(),
                    turn_uuid: column("turn_uuid").value(i).to_owned(),
                },
            );
        }
    }
    rows
}

#[tokio::test]
async fn copilot_native_events_index_incrementally_and_read_back() {
    let checkout = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["remote", "add", "origin", "https://github.com/current/funes.git"],
    ] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(checkout.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let source = tempfile::tempdir().unwrap();
    let events = source.path().join("events.jsonl");
    let session_id = "native-copilot-0001";
    std::fs::write(source.path().join("workspace.yaml"), "cwd: /work/fallback\n").unwrap();
    append_events(
        &events,
        &[
            event(
                "start",
                "session.start",
                json!({"sessionId":session_id, "context":{"cwd":checkout.path()}}),
            ),
            event(
                "user-1",
                "user.message",
                json!({"content":"How should Copilot transcripts be indexed?"}),
            ),
            event(
                "context",
                "session.context_changed",
                json!({"cwd":checkout.path(), "repository":"historical/funes"}),
            ),
            event(
                "assistant-1",
                "assistant.message",
                json!({
                    "content":"Index durable user, assistant, and tool events.",
                    "reasoningText":"Inspect the native event stream.",
                    "toolRequests":[{"toolCallId":"call-1","name":"shell","arguments":{"command":"cargo test"}}]
                }),
            ),
            event(
                "tool-1",
                "tool.execution_complete",
                json!({
                    "toolCallId":"call-1","result":{"content":"brief","detailedContent":"tool-result marker"}
                }),
            ),
        ],
    );
    let incremental_memory = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", incremental_memory.path());
    index_copilot(source.path()).await;
    let initial = stored_content().await;
    assert!(initial.values().any(|chunk| chunk.turn_uuid == "user-1"));
    let original_workdir = funes::traces::jsonl::workdir_of_cwd(checkout.path().to_str().unwrap()).unwrap();
    for chunk in initial.values() {
        let expected = if chunk.turn_uuid == "user-1" {
            "current/funes"
        } else {
            "historical/funes"
        };
        assert_eq!(chunk.repo, expected, "repository attribution for {}", chunk.turn_uuid);
        assert_eq!(
            chunk.workdir, original_workdir,
            "recorded cwd takes precedence over workspace.yaml"
        );
    }

    // A continuation changes repository context to a checkout that no longer exists.
    let missing_checkout = checkout.path().join("missing-checkout");
    append_events(
        &events,
        &[
            event(
                "context-change",
                "session.context_changed",
                json!({"cwd":missing_checkout,"repository":"historical/other"}),
            ),
            event(
                "user-2",
                "user.message",
                json!({"content":"Then verify incremental indexing and idempotence."}),
            ),
        ],
    );
    index_copilot(source.path()).await;
    let incremental = stored_content().await;
    assert!(incremental.len() > initial.len(), "appending a turn adds chunks");
    for (id, chunk) in &initial {
        assert_eq!(
            incremental.get(id),
            Some(chunk),
            "earlier content and attribution changed"
        );
    }
    let appended = incremental.values().find(|chunk| chunk.turn_uuid == "user-2").unwrap();
    assert_eq!(
        appended.repo, "historical/other",
        "recorded attribution survives a missing checkout"
    );
    assert_eq!(
        appended.workdir,
        funes::traces::jsonl::workdir_of_cwd(missing_checkout.to_str().unwrap()).unwrap()
    );

    let memory = funes::memory::Memory::local();
    let recalled = funes::commands::recall::recall(
        memory.clone(),
        "Copilot transcripts indexed durable events".into(),
        5,
        30,
        0.0,
        1,
        None,
        Some("copilot".into()),
    )
    .await
    .unwrap();
    assert!(
        recalled.contains(session_id),
        "harness-filtered recall missed session: {recalled}"
    );
    let got = funes::commands::recall::get(memory, session_id.into(), funes::commands::recall::TurnRange::default())
        .await
        .unwrap();
    assert!(got.contains("tool-result marker"), "get omitted tool output: {got}");

    index_copilot(source.path()).await;
    assert_eq!(stored_content().await, incremental, "unchanged indexing is a no-op");
    let fresh_memory = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", fresh_memory.path());
    index_copilot(source.path()).await;
    assert_eq!(
        stored_content().await,
        incremental,
        "incremental IDs, text, and attribution match a fresh index"
    );
}
