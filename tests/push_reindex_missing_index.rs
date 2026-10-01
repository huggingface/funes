//! Gated live test: a push builds the indexes a remote memory lacks. Each remote is a copy of a
//! synthetic local memory written without any index — as a hand-uploaded snapshot is — so recall
//! refuses it until a push gives it a text index: a forced reindex with nothing new to publish, and
//! an ordinary push of one new chunk, far below the reindex threshold.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set AND `trufflehog` is on PATH (push's pre-publish gate
//! is fail-closed). To run:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>
//!   RUST_MIN_STACK=16777216 cargo test --test push_reindex_missing_index -- --nocapture

use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::RecordBatchIterator;
use funes::commands::push::Confirm;
use funes::memory::{dataset, Memory};
use hf_hub::repository::CommitOperation;
use hf_hub::{HFClient, HFRepository, RepoTypeDataset};
use lance::Dataset;

const OWNER: &str = "optimum-internal-testing";
const NAME: &str = "funes-test";

async fn recall_remote(uri: &str, query: &str) -> String {
    funes::commands::recall::recall(Memory::parse(uri), query.into(), 5, 30, 0, Default::default())
        .await
        .unwrap_or_else(|e| format!("<recall error: {e}>"))
}

/// Write a turns file with the given user turns, one a second, and index it.
async fn index_turns(source: &std::path::Path, texts: &[&str]) {
    let mut f = std::fs::File::create(source.join("sess.funes.jsonl")).unwrap();
    for (i, text) in texts.iter().enumerate() {
        writeln!(
            f,
            r#"{{"format":1,"session_id":"sess","cwd":"/synctest/proj","turn_uuid":"s{i}","seq":{i},"ts":"2026-02-01T00:00:{i:02}Z","role":"user","blocks":[{{"block_type":"text","text":"{text}"}}],"harness":"claude"}}"#
        )
        .unwrap();
    }
    drop(f);
    funes::commands::index::run_index(source, false, None).await.unwrap();
}

/// Upload the local memory's rows, rewritten without any index, as the memory at `<path>/lancedb`.
async fn upload_without_indexes(repo: &HFRepository<RepoTypeDataset>, path: &str) {
    let local = Memory::local().open().await.unwrap();
    let batches = dataset::scan_rows(&local, &[], None, None).await.unwrap();
    let schema = Arc::new(arrow_schema::Schema::from(local.schema()));
    let staging = tempfile::tempdir().unwrap();
    let base = staging.path().join(path).join("lancedb");
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
}

#[tokio::test]
async fn push_builds_the_indexes_a_remote_lacks() {
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
    let client = HFClient::builder().token(token).build().unwrap();
    let repo = client.dataset(OWNER, NAME);

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let prefix = format!("_synctest/{}-{nanos}", std::process::id());
    let forced = format!("hf://datasets/{OWNER}/{NAME}/{prefix}/forced/lancedb");
    let auto = format!("hf://datasets/{OWNER}/{NAME}/{prefix}/auto/lancedb");

    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    index_turns(src.path(), &["NOINDEXSMOKE the remote carries no index yet"]).await;
    upload_without_indexes(&repo, &format!("{prefix}/forced")).await;
    upload_without_indexes(&repo, &format!("{prefix}/auto")).await;

    let before = recall_remote(&forced, "NOINDEXSMOKE").await;

    // Nothing new to publish: a forced reindex only.
    let reindex = funes::commands::push::run_push(Memory::parse(&forced), true, Confirm::Yes, &[]).await;
    let after_forced = recall_remote(&forced, "NOINDEXSMOKE").await;

    // One new chunk, pushed without forcing.
    index_turns(
        src.path(),
        &[
            "NOINDEXSMOKE the remote carries no index yet",
            "NOINDEXSMOKE2 one more turn to publish",
        ],
    )
    .await;
    let append = funes::commands::push::run_push(Memory::parse(&auto), false, Confirm::Yes, &[]).await;
    let after_auto = recall_remote(&auto, "NOINDEXSMOKE").await;

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
        after_forced.contains("NOINDEXSMOKE"),
        "recall should serve the memory once its text index exists: {after_forced}"
    );
    let append = append.expect("push").report;
    assert!(
        append.contains("pushed 1 chunks") && append.contains("reindexed"),
        "a push to a memory with no text index should build it: {append}"
    );
    assert!(
        after_auto.contains("NOINDEXSMOKE"),
        "recall should serve the memory the push indexed: {after_auto}"
    );
}
