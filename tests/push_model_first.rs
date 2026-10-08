//! Gated live test: a first push from a local memory with no rows gives the remote its embedding
//! model, and the next push appends onto it. Publishes to a unique scratch path on the shared test
//! dataset, then deletes it. No real data.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set AND `trufflehog` is on PATH (the second push scans
//! its row). To run:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>
//!   RUST_MIN_STACK=16777216 cargo test --test push_model_first -- --nocapture

use std::io::Write;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use funes::commands::index;
use funes::commands::push::{self, Confirm};
use funes::inference::EmbeddingModel;
use funes::memory::{dataset, Memory, MemoryState};
use hf_hub::HFClient;

const OWNER: &str = "optimum-internal-testing";
const NAME: &str = "funes-test";

fn never_asked(_label: &str, _chunks: usize) -> bool {
    panic!("a push of no chunks asks nothing");
}

/// The remote's model and row count, when it holds a dataset.
async fn remote_model_and_rows(uri: &str) -> Option<(EmbeddingModel, usize)> {
    match Memory::parse(uri).state().await {
        Ok(MemoryState::Ready(ds)) => Some((
            dataset::embedding_model(&ds).unwrap(),
            ds.count_rows(None).await.unwrap(),
        )),
        _ => None,
    }
}

#[tokio::test]
async fn a_first_push_with_no_rows_gives_the_remote_its_model() {
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
    let e5 = EmbeddingModel::MultilingualE5Small;
    index::ensure_local_memory(e5).await.unwrap();
    let created = push::run_push(Memory::parse(&uri), false, Confirm::Ask(never_asked), &[]).await;
    let after_create = remote_model_and_rows(&uri).await;

    let src = tempfile::tempdir().unwrap();
    let mut f = std::fs::File::create(src.path().join("sess.funes.jsonl")).unwrap();
    writeln!(
        f,
        r#"{{"format":1,"session_id":"sess","cwd":"/synctest/proj","turn_uuid":"s1","seq":0,"ts":"2026-02-01T00:00:00Z","role":"user","blocks":[{{"block_type":"text","text":"给表加上索引"}}],"harness":"claude"}}"#
    )
    .unwrap();
    index::run_index(src.path(), false, None).await.unwrap();
    let appended = push::run_push(Memory::parse(&uri), false, Confirm::Yes, &[]).await;
    let after_append = remote_model_and_rows(&uri).await;

    // Cleanup before asserting, so a failed assertion can't leave the scratch path behind.
    let _ = repo
        .delete_folder()
        .path_in_repo(prefix.clone())
        .commit_message("cleanup funes first-push model test")
        .send()
        .await;

    let report = created.unwrap().report;
    assert!(report.contains(&format!("created with {}", e5.id())), "{report}");
    assert_eq!(after_create, Some((e5, 0)));
    let report = appended.unwrap().report;
    assert!(report.contains("pushed 1 chunks"), "{report}");
    assert_eq!(after_append, Some((e5, 1)));
}
