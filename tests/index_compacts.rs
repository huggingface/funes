//! An index run merges the fragments it wrote (one per session) and leaves the indexed ones alone.
//! Own test binary so its `$FUNES_HOME` can't race another integration test's.

use std::io::Write;

fn write_session(source: &std::path::Path, id: &str) {
    let mut f = std::fs::File::create(source.join(format!("{id}.funes.jsonl"))).unwrap();
    writeln!(
        f,
        r#"{{"format":1,"session_id":"{id}","cwd":"/home/u/dev/demo","turn_uuid":"{id}-t0","seq":0,"ts":"2026-01-01T00:00:00Z","role":"user","blocks":[{{"block_type":"text","text":"session {id} about compacting lance fragments"}}],"harness":"claude"}}"#
    )
    .unwrap();
}

async fn fragments() -> usize {
    funes::memory::Memory::local()
        .open()
        .await
        .unwrap()
        .get_fragments()
        .len()
}

#[tokio::test]
async fn index_merges_the_fragments_it_wrote() {
    let src = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db.path());

    for id in ["s1", "s2", "s3"] {
        write_session(src.path(), id);
    }
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    assert_eq!(fragments().await, 1, "three sessions, one fragment");

    for id in ["s4", "s5"] {
        write_session(src.path(), id);
    }
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    assert_eq!(fragments().await, 2, "the indexed fragment stays, the new two merge");
}
