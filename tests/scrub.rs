//! Gated end-to-end: `funes scrub` redacts a secret already sitting in the store, in place, without
//! its source. Skipped unless trufflehog and ssh-keygen are available.
//!
//! A dirty store is created by indexing with `FUNES_TRUFFLEHOG=/usr/bin/true` — `true` ignores its
//! args and exits 0, so the scan finds nothing and the secret is stored unredacted (simulating a
//! store built before redaction existed). Then scrub runs with the real scanner and cleans it.

use std::io::Write;
use std::process::Command;

#[tokio::test]
async fn scrub_redacts_an_existing_secret_in_place() {
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
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        eprintln!("skip: ssh-keygen unavailable");
        return;
    }
    let key = std::fs::read_to_string(&keyfile).unwrap();
    std::fs::remove_file(&keyfile).unwrap();
    let key_body = key.lines().nth(1).unwrap().to_string();

    let workdir = "-home-u-dev-demo";
    let session = "scrub-session-0001";
    let dir = source.path().join("projects").join(workdir);
    std::fs::create_dir_all(&dir).unwrap();
    let content = format!("deploy key:\n{key}");
    let line = serde_json::json!({
        "type": "user",
        "uuid": "t1",
        "timestamp": "2026-01-01T00:00:00Z",
        "message": {"role": "user", "content": content},
    })
    .to_string();
    let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
    writeln!(f, "{line}").unwrap();

    // Index with a no-op scanner so the secret lands in the store unredacted.
    std::env::set_var("FUNES_TRUFFLEHOG", "/usr/bin/true");
    funes::index::run_index(source.path(), false, None).await.unwrap();
    let dirty = funes::recall::get(funes::hub::Store::local(), session.into(), "t1".into(), 3)
        .await
        .unwrap();
    assert!(
        dirty.contains(&key_body),
        "setup: the key should be in the store before scrub"
    );

    // Scrub with the real scanner: it must redact the key in place.
    std::env::remove_var("FUNES_TRUFFLEHOG");
    funes::scrub::run().await.unwrap();
    let clean = funes::recall::get(funes::hub::Store::local(), session.into(), "t1".into(), 3)
        .await
        .unwrap();
    assert!(
        clean.contains("[REDACTED:PrivateKey]"),
        "expected a redaction marker after scrub: {clean}"
    );
    assert!(!clean.contains(&key_body), "key body survived scrub: {clean}");
}
