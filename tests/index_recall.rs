//! End-to-end: build a real index from a tiny transcript in a temp dir, then exercise
//! the read surface (recall / get / list / status). No mocking — this runs the real
//! BGE embedder + reranker (downloaded to the fastembed cache on first run) against a
//! real lancedb store under a temp `$FUNES_HOME`.

use std::io::Write;

/// Write a `<source>/projects/<project>/<session>.jsonl` transcript so `project_of` /
/// `session_id_of` resolve the way they do for real Claude Code projects.
fn write_transcript(source: &std::path::Path) -> (String, String) {
    let project = "-home-u-dev-demo";
    let session = "test-session-0001";
    let dir = source.join("projects").join(project);
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
    let lines = [
        r#"{"type":"user","uuid":"t1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"how do we parse transcripts into turns"}}"#,
        r#"{"type":"assistant","uuid":"t2","parentUuid":"t1","timestamp":"2026-01-01T00:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"We parse each JSONL line into a turn with typed blocks."},{"type":"tool_use","id":"c1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
        r#"{"type":"user","uuid":"t3","parentUuid":"t2","timestamp":"2026-01-01T00:00:10Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":[{"type":"text","text":"22 passed"}]}]}}"#,
    ];
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
    (session.to_string(), project.to_string())
}

#[tokio::test]
async fn index_then_read_surface() {
    let db_dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    // db::funes_dir() reads $FUNES_HOME; point the whole read/write surface at the temp dir.
    std::env::set_var("FUNES_HOME", db_dir.path());
    let (session, project) = write_transcript(source.path());

    // Build the index for real: parse → chunk → embed → lancedb + FTS.
    funes::index::run_index(source.path(), false).await.unwrap();

    // status: non-empty chunk count.
    let status = funes::recall::status(funes::hub::Store::local()).await.unwrap();
    assert!(status.contains("chunks:"), "status missing chunk count: {status}");

    // list: the session appears under its project.
    let list = funes::recall::list(funes::hub::Store::local(), None, 50).await.unwrap();
    assert!(list.contains(&project), "list should name the project: {list}");

    // recall: the parsing turn surfaces, and the `→ get` line carries the full session id.
    let out = funes::recall::recall(
        funes::hub::Store::local(),
        "parse transcripts into turns".into(),
        5,
        30,
        30.0,
        1,
        None,
        None,
    )
    .await
    .unwrap();
    assert_ne!(out, "no results", "recall returned nothing");
    assert!(
        out.contains(&session),
        "recall should surface the indexed session: {out}"
    );

    // type filter: restrict to tool_use → the Bash call.
    let tu = funes::recall::recall(
        funes::hub::Store::local(),
        "cargo test".into(),
        5,
        30,
        0.0,
        0,
        Some("tool_use".into()),
        None,
    )
    .await
    .unwrap();
    assert!(tu.contains("tool_use"), "type filter should keep tool_use rows: {tu}");

    // get: reassemble the assistant turn by its uuid.
    let got = funes::recall::get(funes::hub::Store::local(), session.clone(), "t2".into(), 3)
        .await
        .unwrap();
    assert!(got.contains("typed blocks"), "get should return the turn text: {got}");
}
