//! Indexing into a memory embedded with multilingual-e5-small. Own test binary: it sets
//! `$FUNES_HOME`.

use arrow_array::{Array, FixedSizeListArray, Float32Array, StringArray};
use funes::commands::index;
use funes::inference::{self, EmbeddingModel};
use funes::memory::dataset;

async fn memory() -> lance::Dataset {
    dataset::open(&dataset::table_uri(&dataset::local_memory_dir()), Default::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_multilingual_memory_is_created_then_kept() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let e5 = EmbeddingModel::MultilingualE5Small;
    let turns = home.path().join("turns");
    std::fs::create_dir(&turns).unwrap();
    std::fs::write(
        turns.join("zh.funes.jsonl"),
        r#"{"session_id":"zh","turn_uuid":"t0","seq":0,"ts":"2026-01-02T00:00:00Z","role":"user","harness":"test","blocks":[{"block_type":"text","text":"部署失败后怎么回滚"}]}"#,
    )
    .unwrap();

    index::ensure_local_memory(e5).await.unwrap();
    index::run_index(&turns, false, None).await.unwrap();
    let ds = memory().await;
    assert_eq!(dataset::embedding_model(&ds).unwrap(), e5);
    let rows = dataset::scan_rows(&ds, &["text", "vector"], None, None).await.unwrap();
    let text = rows[0]
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .value(0);
    let stored = rows[0]
        .column(1)
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap()
        .value(0);
    let stored = stored.as_any().downcast_ref::<Float32Array>().unwrap();
    let expected = inference::embedder(e5).unwrap().embed(&[text]).unwrap().remove(0);
    let cosine: f32 = stored.values().iter().zip(&expected).map(|(a, b)| a * b).sum();
    assert!(cosine > 0.9999, "{cosine}");

    let err = index::ensure_local_memory(EmbeddingModel::BgeSmallEn)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(e5.id()), "{err}");

    // The incremental state still records the directory as indexed.
    std::fs::remove_dir_all(dataset::local_memory_dir()).unwrap();
    index::ensure_local_memory(e5).await.unwrap();
    index::run_index(&turns, false, None).await.unwrap();
    assert_eq!(memory().await.count_rows(None).await.unwrap(), 1);
}
