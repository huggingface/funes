//! Indexing into a memory embedded with multilingual-e5-small. Own test binary: it sets
//! `$FUNES_HOME`.

mod support;

use arrow_array::{Array, FixedSizeListArray, Float32Array, StringArray};
use funes::inference::{self, EmbeddingModel};
use funes::memory::dataset;

#[tokio::test]
async fn index_writes_with_the_memorys_model() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let e5 = EmbeddingModel::MultilingualE5Small;
    support::memory_of(e5, &["给表加上索引"], dataset::local_memory_dir().as_ref()).await;

    let turns = home.path().join("zh.funes.jsonl");
    std::fs::write(
        &turns,
        r#"{"session_id":"zh","turn_uuid":"t0","seq":0,"ts":"2026-01-02T00:00:00Z","role":"user","harness":"test","blocks":[{"block_type":"text","text":"部署失败后怎么回滚"}]}"#,
    )
    .unwrap();
    funes::commands::index::run_index(&turns, false, None).await.unwrap();

    let ds = dataset::open(&dataset::table_uri(&dataset::local_memory_dir()), Default::default())
        .await
        .unwrap();
    assert_eq!(dataset::embedding_model(&ds).unwrap(), e5);
    let rows = dataset::scan_rows(&ds, &["text", "vector"], Some("session_id = 'zh'"), None)
        .await
        .unwrap();
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
}
