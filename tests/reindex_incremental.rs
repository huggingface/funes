//! Re-indexing a grown session adds only the new turns (continuation = same memory) and
//! converges to exactly the state of indexing the final session from scratch — no duplication,
//! no loss. Own test binary so its `$FUNES_HOME` can't race the other integration test's.

mod support;

/// `n` user turns, each with distinct content (so each is its own chunk). A longer list grows the
/// same session.
fn turns(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("turn {i} about parsing transcripts and lancedb indexing"))
        .collect()
}

#[tokio::test]
async fn incremental_reindex_matches_from_scratch() {
    // Incremental: index 4 turns, grow the same session to 6, re-index.
    let inc_src = tempfile::tempdir().unwrap();
    let inc_db = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", inc_db.path());
    support::write_session(inc_src.path(), "grow-session-0001", &turns(4));
    funes::commands::index::run_index(inc_src.path(), false, None)
        .await
        .unwrap();
    let after_first = support::chunk_count().await;
    assert!(after_first > 0, "first index produced no chunks");

    support::write_session(inc_src.path(), "grow-session-0001", &turns(6)); // append 2 turns
    funes::commands::index::run_index(inc_src.path(), false, None)
        .await
        .unwrap();
    let incremental = support::chunk_count().await;
    assert!(
        incremental > after_first,
        "the 2 appended turns should add chunks: {after_first} -> {incremental}"
    );

    // From scratch: the same 6-turn session in a fresh db.
    let scratch_src = tempfile::tempdir().unwrap();
    let scratch_db = tempfile::tempdir().unwrap();
    support::write_session(scratch_src.path(), "grow-session-0001", &turns(6));
    std::env::set_var("FUNES_HOME", scratch_db.path());
    funes::commands::index::run_index(scratch_src.path(), false, None)
        .await
        .unwrap();
    let from_scratch = support::chunk_count().await;

    assert_eq!(
        incremental, from_scratch,
        "incremental re-index must converge to the from-scratch state (no dup, no loss)"
    );
}
