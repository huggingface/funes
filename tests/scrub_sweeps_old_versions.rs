//! Gated end-to-end: a scrub with nothing to redact still deletes the older versions, where rows an
//! earlier scrub removed stay readable on disk. Skipped unless trufflehog is available.

mod support;

use std::io::Write;
use std::sync::Arc;

use arrow_array::RecordBatchIterator;
use funes::memory::{dataset, Memory};
use lance::dataset::{Dataset, WriteMode, WriteParams};

#[tokio::test]
async fn scrub_with_nothing_to_redact_deletes_old_versions() {
    if funes::scan::Trufflehog::find().is_err() {
        eprintln!("skip: trufflehog not found");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());

    let residue = "residue-canary-4f1c9e";
    let mut f = std::fs::File::create(source.path().join("sess.funes.jsonl")).unwrap();
    for (i, text) in [residue, "a benign turn"].iter().enumerate() {
        writeln!(
            f,
            r#"{{"format":1,"session_id":"sess","cwd":"/scrubtest/proj","turn_uuid":"s{i}","seq":{i},"ts":"2026-02-01T00:00:{i:02}Z","role":"user","blocks":[{{"block_type":"text","text":"{text}"}}],"harness":"claude"}}"#
        )
        .unwrap();
    }
    drop(f);
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();

    // What a scrub without the sweep leaves: the residue turn only in old versions.
    let ds = Memory::local().open().await.unwrap();
    let kept = dataset::scan_rows(&ds, &[], Some("turn_uuid != 's0'"), None)
        .await
        .unwrap();
    let schema = Arc::new(arrow_schema::Schema::from(ds.schema()));
    Dataset::write(
        RecordBatchIterator::new(kept.into_iter().map(Ok), schema),
        &dataset::table_uri(&dataset::local_memory_dir()),
        Some(WriteParams {
            mode: WriteMode::Overwrite,
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    let memdir = home.path().join("memory");
    assert!(
        !support::files_containing(&memdir, residue).is_empty(),
        "setup: the residue should be on disk before scrub"
    );

    funes::commands::scrub::run().await.unwrap();
    let left = support::files_containing(&memdir, residue);
    assert!(left.is_empty(), "residue still on disk after scrub: {left:?}");
}
