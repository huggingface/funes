//! `status` of a memory named by its local path reads it as it stands, like a remote: its last
//! write and unindexed backlog. The local memory's progress (its sessions, embedding backlog and
//! the source sessions still owed rows) is its own.

use funes::commands::recall::status;
use funes::home;
use funes::memory::Memory;
use std::path::Path;

const TURN: &str = r#"{"format":1,"session_id":"sess","turn_uuid":"s0","seq":0,"ts":"2026-02-01T00:00:00Z","role":"user","blocks":[{"block_type":"text","text":"a turn"}],"harness":"claude"}"#;

/// Index one session from `source` into the memory of the funes home at `home`, and name it by its path.
async fn index_into(home: &Path, source: &Path) -> String {
    std::env::set_var("FUNES_HOME", home);
    std::fs::write(source.join("sess.funes.jsonl"), format!("{TURN}\n")).unwrap();
    funes::commands::index::run_index(source, false, None).await.unwrap();
    Memory::local().label()
}

#[tokio::test]
async fn a_memory_at_a_local_path_is_read_as_it_stands_not_as_the_hosts_index() {
    // A memory indexed under one home, then named by its path from another.
    let other_home = tempfile::tempdir().unwrap();
    let other_source = tempfile::tempdir().unwrap();
    let at_path = index_into(other_home.path(), other_source.path()).await;

    // This host: an index of its own, and a coverage snapshot owing rows to one source session.
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    index_into(home.path(), source.path()).await;
    std::fs::write(
        home::index_coverage_file(),
        r#"{"pending":["/sessions/owed.funes.jsonl"]}"#,
    )
    .unwrap();

    let own = status(Memory::local()).await.unwrap();
    assert!(own.contains("\nsessions: 1\n"), "{own}");
    assert!(own.contains("pending indexing: 1 source session"), "{own}");
    assert!(own.contains("\nlast indexed: "), "{own}");

    let named = status(Memory::parse(&at_path)).await.unwrap();
    assert!(named.starts_with(&format!("memory: {at_path}\nchunks: ")), "{named}");
    assert!(named.contains("\nlast write: "), "{named}");
    for host_line in ["sessions:", "pending indexing", "last indexed"] {
        assert!(
            !named.contains(host_line),
            "{host_line} describes the local memory, not this one: {named}"
        );
    }
}
