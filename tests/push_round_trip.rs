//! Gated live round-trip for `push`. Build a synthetic local store, publish it to a unique
//! scratch path on the shared test dataset (create), grow it by a turn and publish again
//! (append → capture Lance's native append), recall both markers back from the remote, then delete the
//! scratch path. No real data.
//!
//! Also covers the no-overlap confirmation gate against the live remote: declining a first publish
//! (the local index shares nothing with the empty remote) must abort and upload nothing, while an
//! append to a store that already shares chunks must not be prompted at all.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set (it provides `HF_TOKEN` for `Store::open` / the
//! hf-hub client) AND `trufflehog` is on PATH (push's pre-publish gate is fail-closed). Needs a
//! bigger thread stack than the default — lance + fastembed recurse deeply — so set
//! `RUST_MIN_STACK` (CI uses the same value). To run:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>
//!   RUST_MIN_STACK=16777216 cargo test --test push_round_trip -- --nocapture

use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use funes::hub::Store;
use funes::push::Confirm;
use hf_hub::{HFClient, HFError, HFRepository, RepoTypeDataset};

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

/// The repo-root README, or None when the repo has none. Any other failure panics: a transport
/// error must fail the test loudly, not masquerade as "no README".
async fn root_readme(repo: &HFRepository<RepoTypeDataset>) -> Option<String> {
    match repo.download_file_to_bytes().filename("README.md").send().await {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(HFError::EntryNotFound { .. }) => None,
        Err(e) => panic!("querying the repo-root README: {e}"),
    }
}

async fn recall_remote(uri: &str, query: &str) -> String {
    funes::recall::recall(Store::parse(uri), query.into(), 5, 30, 0.0, 0, None, None)
        .await
        .unwrap_or_else(|e| format!("<recall error: {e}>"))
}

/// Push confirmations, recorded so the test can assert whether — and with what pending-chunk count —
/// the no-overlap gate consulted the prompt. Safe as process globals: this binary runs one test.
static PROMPTS: AtomicUsize = AtomicUsize::new(0);
static LAST_CHUNKS: AtomicUsize = AtomicUsize::new(0);

/// A confirmation that records the call and declines.
fn decline(_label: &str, chunks: usize) -> bool {
    PROMPTS.fetch_add(1, Ordering::SeqCst);
    LAST_CHUNKS.store(chunks, Ordering::SeqCst);
    false
}

/// A confirmation that records the call and accepts.
fn accept(_label: &str, chunks: usize) -> bool {
    PROMPTS.fetch_add(1, Ordering::SeqCst);
    LAST_CHUNKS.store(chunks, Ordering::SeqCst);
    true
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
    let client = HFClient::builder().token(token).build().unwrap();
    let repo = client.dataset(OWNER, NAME);

    // The store lives under a prefix, so no push here may ever touch the repo-root README (the
    // dataset card is root-stores-only). Snapshot it now, compare after every push ran.
    let readme_before = root_readme(&repo).await;

    // Unique scratch path so concurrent/repeated runs don't collide.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let prefix = format!("_synctest/{}-{nanos}", std::process::id());
    let uri = format!("hf://datasets/{OWNER}/{NAME}/{prefix}/lancedb");

    // Synthetic local store with one marked turn.
    let db_dir = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    write_session(src.path(), &[("s1", "SYNCSMOKE parsing transcripts into turns")]);
    funes::index::run_index(src.path(), false, None).await.unwrap();

    // Gate: the local index shares nothing with the (empty) remote, so a first publish is prompted.
    // Declining must abort and upload nothing — the "don't publish to the wrong store" promise.
    let declined = funes::push::run_push(Store::parse(&uri), false, Confirm::Ask(decline)).await;
    // Verify via a *successful* Hub query that the declined push published nothing: get_paths_info
    // returns the entries present under the path (Ok([]) when absent) and Err on a transport failure,
    // so an unreachable remote fails the test loudly instead of masquerading as "nothing uploaded".
    let after_decline = repo
        .get_paths_info()
        .paths(vec![prefix.clone()])
        .send()
        .await
        .expect("querying the remote for the declined scratch path");

    // create (first publish): accept the same gate → grow → append (data-only, no reindex) → recall
    // both. The appended turn is left unindexed, so recalling it back exercises Lance's brute-force
    // fallback. Then force a reindex and recall it again, now served by the index.
    let create = funes::push::run_push(Store::parse(&uri), false, Confirm::Ask(accept)).await;
    write_session(
        src.path(),
        &[
            ("s1", "SYNCSMOKE parsing transcripts into turns"),
            ("s2", "SYNCSMOKE2 the continuation adds only this new turn"),
        ],
    );
    funes::index::run_index(src.path(), false, None).await.unwrap();
    // append: the grown local store now shares chunks with the remote, so the gate must NOT fire —
    // pass a prompt that would decline and assert it is never consulted.
    let prompts_before_append = PROMPTS.load(Ordering::SeqCst);
    let append = funes::push::run_push(Store::parse(&uri), false, Confirm::Ask(decline)).await;
    let prompts_after_append = PROMPTS.load(Ordering::SeqCst);
    let recall_base = recall_remote(&uri, "SYNCSMOKE parsing").await;
    let recall_new = recall_remote(&uri, "SYNCSMOKE2 continuation").await;
    // Nothing new to push, so this is a pure forced reindex: fold the unindexed appended turn into
    // the index as its own commit (capture_reindex + a separate commit), then recall it again.
    let reindex = funes::push::run_push(Store::parse(&uri), true, Confirm::Yes).await;
    let recall_reindexed = recall_remote(&uri, "SYNCSMOKE2 continuation").await;
    let readme_after = root_readme(&repo).await;
    // The model id must travel with the store (stamped in the schema metadata, uploaded by push).
    let remote_model = match Store::parse(&uri).open().await {
        Ok(t) => t.schema().metadata.get("embedding_model").cloned(),
        Err(_) => None,
    };

    // Cleanup before asserting, so a failed assertion can't leave the scratch path behind.
    let _ = repo
        .delete_folder()
        .path_in_repo(prefix.clone())
        .commit_message("cleanup funes push round-trip test")
        .send()
        .await;

    // Gate: declining a first publish aborts and leaves the remote empty (nothing was uploaded).
    let declined = match declined {
        Ok(p) => panic!(
            "declining the no-overlap gate must abort, but push reported: {}",
            p.report
        ),
        Err(e) => e,
    };
    assert!(
        declined.to_string().contains("aborted"),
        "a declined push should report an abort, got: {declined}"
    );
    assert!(
        after_decline.is_empty(),
        "a declined push must upload nothing, but the remote holds {} entry(ies) under {prefix}",
        after_decline.len()
    );
    assert!(
        LAST_CHUNKS.load(Ordering::SeqCst) > 0,
        "the confirmation should be told how many chunks are pending"
    );
    // Gate: a store that already shares chunks with the local index is not prompted.
    assert_eq!(
        prompts_after_append, prompts_before_append,
        "an append to a store you already share chunks with must not trigger the confirmation"
    );

    let create = create.expect("create push").report;
    assert!(create.contains("pushed"), "create should publish: {create}");
    let append = append.expect("append push").report;
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
    let reindex = reindex.expect("force reindex").report;
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
    assert_eq!(
        readme_before, readme_after,
        "a prefixed push must never touch the repo-root README (the card is root-stores-only)"
    );
}
