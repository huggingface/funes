//! Gated end-to-end: `funes scrub` drops a row whose secret it cannot safely redact — a key stored
//! with escaped `\n` (literal backslash-n), as a JSON-encoded or logged transcript holds it. The
//! scanner's canonical `raw` (real newlines) isn't a substring of the stored bytes, so it can't be
//! excised in place; scrub locates it by line and removes the block whole. This is the exact shape
//! that once leaked. Skipped unless trufflehog and ssh-keygen are available.

use std::io::Write;
use std::process::Command;

#[tokio::test]
async fn scrub_drops_an_escaped_key_it_cannot_redact() {
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
    // A base64 line of the key body — unchanged by escaping (escaping only rewrites the newlines),
    // so it's a substring of both forms and a reliable "did the secret survive?" probe.
    let key_body = key.lines().nth(1).unwrap().to_string();
    let escaped = key.replace('\n', "\\n");
    assert!(!escaped.contains('\n') && escaped.contains(&key_body));

    let project = "-home-u-dev-demo";
    let session = "scrub-drop-0001";
    let dir = source.path().join("projects").join(project);
    std::fs::create_dir_all(&dir).unwrap();
    // Two turns: a clean one (must survive) and the escaped-key one (must be dropped).
    let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
    for (uuid, content) in [
        ("t1", "just chatting about parsers"),
        ("t2", &format!("deploy key: {escaped}")[..]),
    ] {
        let line = serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"role": "user", "content": content},
        })
        .to_string();
        writeln!(f, "{line}").unwrap();
    }

    // Index with a no-op scanner so the escaped key lands in the store unredacted.
    std::env::set_var("FUNES_TRUFFLEHOG", "/usr/bin/true");
    funes::index::run_index(source.path(), false).await.unwrap();
    let dirty = funes::recall::get(funes::hub::Store::local(), session.into(), "t2".into(), 0)
        .await
        .unwrap();
    assert!(
        dirty.contains(&key_body),
        "setup: the escaped key should be in the store before scrub"
    );

    // Scrub with the real scanner: it can't redact the escaped key, so it drops that row.
    std::env::remove_var("FUNES_TRUFFLEHOG");
    funes::index::run_scrub().await.unwrap();

    let after_key = funes::recall::get(funes::hub::Store::local(), session.into(), "t2".into(), 0)
        .await
        .unwrap_or_default();
    assert!(
        !after_key.contains(&key_body),
        "escaped key survived scrub: {after_key}"
    );
    assert!(
        !after_key.contains("[REDACTED"),
        "escaped key should be dropped, not redacted in place: {after_key}"
    );

    // The clean turn is untouched.
    let clean = funes::recall::get(funes::hub::Store::local(), session.into(), "t1".into(), 0)
        .await
        .unwrap();
    assert!(
        clean.contains("just chatting about parsers"),
        "clean turn was lost: {clean}"
    );
}
