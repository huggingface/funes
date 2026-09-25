//! lance leaks the scratch directory of every IVF index build (fixed upstream in 13.0.0; funes pins
//! 11), so `build_indexes` reclaims the settled ones on its way in. This exercises that against a
//! real build rather than a hand-made directory: it is the tripwire for the file names the sweep
//! matches on, which are lance's to change.
//!
//! Its own test binary so its `$TMPDIR` can't race another integration test's — the sweep reads the
//! process-wide temp directory, and a leftover there can't be attributed to a particular build.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema};
use lance::Dataset;

/// Wide enough to train IVF: lance needs a few hundred rows, below which it declines to build the
/// vector index at all and nothing is shuffled.
const ROWS: usize = 512;

/// What a lance shuffle directory holds. Spelled out again here rather than shared with the sweep:
/// the two lists agreeing is the property under test.
const SHUFFLE_FILES: [&str; 4] = [
    "shuffle_data.lance",
    "shuffle_data.spill",
    "shuffle_offsets.lance",
    "shuffle_offsets.spill",
];

#[test]
fn a_settled_shuffle_dir_is_reclaimed_by_the_next_build() {
    let tmp = tempfile::tempdir().unwrap();
    // Before the runtime exists, so no other thread can be reading the environment, and before
    // anything asks for a temp directory: both lance and the sweep resolve `$TMPDIR` per call.
    std::env::set_var("TMPDIR", tmp.path());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        // Outside the `.tmp*` namespace, so the dataset itself never reads as a leftover.
        let mut ds = write_dataset(&tmp.path().join("memory.lance")).await;

        let before = tmp_dirs(tmp.path());
        funes::memory::dataset::build_indexes(&mut ds, |_| {}).await;
        let leaked: Vec<PathBuf> = tmp_dirs(tmp.path())
            .difference(&before)
            .filter(|d| holds_only_shuffle_files(d))
            .cloned()
            .collect();
        assert!(
            !leaked.is_empty(),
            "a build left no directory holding {SHUFFLE_FILES:?} — if lance stopped leaking, or \
             renamed these files, the sweep is matching on nothing"
        );

        // Age them past the sweep's threshold, as a leftover from an earlier run would be.
        for dir in &leaked {
            let settled = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
            std::fs::File::open(dir).unwrap().set_modified(settled).unwrap();
        }

        funes::memory::dataset::build_indexes(&mut ds, |_| {}).await;
        let kept: Vec<&PathBuf> = leaked.iter().filter(|d| d.exists()).collect();
        assert!(kept.is_empty(), "the next build left {kept:?} behind");
    });
}

/// The `.tmp*` directories directly under `root` — tempfile's prefix, and so lance's.
fn tmp_dirs(root: &Path) -> BTreeSet<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return BTreeSet::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp") && e.path().is_dir())
        .map(|e| e.path())
        .collect()
}

fn holds_only_shuffle_files(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let names: BTreeSet<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    !names.is_empty() && names.iter().all(|n| SHUFFLE_FILES.contains(&n.as_str()))
}

/// A `text` + `vector` dataset at `uri`, shaped like funes's `chunks` table as far as the index
/// build cares: enough distinct vectors for IVF to find centroids.
async fn write_dataset(uri: &Path) -> Dataset {
    let dim = funes::memory::dataset::DIM;
    let item = Arc::new(Field::new("item", DataType::Float32, true));
    let schema = Arc::new(Schema::new(vec![
        Field::new("text", DataType::Utf8, true),
        Field::new("vector", DataType::FixedSizeList(item.clone(), dim), true),
    ]));

    let texts: Vec<Option<String>> = (0..ROWS).map(|i| Some(format!("chunk {i}"))).collect();
    let values: Vec<f32> = (0..ROWS * dim as usize).map(|i| (i % 1000) as f32 / 1000.0).collect();
    let vectors = FixedSizeListArray::try_new(item, dim, Arc::new(Float32Array::from(values)), None).unwrap();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(texts)), Arc::new(vectors)],
    )
    .unwrap();

    let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
    Dataset::write(reader, uri.to_str().unwrap(), None).await.unwrap()
}
