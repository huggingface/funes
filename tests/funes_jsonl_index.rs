//! Indexing `.funes.jsonl` turns files end to end: a valid file is written, an invalid one writes
//! nothing and fails the run, and a directory mixing them writes the valid files, reports the rest,
//! and exits non-zero. Own test binary: it sets `$FUNES_HOME`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use arrow_array::{Array, StringArray};
use funes::memory::dataset;
use funes::traces::harness::Harness;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/funes_jsonl")
        .join(name)
}

async fn index(path: &Path) -> anyhow::Result<()> {
    funes::commands::index::run_index(path, false, None).await
}

/// The `session_id` of every row the local memory holds.
async fn stored_rows() -> Vec<String> {
    let ds = dataset::open(&dataset::table_uri(&dataset::local_memory_dir()), Default::default())
        .await
        .expect("the memory exists");
    let mut out = Vec::new();
    for batch in dataset::scan_rows(&ds, &["session_id"], None, None).await.unwrap() {
        let col = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
        out.extend((0..col.len()).map(|i| col.value(i).to_string()));
    }
    out
}

async fn stored_sessions() -> BTreeSet<String> {
    stored_rows().await.into_iter().collect()
}

#[tokio::test]
async fn turns_files_are_indexed_and_invalid_ones_rejected() {
    // One file at a time: a rejected file fails the run and writes nothing.
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let err = index(&fixture("bad_line.funes.jsonl")).await.unwrap_err().to_string();
    assert!(err.contains("bad_line.funes.jsonl:2:"), "{err}");
    let err = index(&fixture("format2.funes.jsonl")).await.unwrap_err().to_string();
    assert!(err.contains("unknown format 2"), "{err}");
    index(&fixture("valid.funes.jsonl")).await.unwrap();
    assert_eq!(stored_sessions().await, BTreeSet::from(["b3f2e0c4".to_string()]));
    // `--harness` is refused: the facet is in the data.
    let err = funes::commands::index::run_index_roots(
        &[(fixture("valid.funes.jsonl"), Some(Harness::Claude))],
        false,
        None,
        true,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("--harness"), "{err}");

    // The directory: the valid files land, the two rejected ones are counted and fail the exit
    // status.
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let err = index(&fixture("")).await.unwrap_err().to_string();
    assert!(err.contains("2 unit(s) rejected"), "{err}");
    assert_eq!(
        stored_sessions().await,
        BTreeSet::from([
            "b3f2e0c4".to_string(),
            "dup-turn".to_string(),
            "elided".to_string(),
            "gh/huggingface/transformers#31234".to_string(),
        ])
    );
    // A turn re-emitted under its `turn_uuid` in the same file is one row, not two.
    let rows = stored_rows().await;
    assert_eq!(rows.iter().filter(|s| *s == "dup-turn").count(), 1, "{rows:?}");
}
