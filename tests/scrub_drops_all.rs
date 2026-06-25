//! Gated end-to-end: when *every* block scrub sees is an unredactable secret, the single Overwrite
//! commit must still produce a valid (empty) store, not error on a zero-row write. Exercises the
//! all-dropped path that the mixed-block test does not. Skipped unless trufflehog and ssh-keygen
//! are available.

use std::collections::HashMap;
use std::io::Write;
use std::process::Command;

#[tokio::test]
async fn scrub_dropping_every_block_leaves_a_valid_empty_store() {
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

    let keyfile = home.path().join("throwaway_ed25519");
    let made = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f"])
        .arg(&keyfile)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        eprintln!("skip: ssh-keygen unavailable");
        return;
    }
    let key = std::fs::read_to_string(&keyfile).unwrap();
    std::fs::remove_file(&keyfile).unwrap();
    let escaped = key.replace('\n', "\\n"); // unredactable: canonical value never byte-matches

    // A single session, a single block — the escaped key and nothing else. Scrub will drop it all.
    let project = "-home-u-dev-demo";
    let session = "scrub-all-0001";
    let dir = source.path().join("projects").join(project);
    std::fs::create_dir_all(&dir).unwrap();
    let line = serde_json::json!({
        "type": "user",
        "uuid": "t1",
        "timestamp": "2026-01-01T00:00:00Z",
        "message": {"role": "user", "content": format!("deploy key: {escaped}")},
    })
    .to_string();
    let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
    writeln!(f, "{line}").unwrap();

    // Index with a no-op scanner so the escaped key lands in the store unredacted.
    std::env::set_var("FUNES_TRUFFLEHOG", "/usr/bin/true");
    funes::index::run_index(source.path(), false).await.unwrap();

    // Scrub with the real scanner: every block is unredactable, so the store is rewritten empty.
    std::env::remove_var("FUNES_TRUFFLEHOG");
    funes::index::run_scrub().await.unwrap();

    // The empty Overwrite must leave a valid, reopenable store with zero rows.
    let uri = funes::dataset::table_uri(&funes::dataset::local_store_dir());
    let ds = funes::dataset::open(&uri, HashMap::new())
        .await
        .expect("store must reopen after an all-dropped scrub");
    assert_eq!(
        ds.count_rows(None).await.unwrap(),
        0,
        "every block was dropped, so the store is empty"
    );
}
