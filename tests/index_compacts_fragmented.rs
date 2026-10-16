//! An index run with nothing new still compacts a memory of many small indexed fragments.
//! Own test binary so its `$FUNES_HOME` can't race another integration test's.

mod support;

#[tokio::test]
async fn index_compacts_a_fragmented_memory_with_nothing_new() {
    let src = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db.path());
    support::index_fragmented(src.path()).await;

    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    assert_eq!(
        support::fragments().await,
        2,
        "the 257 fragments indexed together merge"
    );
}
