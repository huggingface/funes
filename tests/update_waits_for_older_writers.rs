//! `funes update` waits for a funes up to 1.6 to finish writing the home's memory: those lock
//! `store.lock` in the funes home, not the one in the memory. Own test binary so its `$FUNES_HOME`
//! can't race another integration test's.

use std::time::Duration;

use funes::commands::update;

#[tokio::test]
async fn update_waits_for_an_older_writer() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());

    // Nothing to wait for on a home no older binary ever locked.
    tokio::time::timeout(Duration::from_secs(5), update::wait_for_older_writers())
        .await
        .expect("no older writer")
        .unwrap();

    // An older binary's index holds the home's `store.lock`.
    let older = std::fs::File::create(home.path().join("store.lock")).unwrap();
    older.try_lock().unwrap();
    let wait = tokio::spawn(update::wait_for_older_writers());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !wait.is_finished(),
        "update must wait while an older writer holds its lock"
    );

    older.unlock().unwrap();
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("update resumes once the older writer is done")
        .unwrap()
        .unwrap();
}
