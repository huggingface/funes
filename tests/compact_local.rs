//! `funes compact` on a local memory merges its small fragments, and says there is nothing to
//! compact before the memory exists. Own test binary so its `$FUNES_HOME` can't race another
//! integration test's.

mod support;

use funes::commands::compact;
use funes::memory::Memory;

#[tokio::test]
async fn compact_merges_a_fragmented_local_memory() {
    let src = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db.path());
    let report = compact::run(Memory::resolve(None)).await.unwrap();
    assert!(report.ends_with(": nothing to compact\n"), "{report}");

    support::index_fragmented(src.path()).await;

    let report = compact::run(Memory::resolve(None)).await.unwrap();
    assert!(report.ends_with(": compacted\n"), "{report}");
    assert_eq!(
        support::fragments().await,
        2,
        "the 257 fragments indexed together merge"
    );
}
