//! Gated end-to-end: when *every* block scrub sees is an unredactable secret, the single Overwrite
//! commit must still produce a valid (empty) memory, not error on a zero-row write. Exercises the
//! all-dropped path that the mixed-block test does not. Skipped unless trufflehog and ssh-keygen
//! are available.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

#[tokio::test]
async fn scrub_dropping_every_block_leaves_a_valid_empty_memory() {
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
    // Base64-wrap the key: trufflehog still decodes and flags it (a PrivateKey finding), but the
    // stored bytes are neither the canonical key nor its JSON escaping, so excise can match neither
    // form — the block is genuinely unredactable, which is the all-dropped case this test needs.
    let Some(blob) = base64_line(&key) else {
        eprintln!("skip: base64 unavailable");
        return;
    };

    // A single session, a single block — the unredactable key and nothing else. Scrub drops it all.
    let workdir = "-home-u-dev-demo";
    let session = "scrub-all-0001";
    let dir = source.path().join("projects").join(workdir);
    std::fs::create_dir_all(&dir).unwrap();
    let line = serde_json::json!({
        "type": "user",
        "uuid": "t1",
        "timestamp": "2026-01-01T00:00:00Z",
        "message": {"role": "user", "content": format!("deploy key: {blob}")},
    })
    .to_string();
    let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
    writeln!(f, "{line}").unwrap();

    // Index with a no-op scanner so the escaped key lands in the memory unredacted.
    std::env::set_var("FUNES_TRUFFLEHOG", "/usr/bin/true");
    funes::index::run_index(source.path(), false, None).await.unwrap();

    // Scrub with the real scanner: every block is unredactable, so the memory is rewritten empty.
    std::env::remove_var("FUNES_TRUFFLEHOG");
    funes::scrub::run().await.unwrap();

    // The empty Overwrite must leave a valid, reopenable memory with zero rows.
    let uri = funes::dataset::table_uri(&funes::dataset::local_memory_dir());
    let ds = funes::dataset::open(&uri, HashMap::new())
        .await
        .expect("memory must reopen after an all-dropped scrub");
    assert_eq!(
        ds.count_rows(None).await.unwrap(),
        0,
        "every block was dropped, so the memory is empty"
    );
}

/// Base64-encode `input` to a single line (wrapping stripped), via the `base64` CLI. `None` if it's
/// absent, so the caller can skip rather than fail.
fn base64_line(input: &str) -> Option<String> {
    let mut child = Command::new("base64")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .collect::<String>()
    })
}
