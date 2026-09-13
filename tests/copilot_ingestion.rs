//! End-to-end coverage for the native GitHub Copilot `events.jsonl` source.
//!
//! This test owns its memory directory because indexing and the read surface use process-global
//! `FUNES_HOME`, and because the integration suite runs test binaries concurrently.
//!
//! Fixture shape is based on github/copilot-sdk commit
//! `0cb0050ef4a6206808c7229ee11715f01bc256b0` and the GitHub Copilot CLI
//! streaming-events and CLI config-directory references retrieved 2026-09-11.

use std::io::Write;
use std::path::Path;

use funes::traces::harness::Harness;
use serde_json::{json, Value};

fn event(id: &str, kind: &str, timestamp: &str, data: Value) -> Value {
    json!({
        "id": id,
        "parentId": "previous",
        "timestamp": timestamp,
        "type": kind,
        "data": data,
    })
}

fn write_session(root: &Path, include_appended_turn: bool) {
    let session = root.join("copilot-session-0001");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join("workspace.yaml"), "cwd: /work/funes-copilot\n").unwrap();

    let mut events = vec![
        event(
            "session-start",
            "session.start",
            "2026-09-11T00:00:00Z",
            json!({"sessionId":"native-copilot-0001","context":{"cwd":"/work/funes-copilot"}}),
        ),
        event(
            "user-1",
            "user.message",
            "2026-09-11T00:00:01Z",
            json!({"content":"How should Copilot transcripts be indexed?"}),
        ),
        event(
            "assistant-1",
            "assistant.message",
            "2026-09-11T00:00:02Z",
            json!({
                "content":"Index durable user, assistant, and tool events.",
                "reasoningText":"Inspect the native event stream.",
                "toolRequests":[{"toolCallId":"call-1","name":"shell","arguments":{"command":"cargo test"}}]
            }),
        ),
        event(
            "tool-1",
            "tool.execution_complete",
            "2026-09-11T00:00:03Z",
            json!({"toolCallId":"call-1","result":{"content":"brief","detailedContent":"tool-result marker"}}),
        ),
    ];
    if include_appended_turn {
        events.push(event(
            "user-2",
            "user.message",
            "2026-09-11T00:00:04Z",
            json!({"content":"Then verify incremental indexing and idempotence."}),
        ));
    }

    let mut file = std::fs::File::create(session.join("events.jsonl")).unwrap();
    for record in events {
        writeln!(file, "{record}").unwrap();
    }
}

async fn chunk_count() -> usize {
    let status = funes::commands::recall::status(funes::memory::Memory::local())
        .await
        .unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("chunks: "))
        .and_then(|n| n.trim().parse().ok())
        .expect("status reports a chunk count")
}

async fn stored_content() -> std::collections::BTreeMap<String, String> {
    use arrow_array::Array;
    let ds = funes::memory::Memory::local().open().await.unwrap();
    let batches = funes::memory::dataset::scan_rows(&ds, &["id", "text"], None, None)
        .await
        .unwrap();
    let mut rows = std::collections::BTreeMap::new();
    for batch in batches {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .unwrap();
        let text = batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .unwrap();
        for i in 0..ids.len() {
            rows.insert(ids.value(i).to_owned(), text.value(i).to_owned());
        }
    }
    rows
}

#[tokio::test]
async fn copilot_native_events_index_incrementally_and_read_back() {
    let incremental_source = tempfile::tempdir().unwrap();
    let incremental_memory = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", incremental_memory.path());
    write_session(incremental_source.path(), false);

    funes::commands::index::run_index_roots(
        &[(incremental_source.path().to_path_buf(), Some(Harness::Copilot))],
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let first_count = chunk_count().await;
    assert!(first_count > 0, "initial Copilot events produced no chunks");

    write_session(incremental_source.path(), true);
    funes::commands::index::run_index_roots(
        &[(incremental_source.path().to_path_buf(), Some(Harness::Copilot))],
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let incremental_count = chunk_count().await;
    assert!(
        incremental_count > first_count,
        "the appended event should add chunks: {first_count} -> {incremental_count}"
    );

    // Re-indexing the unchanged native session is idempotent.
    funes::commands::index::run_index_roots(
        &[(incremental_source.path().to_path_buf(), Some(Harness::Copilot))],
        false,
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(chunk_count().await, incremental_count);

    let session = "native-copilot-0001".to_string();
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
        recalled.contains(&session),
        "harness-filtered recall missed session: {recalled}"
    );
    assert!(
        recalled.contains("copilot"),
        "recall did not render the harness: {recalled}"
    );

    let listed = funes::commands::recall::sessions(memory.clone(), Default::default())
        .await
        .unwrap();
    assert!(
        listed.contains("copilot") && listed.contains(&session),
        "session listing: {listed}"
    );

    let got = funes::commands::recall::get(
        memory.clone(),
        session.clone(),
        funes::commands::recall::TurnRange::default(),
    )
    .await
    .unwrap();
    assert!(got.contains("tool-result marker"), "get omitted tool output: {got}");

    let sketched = funes::commands::sketch::run(memory.clone(), session.clone(), None, None, Some(20), Some(20_000))
        .await
        .unwrap();
    assert!(
        sketched.contains("tool_result (shell)"),
        "sketch omitted the correlated tool name: {sketched}"
    );

    let scanned = funes::commands::recall::scan(memory, "tool-result marker".into(), session, None, None, false, 40)
        .await
        .unwrap();
    assert!(
        scanned.contains("tool_result"),
        "scan omitted tool-result block type: {scanned}"
    );

    let incremental_content = stored_content().await;
    let scratch_source = tempfile::tempdir().unwrap();
    let scratch_memory = tempfile::tempdir().unwrap();
    write_session(scratch_source.path(), true);
    std::env::set_var("FUNES_HOME", scratch_memory.path());
    funes::commands::index::run_index_roots(
        &[(scratch_source.path().to_path_buf(), Some(Harness::Copilot))],
        false,
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        chunk_count().await,
        incremental_count,
        "incremental Copilot indexing must match from-scratch chunk count"
    );
    assert_eq!(
        stored_content().await,
        incremental_content,
        "incremental IDs and text must match a fresh index"
    );
}
