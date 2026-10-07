//! Gated end-to-end: `funes scrub` over a memory embedded with multilingual-e5-small. Own test
//! binary: it sets `$FUNES_HOME`.

mod support;

use std::process::Command;

use arrow_array::{Array, FixedSizeListArray, Float32Array, StringArray};
use funes::inference::{self, EmbeddingModel};
use funes::memory::dataset;

#[tokio::test]
async fn scrub_rewrites_with_the_memorys_model() {
    if funes::scan::Trufflehog::find().is_err() || !std::path::Path::new("/usr/bin/true").exists() {
        eprintln!("skip: trufflehog or /usr/bin/true unavailable");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let keyfile = home.path().join("throwaway_ed25519");
    let made = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f"])
        .arg(&keyfile)
        .status()
        .is_ok_and(|s| s.success());
    if !made {
        eprintln!("skip: ssh-keygen unavailable");
        return;
    }
    let key = std::fs::read_to_string(&keyfile).unwrap();
    std::fs::remove_file(&keyfile).unwrap();

    let e5 = EmbeddingModel::MultilingualE5Small;
    support::memory_of(e5, &["给表加上索引"], dataset::local_memory_dir().as_ref()).await;
    let turns = home.path().join("zh.funes.jsonl");
    let line = serde_json::json!({
        "session_id": "zh", "turn_uuid": "t0", "seq": 0, "ts": "2026-01-02T00:00:00Z",
        "role": "user", "harness": "test",
        "blocks": [{"block_type": "text", "text": format!("部署密钥：\n{key}")}],
    });
    std::fs::write(&turns, line.to_string()).unwrap();
    std::env::set_var("FUNES_TRUFFLEHOG", "/usr/bin/true");
    funes::commands::index::run_index(&turns, false, None).await.unwrap();
    std::env::remove_var("FUNES_TRUFFLEHOG");
    funes::commands::scrub::run().await.unwrap();

    let ds = dataset::open(&dataset::table_uri(&dataset::local_memory_dir()), Default::default())
        .await
        .unwrap();
    assert_eq!(dataset::embedding_model(&ds).unwrap(), e5);
    let mut embedder = inference::embedder(e5).unwrap();
    let mut redacted = false;
    for batch in dataset::scan_rows(&ds, &["text", "vector"], Some("session_id = 'zh'"), None)
        .await
        .unwrap()
    {
        let texts = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
        let vectors = batch.column(1).as_any().downcast_ref::<FixedSizeListArray>().unwrap();
        for i in 0..batch.num_rows() {
            redacted |= texts.value(i).contains("[REDACTED:PrivateKey]");
            let stored = vectors.value(i);
            let stored = stored.as_any().downcast_ref::<Float32Array>().unwrap();
            let expected = embedder.embed(&[texts.value(i)]).unwrap().remove(0);
            let cosine: f32 = stored.values().iter().zip(&expected).map(|(a, b)| a * b).sum();
            assert!(cosine > 0.9999, "{cosine}: {}", texts.value(i));
        }
    }
    assert!(redacted, "scrub should have redacted the key");
}
