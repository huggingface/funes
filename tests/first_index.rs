//! The `funes add` seed drives the budgeted drain end to end: a small history finishes whole
//! within the budget, a rerun is a no-op, and a deleted store self-heals. Own test binary so its
//! `$FUNES_HOME` can't race the other integration tests'.

use std::io::Write;

/// A session with a user text turn, an assistant `tool_use`, and a `tool_result` — one block in
/// each tier.
fn write_session(source: &std::path::Path) {
    let dir = source.join("projects").join("-home-u-dev-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("sess-0001.jsonl")).unwrap();
    for l in [
        r#"{"type":"user","uuid":"t0","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"decide how to parse transcripts and index them into lancedb"}}"#,
        r#"{"type":"assistant","uuid":"t1","parentUuid":"t0","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"Bash","input":{"command":"ls the project directory tree"}}]}}"#,
        r#"{"type":"user","uuid":"t2","parentUuid":"t1","timestamp":"2026-01-01T00:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":[{"type":"text","text":"a long directory listing output with many files"}]}]}}"#,
    ] {
        writeln!(f, "{l}").unwrap();
    }
}

async fn chunk_count() -> usize {
    let s = funes::recall::status(funes::hub::Store::local()).await.unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix("chunks: "))
        .and_then(|n| n.trim().parse().ok())
        .expect("status reports a chunk count")
}

fn state_level(home: &std::path::Path) -> String {
    let s = std::fs::read_to_string(home.join("state.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    v.as_object()
        .and_then(|m| m.values().next())
        .and_then(|e| e["level"].as_str())
        .expect("state.json entry has a level")
        .to_string()
}

#[tokio::test]
async fn seed_finishes_a_small_history_and_a_rerun_is_a_noop() {
    let src = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    write_session(src.path());

    // The seed `funes add` runs: budgeted, tier-major. This history fits the budget, so every
    // tier lands and the unit is stamped at the top one.
    funes::index::run_index_seed(src.path(), funes::harness::Harness::Claude)
        .await
        .unwrap();
    let full = chunk_count().await;
    assert!(full > 0, "seed indexed the session");
    assert_eq!(
        state_level(home.path()),
        "ToolResult",
        "a finished seed records the top tier"
    );

    // The budgeted no-path run (the per-turn hook): nothing owed, nothing added.
    let roots = [(src.path().to_path_buf(), Some(funes::harness::Harness::Claude))];
    funes::index::run_index_budgeted(&roots, false, None, false)
        .await
        .unwrap();
    assert_eq!(chunk_count().await, full, "rerun adds nothing");

    // A deleted store self-heals: the store dir is gone but state.json survived — the next run
    // must re-index everything, not trust the stale state and skip against an empty store.
    std::fs::remove_dir_all(home.path().join("store")).unwrap();
    funes::index::run_index_budgeted(&roots, false, None, false)
        .await
        .unwrap();
    assert_eq!(chunk_count().await, full, "deleted store rebuilt in full");
}
