//! Gated end-to-end: a secret planted in a transcript is redacted out of the store at index time,
//! so it never reaches recall — or, via push, the Hub. Skipped unless trufflehog (the scanner) and
//! ssh-keygen (to mint a throwaway key) are both available.

use std::io::Write;
use std::process::Command;

#[tokio::test]
async fn planted_key_is_redacted_at_index_time() {
    if funes::scan::Trufflehog::find().is_err() {
        eprintln!("skip: trufflehog not found");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());

    // Mint a throwaway private key (never committed) and plant it in a transcript.
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
    // A distinctive slice of the key body — must not survive into the store.
    let key_body = key.lines().nth(1).unwrap().to_string();
    assert!(key_body.len() > 20);

    let project = "-home-u-dev-demo";
    let session = "redact-session-0001";
    let dir = source.path().join("projects").join(project);
    std::fs::create_dir_all(&dir).unwrap();
    let content = format!("here is my deploy key, keep it safe:\n{key}");
    let line = serde_json::json!({
        "type": "user",
        "uuid": "t1",
        "timestamp": "2026-01-01T00:00:00Z",
        "message": {"role": "user", "content": content},
    })
    .to_string();
    let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
    writeln!(f, "{line}").unwrap();

    // Index for real — redaction runs over the block text before chunking/embedding/storing.
    funes::index::run_index(source.path(), false).await.unwrap();

    // Read the stored turn back: the marker is present, the key body is gone.
    let got = funes::recall::get(funes::hub::Store::local(), session.into(), "t1".into(), 3)
        .await
        .unwrap();
    assert!(
        got.contains("[REDACTED:PrivateKey]"),
        "expected a redaction marker, got: {got}"
    );
    assert!(!got.contains(&key_body), "key body leaked into the store: {got}");
}
