//! Gated live test: resolve a real Hub trace dataset's `refs/convert/parquet` branch, download a
//! shard, and read turns from it — the surface `funes index <org/repo>` is built on. Skipped unless
//! `HF_FUNES_TEST_TOKEN` is in the environment — to run it:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>   # or a CI secret
//!   cargo test --test remote_repo_source -- --nocapture
//!
//! Reads at the source level (resolve + download + parse), not the full index pipeline — the
//! end-to-end `funes index` + status run is `tests/index_real_repo.rs`.

use funes::source;

const OWNER: &str = "julien-c";
const NAME: &str = "pi-sessions";

#[tokio::test]
async fn open_remote_resolves_downloads_and_reads_a_shard() {
    // Gate on a non-empty token. In CI, `${{ secrets.X }}` for a fork/unset secret expands to an
    // empty string (env::var returns Ok("")), which must also skip.
    let token = std::env::var("HF_FUNES_TEST_TOKEN").unwrap_or_default();
    let token = token.trim();
    if token.is_empty() {
        eprintln!("skip: HF_FUNES_TEST_TOKEN not set");
        return;
    }
    // open_remote authenticates via hub::hf_token(), which reads HF_TOKEN.
    std::env::set_var("HF_TOKEN", token);

    // Resolve the convert branch + download shards; cap at one shard to keep the test light.
    let src = source::open_remote(OWNER, NAME, Some(1))
        .await
        .expect("resolve + download the pi-sessions convert parquet");

    let desc = src.describe();
    eprintln!("{desc}");
    assert!(desc.contains("julien-c/pi-sessions"), "describe: {desc}");
    assert!(desc.contains("refs/convert/parquet"), "describe: {desc}");

    let units = src.units().expect("list shards");
    assert!(!units.is_empty(), "no parquet shards resolved");
    // Every shard is signed by the convert-branch commit — the incremental skip signal.
    assert!(
        units.iter().all(|u| u.signature.is_some()),
        "shard missing convert-oid signature"
    );

    let turns = src.read(&units[0]).expect("read turns from the shard");
    assert!(!turns.is_empty(), "shard parsed to zero turns");
    // The converted parquet carries a per-row harness; every turn must inherit one.
    assert!(
        turns.iter().all(|t| !t.harness.is_empty()),
        "a turn is missing its harness"
    );
    eprintln!("read {} turns, harness = {:?}", turns.len(), turns[0].harness);
}
