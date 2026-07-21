//! `funes ask` grounding: a real temp memory's passages land in the prompt handed to the agent,
//! wrapped in the ask instruction. Its own binary because it sets the process-global
//! `FUNES_HOME`, and cargo runs a file's tests concurrently — two such tests would clobber each
//! other's memory.

use std::io::Write;

/// Write a `<source>/projects/<project>/<session>.jsonl` transcript so the indexer resolves
/// workdir/session the way it does for real Claude Code projects.
fn write_transcript(source: &std::path::Path) -> String {
    let dir = source.join("projects").join("-home-u-dev-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let session = "ask-session-0001";
    let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
    let lines = [
        r#"{"type":"user","uuid":"t1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"why did we settle on reciprocal rank fusion"}}"#,
        r#"{"type":"assistant","uuid":"t2","parentUuid":"t1","timestamp":"2026-01-01T00:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"We fuse vector and BM25 rankings with reciprocal rank fusion because it needs no score calibration."}]}}"#,
    ];
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
    session.to_string()
}

#[tokio::test]
async fn grounding_embeds_memory_passages_in_the_prompt() {
    let db_dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    let session = write_transcript(source.path());
    funes::index::run_index(source.path(), false, None).await.unwrap();

    let prompt = funes::ask::grounding(funes::hub::Memory::local(), "why reciprocal rank fusion", &|_| ())
        .await
        .unwrap();

    assert!(
        prompt.starts_with("Answer the question below"),
        "the instruction leads the prompt: {prompt}"
    );
    assert!(
        prompt.contains("why reciprocal rank fusion"),
        "the question is embedded: {prompt}"
    );
    assert!(
        prompt.contains("no score calibration"),
        "the memory's passage is embedded: {prompt}"
    );
    assert!(prompt.contains(session.as_str()), "provenance is kept: {prompt}");
}
