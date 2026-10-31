//! Gated live test: what `bind` and `add` owe a memory on the Hub. A memory this host never
//! published to is owed a first push, one holding this host's chunks is not, and one emptied since
//! is owed one again. Publishes to a unique scratch path on the shared test dataset, then deletes
//! it. No real data.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set AND `trufflehog` is on PATH (the push scans its
//! row). To run:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>
//!   RUST_MIN_STACK=16777216 cargo test --test bind_first_push_owed -- --nocapture

use std::io::Write;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use funes::commands::bind::owes_first_push;
use funes::commands::index;
use funes::commands::push::{self, Confirm};
use funes::memory::Memory;
use hf_hub::HFClient;

const OWNER: &str = "optimum-internal-testing";
const NAME: &str = "funes-test";

#[tokio::test]
async fn a_first_push_is_owed_to_a_memory_new_to_this_host_or_emptied_since() {
    let token = std::env::var("HF_FUNES_TEST_TOKEN")
        .unwrap_or_default()
        .trim()
        .to_string();
    if token.is_empty() {
        eprintln!("skip: HF_FUNES_TEST_TOKEN not set");
        return;
    }
    if !Command::new("trufflehog")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skip: trufflehog not installed (push's secret gate is fail-closed)");
        return;
    }
    std::env::set_var("HF_TOKEN", &token);
    let repo = HFClient::builder().token(token).build().unwrap().dataset(OWNER, NAME);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let prefix = format!("_synctest/{}-{nanos}", std::process::id());
    let uri = format!("hf://datasets/{OWNER}/{NAME}/{prefix}/lancedb");

    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let src = tempfile::tempdir().unwrap();
    let mut f = std::fs::File::create(src.path().join("sess.funes.jsonl")).unwrap();
    writeln!(
        f,
        r#"{{"format":1,"session_id":"sess","cwd":"/synctest/proj","turn_uuid":"s1","seq":0,"ts":"2026-02-01T00:00:00Z","role":"user","blocks":[{{"block_type":"text","text":"a turn to publish"}}],"harness":"claude"}}"#
    )
    .unwrap();
    drop(f);
    index::run_index(src.path(), false, None).await.unwrap();

    // Never published there: owed. Published there: not, the hooks' next push goes through.
    let before = owes_first_push(&uri, false).await;
    let pushed = push::run_push(Memory::parse(&uri), false, Confirm::Yes, &[]).await;
    let after = owes_first_push(&uri, false).await;

    // The dataset deleted from under a receipt that still names its chunks: owed again, as the
    // first publish the hooks would refuse off a terminal.
    let emptied = repo
        .delete_folder()
        .path_in_repo(prefix.clone())
        .commit_message("funes first-push test: empty the memory")
        .send()
        .await;
    let again = owes_first_push(&uri, false).await;

    // Cleanup before asserting, so a failed assertion can't leave the scratch path behind.
    let _ = repo
        .delete_folder()
        .path_in_repo(prefix.clone())
        .commit_message("cleanup funes first-push test")
        .send()
        .await;

    assert!(before.unwrap(), "a memory this host never published to");
    assert!(pushed.unwrap().report.contains("pushed 1 chunks"));
    assert!(!after.unwrap(), "a memory holding this host's chunks");
    emptied.unwrap();
    assert!(again.unwrap(), "a memory emptied since this host published there");
}
