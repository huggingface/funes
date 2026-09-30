//! `funes status` with no memory named: the local memory, then each bound memory with the agents
//! bound to it, then the agents installed with no memory recorded.

use funes::commands::recall::host_status;
use funes::memory::Memory;
use std::io::Write;

#[tokio::test]
async fn host_status_lists_each_bound_memory_and_the_unrecorded() {
    // A memory at a local path, indexed while it was the local memory.
    let other = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", other.path());
    let source = tempfile::tempdir().unwrap();
    let mut f = std::fs::File::create(source.path().join("sess.funes.jsonl")).unwrap();
    writeln!(
        f,
        r#"{{"format":1,"session_id":"sess","turn_uuid":"s0","seq":0,"ts":"2026-02-01T00:00:00Z","role":"user","blocks":[{{"block_type":"text","text":"a turn"}}],"harness":"claude"}}"#
    )
    .unwrap();
    drop(f);
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();
    let at_path = Memory::local().label();

    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let missing = home.path().join("no-such-memory").to_string_lossy().into_owned();

    let out = host_status(
        &[
            (missing, vec!["claude".to_string(), "codex".to_string()]),
            (at_path, vec!["hermes".to_string()]),
        ],
        &["pi".to_string()],
    )
    .await
    .unwrap();

    assert!(out.starts_with("memory: "), "the local memory comes first: {out}");
    assert!(out.contains("no index yet"), "{out}");
    assert!(
        out.contains("(bound: claude, codex)"),
        "a bound memory names the agents bound to it: {out}"
    );
    let at_path = out
        .split("(bound: hermes)")
        .nth(1)
        .unwrap_or_else(|| panic!("a memory at a local path is listed: {out}"));
    assert!(at_path.starts_with("\nchunks: "), "{out}");
    assert!(
        !at_path.contains("last push"),
        "a memory at a local path is not a remote anything pushes to: {out}"
    );
    assert!(
        out.contains("installed with no memory recorded: pi"),
        "an install funes has no record of is named: {out}"
    );
}
