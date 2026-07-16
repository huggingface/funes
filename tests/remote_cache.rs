//! Gated live test: the read-through cache behind remote reads. A first recall over `hf://`
//! downloads the fixture's files (index + touched fragments) into an isolated HF cache; a second
//! recall at the same head commit is served from that cache and downloads nothing. Skipped unless
//! `HF_FUNES_TEST_TOKEN` is in the environment — to run it:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>   # or a CI secret
//!   cargo test --test remote_cache -- --nocapture
//!
//! The fixture is a stable, synthetic, read-only dataset (no real data).

use std::path::Path;

use funes::hub::Store;

const FIXTURE_URI: &str = "hf://datasets/optimum-internal-testing/funes-test/fixture/lancedb";
const MARKER: &str = "UNIQUEMARKERXYZZY";

/// (entry count, total bytes) under `dir`, recursively. A download grows both; a cache hit grows
/// neither — so comparing this across two recalls detects whether the second one fetched anything.
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

async fn recall(query: &str) -> String {
    funes::recall::recall(Store::parse(FIXTURE_URI), query.to_string(), 5, 30, 0.0, 0, None, None)
        .await
        .expect("recall over remote fixture")
}

#[tokio::test]
async fn warm_recall_is_served_from_cache_without_downloading() {
    // In CI, an unset/fork secret expands to "" (env::var returns Ok("")), which must also skip.
    let token = std::env::var("HF_FUNES_TEST_TOKEN").unwrap_or_default();
    let token = token.trim();
    if token.is_empty() {
        eprintln!("skip: HF_FUNES_TEST_TOKEN not set");
        return;
    }
    // funes' Store::open authenticates via HF_TOKEN.
    std::env::set_var("HF_TOKEN", token);

    // Isolate the hf-hub cache to a fresh dir, so "cold" is a genuine first download and "warm"
    // can't be served by an entry left over from another run.
    let cache = tempfile::tempdir().unwrap();
    std::env::set_var("HF_HUB_CACHE", cache.path());
    assert_eq!(cache_footprint(cache.path()), (0, 0), "cache must start empty");

    // Cold: the read wrapper downloads the fixture's index + touched fragments into the cache.
    let cold = recall(MARKER).await;
    assert!(cold.contains(MARKER), "cold recall should surface the marker: {cold}");
    let after_cold = cache_footprint(cache.path());
    assert!(
        after_cold.0 > 0,
        "cold recall must populate the cache, got {after_cold:?}"
    );

    // Warm: same head commit ⇒ every file is already cached ⇒ nothing is downloaded.
    let warm = recall(MARKER).await;
    assert!(warm.contains(MARKER), "warm recall should surface the marker: {warm}");
    let after_warm = cache_footprint(cache.path());
    assert_eq!(
        after_warm, after_cold,
        "warm recall must download nothing — cache changed: cold={after_cold:?} warm={after_warm:?}"
    );
}
