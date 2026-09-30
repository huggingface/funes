//! Gated live test: the read-through cache behind remote reads. It publishes a synthetic memory to
//! a throwaway repo of its own (created and deleted here), so no other writer moves the head between
//! the two recalls. A first recall over `hf://` downloads the index + touched fragments into an
//! isolated HF cache; a second one is served from that cache and leaves it unchanged.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set (it provides `HF_TOKEN`) AND `trufflehog` is on
//! PATH (push's pre-publish gate is fail-closed) — and skipped with a note if the token cannot
//! create a scratch repo under the test org. Needs a bigger thread stack than the default. To
//! run:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>
//!   RUST_MIN_STACK=16777216 cargo test --test remote_cache -- --nocapture

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use funes::commands::push::Confirm;
use funes::memory::Memory;
use hf_hub::{HFClient, RepoTypeDataset};

const OWNER: &str = "optimum-internal-testing";
const MARKER: &str = "CACHESMOKE";

/// Write a turns file with one user turn.
fn write_session(source: &Path, text: &str) {
    let mut f = std::fs::File::create(source.join("sess.funes.jsonl")).unwrap();
    writeln!(
        f,
        r#"{{"format":1,"session_id":"sess","cwd":"/cachetest/proj","turn_uuid":"s1","seq":0,"ts":"2026-02-01T00:00:00Z","role":"user","blocks":[{{"block_type":"text","text":"{text}"}}],"harness":"claude"}}"#
    )
    .unwrap();
}

fn tool_ok(bin: &str, arg: &str) -> bool {
    Command::new(bin)
        .arg(arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// (entry count, total bytes) under `dir`, recursively.
fn cache_footprint(dir: &Path) -> (usize, u64) {
    let (mut entries, mut bytes) = (0usize, 0u64);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match std::fs::symlink_metadata(e.path()) {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => {
                    entries += 1;
                    if m.is_file() {
                        bytes += m.len();
                    }
                }
                _ => {}
            }
        }
    }
    (entries, bytes)
}

async fn recall(uri: &str) -> anyhow::Result<String> {
    funes::commands::recall::recall(Memory::parse(uri), MARKER.to_string(), 5, 30, 0.0, 0, None, None).await
}

#[tokio::test]
async fn warm_recall_is_served_from_cache_without_downloading() {
    // In CI, an unset/fork secret expands to "" (env::var returns Ok("")), which must also skip.
    let token = std::env::var("HF_FUNES_TEST_TOKEN")
        .unwrap_or_default()
        .trim()
        .to_string();
    if token.is_empty() {
        eprintln!("skip: HF_FUNES_TEST_TOKEN not set");
        return;
    }
    if !tool_ok("trufflehog", "--version") {
        eprintln!("skip: trufflehog not installed (push's secret gate is fail-closed)");
        return;
    }
    // funes' Memory::open authenticates via HF_TOKEN.
    std::env::set_var("HF_TOKEN", &token);
    let client = HFClient::builder().token(token).build().unwrap();

    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    write_session(src.path(), &format!("{MARKER} the only turn"));
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();

    // Unique repo name so concurrent/repeated runs don't collide.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("funes-test-cache-{}-{nanos}", std::process::id());
    if let Err(e) = funes::hub::create_dataset_repo(OWNER, &name).await {
        eprintln!("skip: cannot create a scratch repo under {OWNER}: {e}");
        return;
    }
    let uri = format!("hf://datasets/{OWNER}/{name}");
    let push = funes::commands::push::run_push(Memory::parse(&uri), false, Confirm::Yes, &[]).await;

    // Isolate the hf-hub cache to a fresh dir, so "cold" is a genuine first download and "warm"
    // can't be served by an entry left over from another run.
    let cache = tempfile::tempdir().unwrap();
    std::env::set_var("HF_HUB_CACHE", cache.path());
    let before = cache_footprint(cache.path());

    // Cold: the read wrapper downloads the index + touched fragments into the cache.
    let cold = recall(&uri).await;
    let after_cold = cache_footprint(cache.path());

    // Warm: same head commit ⇒ every file is already cached ⇒ the cache stays as it was.
    let warm = recall(&uri).await;
    let after_warm = cache_footprint(cache.path());

    // Cleanup before asserting, so a failed assertion can't leave the scratch repo behind.
    let _ = client
        .delete_repository()
        .repo_id(format!("{OWNER}/{name}"))
        .repo_type(RepoTypeDataset)
        .send()
        .await;

    push.expect("publishing the scratch memory");
    assert_eq!(before, (0, 0), "cache must start empty");
    let cold = cold.expect("cold recall");
    assert!(cold.contains(MARKER), "cold recall should surface the marker: {cold}");
    assert!(
        after_cold.0 > 0,
        "cold recall must populate the cache, got {after_cold:?}"
    );
    let warm = warm.expect("warm recall");
    assert!(warm.contains(MARKER), "warm recall should surface the marker: {warm}");
    assert_eq!(
        after_warm, after_cold,
        "warm recall must be served from the cache — cache changed: cold={after_cold:?} warm={after_warm:?}"
    );
}
