//! Gated live round-trip for `push`. Build a synthetic local store, publish it to a unique
//! scratch path on the shared test dataset (create), grow it by a turn and publish again
//! (append → capture Lance's native append), recall both markers back from the remote, then delete the
//! scratch path. No real data.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set (it provides `HF_TOKEN` for `Store::open` / the
//! hf-hub client) AND `trufflehog` is on PATH (push's pre-publish gate is fail-closed). Needs a
//! bigger thread stack than the default — lance + fastembed recurse deeply — so set
//! `RUST_MIN_STACK` (CI uses the same value). To run:
//!
//!   export HF_FUNES_TEST_TOKEN="$(cat hf_funes_test_token.txt)"
//!   RUST_MIN_STACK=16777216 cargo test --test push_round_trip -- --nocapture

use std::io::Write;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use funes::hub::Store;
use hf_hub::HFClient;

const OWNER: &str = "optimum-internal-testing";
const NAME: &str = "funes-test";

/// Write `projects/<proj>/sess.jsonl` with the given (uuid, text) user turns.
fn write_session(source: &std::path::Path, turns: &[(&str, &str)]) {
    let dir = source.join("projects").join("-synctest-proj");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("sess.jsonl")).unwrap();
    for (i, (uuid, text)) in turns.iter().enumerate() {
        writeln!(
            f,
            r#"{{"type":"user","uuid":"{uuid}","timestamp":"2026-02-01T00:00:{i:02}Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
        .unwrap();
    }
}

fn tool_ok(bin: &str, arg: &str) -> bool {
    Command::new(bin)
        .arg(arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn recall_remote(uri: &str, query: &str) -> String {
    funes::recall::recall(Store::parse(uri), query.into(), 5, 30, 0.0, 0, None, None)
        .await
        .unwrap_or_else(|e| format!("<recall error: {e}>"))
}

#[tokio::test]
async fn push_round_trip_create_append_recall() {
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
    std::env::set_var("HF_TOKEN", &token);

    // Unique scratch path so concurrent/repeated runs don't collide.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let prefix = format!("_synctest/{}-{nanos}", std::process::id());
    let uri = format!("hf://datasets/{OWNER}/{NAME}/{prefix}/lancedb");

    // Synthetic local store with one marked turn.
    let db_dir = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    write_session(src.path(), &[("s1", "SYNCSMOKE parsing transcripts into turns")]);
    funes::index::run_index(src.path(), false).await.unwrap();

    // create (first publish) → grow → append (data-only, no reindex) → recall both. The appended
    // turn is left unindexed, so recalling it back exercises Lance's brute-force fallback. Then
    // force a reindex and recall it again, now served by the index.
    let create = funes::push::run_push(Store::parse(&uri), false).await;
    write_session(
        src.path(),
        &[
            ("s1", "SYNCSMOKE parsing transcripts into turns"),
            ("s2", "SYNCSMOKE2 the continuation adds only this new turn"),
        ],
    );
    funes::index::run_index(src.path(), false).await.unwrap();
    let append = funes::push::run_push(Store::parse(&uri), false).await;
    let recall_base = recall_remote(&uri, "SYNCSMOKE parsing").await;
    let recall_new = recall_remote(&uri, "SYNCSMOKE2 continuation").await;
    // Nothing new to push, so this is a pure forced reindex: fold the unindexed appended turn into
    // the index as its own commit (capture_reindex + a separate commit), then recall it again.
    let reindex = funes::push::run_push(Store::parse(&uri), true).await;
    let recall_reindexed = recall_remote(&uri, "SYNCSMOKE2 continuation").await;
    // The model id must travel with the store (stamped in the schema metadata, uploaded by push).
    let remote_model = match Store::parse(&uri).open().await {
        Ok(t) => t.schema().metadata.get("embedding_model").cloned(),
        Err(_) => None,
    };

    // Cleanup before asserting, so a failed assertion can't leave the scratch path behind.
    let client = HFClient::builder().token(token).build().unwrap();
    let _ = client
        .dataset(OWNER, NAME)
        .delete_folder()
        .path_in_repo(prefix)
        .commit_message("cleanup funes push round-trip test")
        .send()
        .await;

    let create = create.expect("create push");
    assert!(create.contains("pushed"), "create should publish: {create}");
    let append = append.expect("append push");
    assert!(
        append.contains("pushed 1 chunks"),
        "append should publish only the new chunk: {append}"
    );
    assert!(
        recall_base.contains("SYNCSMOKE"),
        "remote recall should still surface the base turn: {recall_base}"
    );
    assert!(
        recall_new.contains("SYNCSMOKE2"),
        "remote recall should surface the appended turn: {recall_new}"
    );
    let reindex = reindex.expect("force reindex");
    assert!(
        reindex.contains("reindexed"),
        "force-reindex should commit an index delta: {reindex}"
    );
    assert!(
        recall_reindexed.contains("SYNCSMOKE2"),
        "remote recall should still surface the turn after reindex: {recall_reindexed}"
    );
    assert_eq!(
        remote_model.as_deref(),
        Some(funes::index::MODEL),
        "the model id should travel with the store via push"
    );
}
