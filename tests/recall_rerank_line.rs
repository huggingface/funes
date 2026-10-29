//! The line between a pool reranked and a pool cut as fused, over a real index under a temp
//! `$FUNES_HOME`: a memory with no more rows than the hits is read whole, fewer hits than rows are
//! reranked.

mod support;

use funes::commands::recall;
use funes::memory::Memory;

#[tokio::test]
async fn a_pool_within_the_hits_is_not_reranked() {
    let db_dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    support::write_transcript(source.path());
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();
    let quiet = |_: &str| ();
    let query = "parse transcripts into turns";

    // The defaults, 8 hits from a pool of 30, on a memory of a few rows: nothing to choose.
    let rows = support::stored_rows().await;
    assert!(rows > 1 && rows <= recall::DEFAULT_K, "the fixture holds {rows} rows");
    let (_, hits) = recall::recall_hits(
        Memory::local(),
        query.into(),
        recall::DEFAULT_K,
        recall::DEFAULT_CANDIDATES,
        0,
        Default::default(),
        &quiet,
    )
    .await
    .unwrap();
    assert_eq!(hits.len(), rows, "the memory is read whole");
    for (hit, score) in &hits {
        assert_eq!(*score, hit.fused as f64, "a pool within the hits keeps the fused order");
    }

    // Fewer hits than the memory holds: the pool is reranked.
    let (_, hits) = recall::recall_hits(Memory::local(), query.into(), 1, 30, 0, Default::default(), &quiet)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|(hit, score)| *score != hit.fused as f64),
        "a pool larger than the hits is reranked"
    );
}
