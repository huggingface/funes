//! An index run with nothing new still compacts a memory of many small indexed fragments.
//! Own test binary so its `$FUNES_HOME` can't race another integration test's.

use std::io::Write;
use std::sync::Arc;

use arrow_array::RecordBatchIterator;
use funes::memory::{dataset, Memory};

async fn fragments() -> usize {
    Memory::local().open().await.unwrap().get_fragments().len()
}

#[tokio::test]
async fn index_compacts_a_fragmented_memory_with_nothing_new() {
    let src = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db.path());
    let mut f = std::fs::File::create(src.path().join("s1.funes.jsonl")).unwrap();
    writeln!(
        f,
        r#"{{"format":1,"session_id":"s1","cwd":"/home/u/dev/demo","turn_uuid":"t0","seq":0,"ts":"2026-01-01T00:00:00Z","role":"user","blocks":[{{"block_type":"text","text":"a memory split into many small fragments"}}],"harness":"claude"}}"#
    )
    .unwrap();
    drop(f);
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();

    let mut ds = Memory::local().open().await.unwrap();
    let rows = dataset::scan_rows(&ds, &[], None, None).await.unwrap();
    let schema = Arc::new(arrow_schema::Schema::from(ds.schema()));
    for _ in 0..257 {
        let reader = RecordBatchIterator::new(rows.clone().into_iter().map(Ok), schema.clone());
        ds.append(reader, None).await.unwrap();
    }
    dataset::build_indexes(&mut ds, |_| {}).await.unwrap();
    assert_eq!(fragments().await, 258);

    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    assert_eq!(fragments().await, 2, "the 257 fragments indexed together merge");
}
