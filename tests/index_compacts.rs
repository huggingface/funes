//! An index run merges the fragments it wrote (one per session) and leaves the indexed ones alone.
//! Own test binary so its `$FUNES_HOME` can't race another integration test's.

mod support;

#[tokio::test]
async fn index_merges_the_fragments_it_wrote() {
    let src = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db.path());

    for id in ["s1", "s2", "s3"] {
        support::write_session(
            src.path(),
            id,
            &[format!("session {id} about compacting lance fragments")],
        );
    }
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    assert_eq!(support::fragments().await, 1, "three sessions, one fragment");

    for id in ["s4", "s5"] {
        support::write_session(
            src.path(),
            id,
            &[format!("session {id} about compacting lance fragments")],
        );
    }
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    assert_eq!(
        support::fragments().await,
        2,
        "the indexed fragment stays, the new two merge"
    );
}
