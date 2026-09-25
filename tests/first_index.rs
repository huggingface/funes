//! The `funes add` seed drives the budgeted drain end to end: a small history finishes whole
//! within the budget, its spool copy is dropped, a rerun is a no-op, and a deleted memory rebuilds
//! from a source that still holds the session. Own test binary so its `$FUNES_HOME` can't race the
//! other integration tests'.

use funes::traces::spool;
use std::io::Write;

/// A session with a user text turn, an assistant `tool_use`, and a `tool_result` — one block in
/// each tier.
fn write_session(source: &std::path::Path) {
    let mut f = std::fs::File::create(source.join("sess-0001.funes.jsonl")).unwrap();
    for l in [
        r#"{"format":1,"session_id":"sess-0001","cwd":"/home/u/dev/demo","turn_uuid":"t0","seq":0,"ts":"2026-01-01T00:00:00Z","role":"user","blocks":[{"block_type":"text","text":"decide how to parse transcripts and index them into lancedb"}],"harness":"claude"}"#,
        r#"{"format":1,"session_id":"sess-0001","cwd":"/home/u/dev/demo","turn_uuid":"t1","parent_uuid":"t0","seq":1,"ts":"2026-01-01T00:00:01Z","role":"assistant","blocks":[{"block_type":"tool_use","text":"{\"command\":\"ls the project directory tree\"}","tool_name":"Bash","tool_use_id":"c1"}],"harness":"claude"}"#,
        r#"{"format":1,"session_id":"sess-0001","cwd":"/home/u/dev/demo","turn_uuid":"t2","parent_uuid":"t1","seq":2,"ts":"2026-01-01T00:00:02Z","role":"user","blocks":[{"block_type":"tool_result","text":"a long directory listing output with many files","tool_name":"Bash","tool_use_id":"c1"}],"harness":"claude"}"#,
    ] {
        writeln!(f, "{l}").unwrap();
    }
}

async fn chunk_count() -> usize {
    let s = funes::commands::recall::status(funes::memory::Memory::local())
        .await
        .unwrap();
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
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    // Where the agent's integration converts into. funes finds it by listing the spool root, so a
    // stray file or a directory no id could name is not a producer; `--harness <id>` selects one
    // that exists.
    let src = spool::spool_dir("clyde");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(spool::spool_root().join("Not An Id")).unwrap();
    std::fs::write(spool::spool_root().join(".DS_Store"), b"").unwrap();
    assert_eq!(spool::spools(), vec![src.clone()]);
    assert_eq!(spool::select("clyde").unwrap(), src);
    // Refused as an install this funes does not match; the wording is stale_install_notice's.
    let err = spool::select("bonnie").unwrap_err().to_string();
    assert!(err.contains("the bonnie integration does not match"), "{err}");
    let err = spool::select("Not An Id").unwrap_err().to_string();
    assert!(err.contains("not an integration id"), "{err}");
    write_session(&src);

    // The seed `funes add` runs: budgeted, tier-major. This history fits the budget, so every
    // tier lands and the unit is stamped at the top one.
    funes::commands::index::run_index_seed(&src).await.unwrap();
    let full = chunk_count().await;
    assert!(full > 0, "seed indexed the session");
    assert_eq!(
        state_level(home.path()),
        "ToolResult",
        "a finished seed records the top tier"
    );
    assert!(
        !src.join("sess-0001.funes.jsonl").exists(),
        "funes drains a spool file it has taken to the top tier"
    );

    // The budgeted no-path run (the per-turn hook): nothing owed, nothing added.
    let roots = [src.clone()];
    funes::commands::index::run_index_budgeted(&roots, false, None, false)
        .await
        .unwrap();
    assert_eq!(chunk_count().await, full, "rerun adds nothing");

    // A deleted memory self-heals: the memory dir is gone but state.json survived — the next run
    // must re-index everything, not trust the stale state and skip against an empty memory. The
    // drained spool has nothing left to rebuild from, so this runs over a turns directory funes does
    // not own: the same session, so the same chunks.
    let kept = home.path().join("elsewhere");
    std::fs::create_dir_all(&kept).unwrap();
    write_session(&kept);
    let roots = [kept.clone()];
    funes::commands::index::run_index_budgeted(&roots, false, None, false)
        .await
        .unwrap();
    assert_eq!(chunk_count().await, full, "the same session is the same chunks");
    assert!(kept.join("sess-0001.funes.jsonl").exists(), "and it is left alone");

    std::fs::remove_dir_all(home.path().join("memory")).unwrap();
    funes::commands::index::run_index_budgeted(&roots, false, None, false)
        .await
        .unwrap();
    assert_eq!(chunk_count().await, full, "deleted memory rebuilt in full");
}
