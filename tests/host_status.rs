//! `funes status` with no memory named: the local memory, then the memory this host is bound to,
//! or what an older funes bound its agents to.

use funes::commands::recall::host_status;
use funes::memory::Memory;
use std::io::Write;

#[tokio::test]
async fn host_status_shows_the_bound_memory_or_the_older_bindings() {
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

    let out = host_status(Some(&at_path), None).await.unwrap();
    assert!(out.starts_with("memory: "), "the local memory comes first: {out}");
    assert!(out.contains("no index yet"), "{out}");
    let block = out
        .split(&format!("memory: {at_path} (bound)"))
        .nth(1)
        .unwrap_or_else(|| panic!("the bound memory is listed: {out}"));
    assert!(block.starts_with("\nchunks: "), "{out}");
    assert!(
        block.contains("\nlast write: "),
        "a bound memory is read as it stands: {out}"
    );
    assert!(
        !block.contains("push coverage"),
        "a memory at a local path is not a remote anything pushes to: {out}"
    );

    let out = host_status(Some(&missing), None).await.unwrap();
    assert!(out.contains(&format!("memory: {missing} (bound)")), "{out}");

    let note = "claude is bound to acme/kb, codex to acme/other; this host reads local until `funes bind <memory>` binds it to one";
    let out = host_status(None, Some(note)).await.unwrap();
    assert!(
        out.ends_with(&format!("\n{note}\n")),
        "a host without the binding is told: {out}"
    );
}
