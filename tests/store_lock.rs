//! The in-binary memory lock serializes local-memory mutations, failing loudly on contention. Its own test
//! binary so its `$FUNES_HOME` can't race another integration test's. `flock` treats independent
//! `open()`s of the same file as contending — even within one process (flock(2)) — so a single
//! process can hold the lock and then observe the contention paths without spawning `funes`
//! subprocesses.

use funes::memory::lock::MemoryLock;

#[tokio::test]
async fn memory_lock_fails_loudly_on_contention() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());

    // Hold the memory lock, then observe every writer refuse rather than wait or skip.
    let held = MemoryLock::acquire().unwrap();

    // A second acquire fails loudly.
    let err = MemoryLock::acquire().unwrap_err();
    assert!(
        err.to_string()
            .contains("another funes memory operation is in progress"),
        "acquire should report contention, got: {err}"
    );

    // scrub refuses while the lock is held (its guard is the same acquire).
    let err = funes::commands::scrub::run().await.unwrap_err();
    assert!(
        err.to_string()
            .contains("another funes memory operation is in progress"),
        "scrub should report contention, got: {err}"
    );

    // A writer reaching the memory by its path, from any funes home, refuses too.
    let memory = home.path().join("memory");
    assert!(MemoryLock::acquire_in(&memory).is_err());

    // Releasing frees it for the next writer.
    drop(held);
    let regained = MemoryLock::acquire().unwrap();
    drop(regained);

    // And a writer holding the memory by its path keeps the home's writers out.
    let by_path = MemoryLock::acquire_in(&memory).unwrap();
    assert!(MemoryLock::acquire().is_err());
    drop(by_path);
}
