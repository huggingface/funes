//! The line between a pool reranked and a pool cut as fused, over a real index under a temp
//! `$FUNES_HOME`: the defaults sit under it, fewer hits from the same pool sit over it.

mod support;

use funes::commands::recall;
use funes::memory::Memory;

#[tokio::test]
async fn a_pool_near_the_hits_is_not_reranked() {
    let db_dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    support::write_transcript(source.path());
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();
    let quiet = |_: &str| ();
    let query = "parse transcripts into turns";

    // The defaults: 8 hits from a pool of 30, under the line.
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
    assert!(!hits.is_empty());
    for (hit, score) in &hits {
        assert_eq!(*score, hit.fused as f64, "the defaults keep the fused order");
    }

    // Fewer hits from the same pool: four times the hits, reranked.
    let (_, hits) = recall::recall_hits(Memory::local(), query.into(), 5, 30, 0, Default::default(), &quiet)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|(hit, score)| *score != hit.fused as f64),
        "a pool several times the hits is reranked"
    );
}
