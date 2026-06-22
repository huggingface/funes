//! Re-indexing a grown session adds only the new turns (continuation = same memory) and
//! converges to exactly the state of indexing the final session from scratch — no duplication,
//! no loss. Own test binary so its `$FUNES_DB` can't race the other integration test's.

use std::io::Write;

/// Write a `<source>/projects/<project>/<session>.jsonl` with `n_turns` user turns, each with
/// distinct content (so each is its own chunk). Appending turns later grows the same session.
fn write_session(source: &std::path::Path, n_turns: usize) {
    let dir = source.join("projects").join("-home-u-dev-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("grow-session-0001.jsonl")).unwrap();
    for i in 0..n_turns {
        let line = format!(
            r#"{{"type":"user","uuid":"turn{i}","timestamp":"2026-01-01T00:00:{i:02}Z","message":{{"role":"user","content":"turn {i} about parsing transcripts and lancedb indexing"}}}}"#
        );
        writeln!(f, "{line}").unwrap();
    }
}

async fn chunk_count() -> usize {
    let s = funes::recall::status(funes::hub::Store::local()).await.unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix("chunks: "))
        .and_then(|n| n.trim().parse().ok())
        .expect("status reports a chunk count")
}

#[tokio::test]
async fn incremental_reindex_matches_from_scratch() {
    // Incremental: index 4 turns, grow the same session to 6, re-index.
    let inc_src = tempfile::tempdir().unwrap();
    let inc_db = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_DB", inc_db.path());
    write_session(inc_src.path(), 4);
    funes::index::run_index(inc_src.path(), false).await.unwrap();
    let after_first = chunk_count().await;
    assert!(after_first > 0, "first index produced no chunks");

    write_session(inc_src.path(), 6); // append 2 turns
    funes::index::run_index(inc_src.path(), false).await.unwrap();
    let incremental = chunk_count().await;
    assert!(
        incremental > after_first,
        "the 2 appended turns should add chunks: {after_first} -> {incremental}"
    );

    // From scratch: the same 6-turn session in a fresh db.
    let scratch_src = tempfile::tempdir().unwrap();
    let scratch_db = tempfile::tempdir().unwrap();
    write_session(scratch_src.path(), 6);
    std::env::set_var("FUNES_DB", scratch_db.path());
    funes::index::run_index(scratch_src.path(), false).await.unwrap();
    let from_scratch = chunk_count().await;

    assert_eq!(
        incremental, from_scratch,
        "incremental re-index must converge to the from-scratch state (no dup, no loss)"
    );
}
