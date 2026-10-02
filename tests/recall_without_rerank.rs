//! A search told not to rerank returns the fused order, scored by the fused score, over a real
//! index under a temp `$FUNES_HOME`. The same search reranked scores with the cross-encoder.

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
async fn a_search_without_rerank_keeps_the_fused_order() {
    let db_dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    write_transcript(source.path());
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();
    let quiet = |_: &str| ();
    let query = "parse transcripts into turns";

    let search = recall::Search::new(query.into(), 30, Default::default(), &quiet)
        .await
        .unwrap()
        .with_rerank(false);
    let pool = search.candidates(&Memory::local(), &quiet).await.unwrap();
    let (_, fused) = search.rank(vec![pool], 5, 0, &quiet).await.unwrap();
    assert!(!fused.is_empty(), "the indexed turns should be candidates");
    for (hit, score) in &fused {
        assert_eq!(
            *score, hit.fused as f64,
            "without a rerank the fused score is the score"
        );
    }
    assert!(
        fused.windows(2).all(|pair| pair[0].1 >= pair[1].1),
        "hits should come in fused order"
    );

    let search = recall::Search::new(query.into(), 30, Default::default(), &quiet)
        .await
        .unwrap();
    let pool = search.candidates(&Memory::local(), &quiet).await.unwrap();
    let (_, reranked) = search.rank(vec![pool], 5, 0, &quiet).await.unwrap();
    assert_eq!(reranked.len(), fused.len(), "a rerank keeps the same hits");
    assert!(
        reranked.iter().all(|(_, score)| *score > 0.0 && *score <= 1.0),
        "a rerank scores with the cross-encoder's probability"
    );
    assert!(
        reranked.iter().any(|(hit, score)| *score != hit.fused as f64),
        "a rerank should replace the fused scores"
    );
}
