//! End-to-end: `funes doctor --yes` drops the extra copies of a duplicated chunk, prunes
//! bookkeeping whose transcript is gone, removes a lock file without a receipt and the pre-rename
//! memory location, and reclaims the versions its own repairs left behind — with the memory still
//! recallable afterwards.
//!
//! Duplicates are planted the way a raced publish lands them — the memory's own rows appended a
//! second time, so every column matches and only the row address differs.

use std::collections::HashMap;
use std::io::Write;

use arrow_array::{Array, RecordBatch, RecordBatchIterator, StringArray};
use funes::memory::dataset;
use lance::dataset::WriteParams;

/// Index one session into a fresh `FUNES_HOME` and return the memory's table URI.
async fn indexed_memory(source: &std::path::Path) -> String {
    let dir = source.join("projects").join("-home-u-dev-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("doctor-session-0001.jsonl")).unwrap();
    for (i, text) in ["the first turn", "the second turn"].iter().enumerate() {
        let line = serde_json::json!({
            "type": "user",
            "uuid": format!("t{i}"),
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"role": "user", "content": text},
        });
        writeln!(f, "{line}").unwrap();
    }
    funes::commands::index::run_index(source, false, None).await.unwrap();
    dataset::table_uri(&dataset::local_memory_dir())
}

#[tokio::test]
async fn doctor_repairs_duplicates_bookkeeping_locks_and_disk() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let uri = indexed_memory(source.path()).await;

    // Plant the duplicates: re-append every row exactly as stored.
    let ds = dataset::open(&uri, HashMap::new()).await.unwrap();
    let schema: arrow_schema::SchemaRef = std::sync::Arc::new(arrow_schema::Schema::from(ds.schema()));
    let rows = dataset::scan_rows(&ds, &[], None, None).await.unwrap();
    let batches: Vec<RecordBatch> = rows
        .into_iter()
        .map(|b| RecordBatch::try_new(schema.clone(), b.columns().to_vec()).unwrap())
        .collect();
    let original = ds.count_rows(None).await.unwrap();
    assert!(original > 0, "setup: the session should have been indexed");
    let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema);
    lance::dataset::Dataset::write(
        reader,
        &uri,
        Some(WriteParams {
            mode: lance::dataset::WriteMode::Append,
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    let ds = dataset::open(&uri, HashMap::new()).await.unwrap();
    assert_eq!(ds.count_rows(None).await.unwrap(), original * 2);

    // Plant a stale pending entry: a transcript path that was never on this disk.
    let coverage = home.path().join("index-coverage.json");
    let live = std::fs::read_to_string(&coverage).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&live).unwrap();
    let gone = source.path().join("projects/-gone/deleted-session.jsonl");
    doc["pending"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(gone.to_string_lossy()));
    std::fs::write(&coverage, serde_json::to_string(&doc).unwrap()).unwrap();

    // Plant the pre-rename memory location, and a push-receipt lock whose receipt is gone.
    let legacy = home.path().join("store").join("chunks.lance");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("stale"), "x").unwrap();
    let locks = home.path().join("pushed").join(".locks");
    std::fs::create_dir_all(&locks).unwrap();
    std::fs::write(locks.join("hf___datasets_o_gone"), "").unwrap();

    let versions_before = dataset::open(&uri, HashMap::new())
        .await
        .unwrap()
        .versions()
        .await
        .unwrap()
        .len();

    funes::commands::doctor::run(funes::memory::Memory::local(), true)
        .await
        .unwrap();

    // Every chunk is back to one row, and the surviving ids are the ones the index wrote.
    let ds = dataset::open(&uri, HashMap::new()).await.unwrap();
    assert_eq!(
        ds.count_rows(None).await.unwrap(),
        original,
        "doctor should leave one row per chunk"
    );
    let ids = ids_in(&ds).await;
    assert_eq!(ids.len(), original, "a surviving row per chunk id");

    // The stale pending entry is gone and nothing else was invented in its place.
    let pruned: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&coverage).unwrap()).unwrap();
    let pending = pruned["pending"].as_array().unwrap();
    assert!(
        !pending.iter().any(|v| v.as_str() == Some(&gone.to_string_lossy())),
        "the gone transcript should have been dropped: {pending:?}"
    );

    // The home is tidied: the table nothing reads is gone, so is the lock without a receipt, and
    // the versions the repairs left behind were dropped.
    assert!(
        !home.path().join("store").exists(),
        "the pre-rename memory location should have been deleted"
    );
    assert!(
        !locks.join("hf___datasets_o_gone").exists(),
        "the lock without a receipt should have been removed"
    );
    let versions_after = ds.versions().await.unwrap().len();
    assert!(
        versions_after < versions_before,
        "expected old versions to be dropped: {versions_before} → {versions_after}"
    );

    // A recall still answers: the repair kept the memory readable.
    let hit = funes::commands::recall::recall(
        funes::memory::Memory::local(),
        "the second turn".to_string(),
        funes::commands::recall::DEFAULT_K,
        funes::commands::recall::DEFAULT_CANDIDATES,
        funes::commands::recall::DEFAULT_HALF_LIFE,
        funes::commands::recall::DEFAULT_NEIGHBORS,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(hit.contains("the second turn"), "recall after doctor: {hit}");
}

/// Every distinct chunk id in the memory.
async fn ids_in(ds: &lance::dataset::Dataset) -> std::collections::HashSet<String> {
    let batches = dataset::scan_rows(ds, &["id"], None, None).await.unwrap();
    let mut out = std::collections::HashSet::new();
    for b in &batches {
        let col = b
            .column_by_name("id")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for i in 0..col.len() {
            out.insert(col.value(i).to_string());
        }
    }
    out
}
