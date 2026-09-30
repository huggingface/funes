//! Gated live test: a forced reindex builds the indexes a remote memory lacks. The remote is a copy
//! of a synthetic local memory written without any index — as a hand-uploaded snapshot is — so
//! recall refuses it until `push --force-reindex` gives it a text index.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set. To run:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>
//!   RUST_MIN_STACK=16777216 cargo test --test push_reindex_missing_index -- --nocapture

use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::RecordBatchIterator;
use funes::commands::push::Confirm;
use funes::memory::{dataset, Memory};
use hf_hub::repository::CommitOperation;
use hf_hub::HFClient;
use lance::Dataset;

const OWNER: &str = "optimum-internal-testing";
const NAME: &str = "funes-test";

async fn recall_remote(uri: &str, query: &str) -> String {
    funes::commands::recall::recall(Memory::parse(uri), query.into(), 5, 30, 0.0, 0, None, None)
        .await
        .unwrap_or_else(|e| format!("<recall error: {e}>"))
}

#[tokio::test]
async fn force_reindex_builds_the_indexes_a_remote_lacks() {
    let token = std::env::var("HF_FUNES_TEST_TOKEN")
        .unwrap_or_default()
        .trim()
        .to_string();
    if token.is_empty() {
        eprintln!("skip: HF_FUNES_TEST_TOKEN not set");
        return;
    }
    std::env::set_var("HF_TOKEN", &token);
    let client = HFClient::builder().token(token).build().unwrap();
    let repo = client.dataset(OWNER, NAME);

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let prefix = format!("_synctest/{}-{nanos}", std::process::id());
    let uri = format!("hf://datasets/{OWNER}/{NAME}/{prefix}/lancedb");

    // A local memory holding one marked turn: the push below finds nothing new to publish.
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let mut f = std::fs::File::create(src.path().join("sess.funes.jsonl")).unwrap();
    writeln!(
        f,
        r#"{{"format":1,"session_id":"sess","cwd":"/synctest/proj","turn_uuid":"s1","seq":0,"ts":"2026-02-01T00:00:00Z","role":"user","blocks":[{{"block_type":"text","text":"NOINDEXSMOKE the remote carries no index yet"}}],"harness":"claude"}}"#
    )
    .unwrap();
    drop(f);
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();

    // The same rows, rewritten without any index, uploaded under the scratch prefix.
    let local = Memory::local().open().await.unwrap();
    let batches = dataset::scan_rows(&local, &[], None, None).await.unwrap();
    let schema = Arc::new(arrow_schema::Schema::from(local.schema()));
    let staging = tempfile::tempdir().unwrap();
    let base = staging.path().join(&prefix).join("lancedb");
    std::fs::create_dir_all(&base).unwrap();
    let table = dataset::table_uri(&base.to_string_lossy());
    Dataset::write(
        RecordBatchIterator::new(batches.into_iter().map(Ok), schema),
        &table,
        None,
    )
    .await
    .unwrap();
    let ops: Vec<CommitOperation> = walkdir::WalkDir::new(staging.path())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            let rel = e.path().strip_prefix(staging.path()).unwrap();
            CommitOperation::add_file(rel.to_string_lossy().into_owned(), e.path().to_path_buf())
        })
        .collect();
    repo.create_commit()
        .operations(ops)
        .commit_message("funes test: a memory without indexes")
        .send()
        .await
        .expect("uploading the index-less memory");

    let before = recall_remote(&uri, "NOINDEXSMOKE").await;
    let reindex = funes::commands::push::run_push(Memory::parse(&uri), true, Confirm::Yes, &[]).await;
    let after = recall_remote(&uri, "NOINDEXSMOKE").await;

    // Cleanup before asserting, so a failed assertion can't leave the scratch path behind.
    let _ = repo
        .delete_folder()
        .path_in_repo(prefix.clone())
        .commit_message("cleanup funes missing-index test")
        .send()
        .await;

    assert!(
        before.contains("recall error"),
        "recall should refuse a memory with no text index: {before}"
    );
    let reindex = reindex.expect("force reindex").report;
    assert!(
        reindex.contains("reindexed"),
        "force-reindex should build the missing indexes: {reindex}"
    );
    assert!(
        after.contains("NOINDEXSMOKE"),
        "recall should serve the memory once its text index exists: {after}"
    );
}
