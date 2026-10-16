//! Gated end-to-end: `funes scrub` redacts a secret already sitting in the memory, in place, without
//! its source, and re-embeds what it rewrote with the memory's own model (multilingual-e5-small
//! here). Skipped unless trufflehog and ssh-keygen are available.
//!
//! A dirty memory is created by indexing with `FUNES_TRUFFLEHOG=/usr/bin/true` — `true` ignores its
//! args and exits 0, so the scan finds nothing and the secret is stored unredacted (simulating a
//! memory built before redaction existed). Then scrub runs with the real scanner and cleans it.

mod support;

use std::process::Command;

use arrow_array::{Array, FixedSizeListArray, Float32Array, StringArray};
use funes::commands::recall::{self, TurnRange};
use funes::home;
use funes::inference::{self, EmbeddingModel};
use funes::memory::{dataset, Memory};

#[tokio::test]
async fn scrub_redacts_an_existing_secret_in_place_with_the_memorys_model() {
    if funes::scan::Trufflehog::find().is_err() {
        eprintln!("skip: trufflehog not found");
        return;
    }
    if !std::path::Path::new("/usr/bin/true").exists() {
        eprintln!("skip: /usr/bin/true unavailable");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());

    // Mint a throwaway key and plant it in a transcript.
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
    let key_body = key.lines().nth(1).unwrap().to_string();

    let e5 = EmbeddingModel::MultilingualE5Small;
    support::memory_of(e5, &["给表加上索引"], &home::memory_dir()).await;
    let session = "scrub-session-0001";
    support::write_session(source.path(), session, &[format!("部署密钥：\n{key}")]);

    // Index with a no-op scanner so the secret lands in the memory unredacted.
    std::env::set_var("FUNES_TRUFFLEHOG", "/usr/bin/true");
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();
    let dirty = recall::get(Memory::local(), session.into(), TurnRange::default())
        .await
        .unwrap();
    assert!(
        dirty.contains(&key_body),
        "setup: the key should be in the memory before scrub"
    );
    let memdir = home.path().join("memory");
    assert!(
        !support::files_containing(&memdir, &key_body).is_empty(),
        "setup: the key should be on disk before scrub"
    );

    // Scrub with the real scanner: it must redact the key in place.
    std::env::remove_var("FUNES_TRUFFLEHOG");
    funes::commands::scrub::run().await.unwrap();
    let clean = recall::get(Memory::local(), session.into(), TurnRange::default())
        .await
        .unwrap();
    assert!(
        clean.contains("[REDACTED:PrivateKey]"),
        "expected a redaction marker after scrub: {clean}"
    );
    assert!(!clean.contains(&key_body), "key body survived scrub: {clean}");
    let residue = support::files_containing(&memdir, &key_body);
    assert!(residue.is_empty(), "key body still on disk after scrub: {residue:?}");

    // What it rewrote is embedded by the memory's model, which it keeps.
    let ds = dataset::open(&dataset::table_uri(home::memory_dir()), Default::default())
        .await
        .unwrap();
    assert_eq!(dataset::embedding_model(&ds).unwrap(), e5);
    let mut embedder = inference::embedder(e5).unwrap();
    let filter = format!("session_id = '{session}'");
    let mut redacted = false;
    for batch in dataset::scan_rows(&ds, &["text", "vector"], Some(filter.as_str()), None)
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
    assert!(redacted, "the rewritten row is among those checked");
}
