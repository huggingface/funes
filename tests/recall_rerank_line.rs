//! The line between a pool reranked and a pool cut as fused, over a real index under a temp
//! `$FUNES_HOME`: the defaults sit under it, fewer hits from the same pool sit over it.

use std::io::Write;

use funes::commands::recall;
use funes::memory::Memory;

fn write_transcript(source: &std::path::Path) {
    let mut f = std::fs::File::create(source.join("test-session-0001.funes.jsonl")).unwrap();
    let lines = [
        r#"{"format":1,"session_id":"test-session-0001","cwd":"/home/u/dev/demo","turn_uuid":"t1","seq":0,"ts":"2026-01-01T00:00:00Z","role":"user","blocks":[{"block_type":"text","text":"how do we parse transcripts into turns"}],"harness":"claude"}"#,
        r#"{"format":1,"session_id":"test-session-0001","cwd":"/home/u/dev/demo","turn_uuid":"t2","parent_uuid":"t1","seq":1,"ts":"2026-01-01T00:00:05Z","role":"assistant","blocks":[{"block_type":"text","text":"We parse each JSONL line into a turn with typed blocks."},{"block_type":"tool_use","text":"{\"command\":\"cargo test\"}","tool_name":"Bash","tool_use_id":"c1"}],"harness":"claude"}"#,
        r#"{"format":1,"session_id":"test-session-0001","cwd":"/home/u/dev/demo","turn_uuid":"t3","parent_uuid":"t2","seq":2,"ts":"2026-01-01T00:00:10Z","role":"user","blocks":[{"block_type":"tool_result","text":"22 passed","tool_name":"Bash","tool_use_id":"c1"}],"harness":"claude"}"#,
    ];
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}
#[tokio::test]
async fn a_pool_near_the_hits_is_not_reranked() {
    let db_dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    write_transcript(source.path());
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
