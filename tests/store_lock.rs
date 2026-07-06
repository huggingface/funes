//! The in-binary store lock serializes local-store mutations, failing loudly on contention. Its own test
//! binary so its `$FUNES_HOME` can't race another integration test's. `flock` treats independent
//! `open()`s of the same file as contending — even within one process (flock(2)) — so a single
//! process can hold the lock and then observe the contention paths without spawning `funes`
//! subprocesses.

use funes::lock::StoreLock;

#[tokio::test]
async fn store_lock_fails_loudly_on_contention() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());

    // Hold the store lock, then observe every writer refuse rather than wait or skip.
    let held = StoreLock::acquire().unwrap();

    // A second acquire fails loudly.
    let err = StoreLock::acquire().unwrap_err();
    assert!(
        err.to_string().contains("another funes store operation is in progress"),
        "acquire should report contention, got: {err}"
    );

    // scrub refuses while the lock is held (its guard is the same acquire).
    let err = funes::scrub::run().await.unwrap_err();
    assert!(
        err.to_string().contains("another funes store operation is in progress"),
        "scrub should report contention, got: {err}"
    );

    // Releasing frees it for the next writer.
    drop(held);
    let regained = StoreLock::acquire().unwrap();
    drop(regained);
}
