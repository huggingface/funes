//! A fresh install with no personal index: the read verbs point the user at `funes add` (the
//! onboarding) instead of returning canned content — there is no built-in corpus anymore. Points
//! `$FUNES_HOME` at an empty temp dir and exercises the no-index paths.

use funes::hub::Memory;

#[tokio::test]
async fn recall_without_an_index_guides_to_funes_add() {
    let empty = tempfile::tempdir().unwrap();
    // $FUNES_HOME points at an empty dir: Memory::local() resolves there with no dataset to open.
    std::env::set_var("FUNES_HOME", empty.path());

    // recall: no index → a clear, actionable error (not canned corpus, not a leaked lance path).
    let err = funes::recall::recall(
        Memory::local(),
        "how do I connect funes to claude code".into(),
        5,
        30,
        30.0,
        1,
        None,
        None,
    )
    .await
    .expect_err("recall with no index should error, not return canned content")
    .to_string();
    assert!(err.contains("funes add"), "recall should point at funes add: {err}");

    // status: informational — reports no index and points at the onboarding command (does not error).
    let status = funes::recall::status(Memory::local()).await.unwrap();
    assert!(
        status.contains("no index yet"),
        "status should report no index: {status}"
    );
    assert!(
        status.contains("funes add"),
        "status should point at funes add: {status}"
    );
}
