//! `funes compact` takes the lock inside the memory it compacts, the one every writer of that memory
//! takes, so it waits for writers from any funes home and creates nothing outside the memory. Own test
//! binary so its `$FUNES_HOME` can't race another integration test's.

use std::io::Write;
use std::path::Path;

use funes::commands::compact;
use funes::memory::lock::MemoryLock;
use funes::memory::Memory;

async fn index_into_home(src: &Path) {
    let mut f = std::fs::File::create(src.join("s1.funes.jsonl")).unwrap();
    writeln!(
        f,
        r#"{{"format":1,"session_id":"s1","cwd":"/home/u/dev/demo","turn_uuid":"t0","seq":0,"ts":"2026-01-01T00:00:00Z","role":"user","blocks":[{{"block_type":"text","text":"a memory to compact"}}],"harness":"claude"}}"#
    )
    .unwrap();
    drop(f);
    funes::commands::index::run_index(src, false, None).await.unwrap();
}

async fn refused(target: &Memory) -> bool {
    match compact::run(target.clone()).await {
        Err(e) => e.to_string().contains("in progress"),
        Ok(report) => panic!("compacted a memory another writer holds: {report}"),
    }
}

#[tokio::test]
async fn compact_waits_for_any_writer_of_its_memory() {
    let src = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();

    // Home B's own memory, held as an index run there holds it.
    std::env::set_var("FUNES_HOME", b.path());
    index_into_home(src.path()).await;
    let held = MemoryLock::acquire().unwrap();

    // A compaction run from home A refuses it, then compacts it once B is done.
    std::env::set_var("FUNES_HOME", a.path());
    let b_memory = Memory::parse(&b.path().join("memory").to_string_lossy());
    assert!(refused(&b_memory).await);
    drop(held);
    let report = compact::run(b_memory).await.unwrap();
    assert!(report.ends_with(": compacted\n"), "{report}");

    // A memory outside any home: its lock is inside it, and nothing appears beside it.
    let moved = a.path().join("elsewhere").join("memory");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(b.path().join("memory"), &moved).unwrap();
    let elsewhere = Memory::parse(&moved.to_string_lossy());
    let held = MemoryLock::acquire_in(&moved).unwrap();
    assert!(refused(&elsewhere).await);
    drop(held);
    let report = compact::run(elsewhere).await.unwrap();
    assert!(report.ends_with(": compacted\n"), "{report}");
    let beside: Vec<_> = std::fs::read_dir(moved.parent().unwrap()).unwrap().collect();
    assert_eq!(beside.len(), 1, "only the memory itself: {beside:?}");

    // Home A's own memory: the lock its index runs take.
    index_into_home(src.path()).await;
    let held = MemoryLock::acquire().unwrap();
    assert!(refused(&Memory::resolve(None)).await);
    drop(held);
}
