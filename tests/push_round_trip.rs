//! Gated live round-trip for `push`. Build a synthetic local memory, publish it to a unique
//! scratch path on the shared test dataset (create), grow it by a turn and publish again
//! (append → capture Lance's native append), recall both markers back from the remote, then delete the
//! scratch path. No real data.
//!
//! Also covers the no-overlap confirmation gate against the live remote: declining a first publish
//! (the local index shares nothing with the empty remote) must abort and upload nothing, while an
//! append to a memory that already shares chunks must not be prompted at all.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set (it provides `HF_TOKEN` for `Memory::open` / the
//! hf-hub client) AND `trufflehog` is on PATH (push's pre-publish gate is fail-closed). Needs a
//! bigger thread stack than the default — lance + fastembed recurse deeply — so set
//! `RUST_MIN_STACK` (CI uses the same value). To run:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>
//!   RUST_MIN_STACK=16777216 cargo test --test push_round_trip -- --nocapture

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use funes::commands::push::{Confirm, Disjoint};
use funes::commands::recall;
use funes::commands::target::Target;
use funes::memory::Memory;
use hf_hub::HFClient;

const OWNER: &str = "optimum-internal-testing";
const NAME: &str = "funes-test";

/// The remote's row count and its distinct chunk-id count.
async fn remote_rows_and_ids(uri: &str) -> (usize, usize) {
    let ds = Memory::parse(uri).open().await.expect("opening the remote");
    let batches = funes::memory::dataset::scan_rows(&ds, &["id"], None, None)
        .await
        .expect("scanning the remote ids");
    let mut rows = 0usize;
    let mut ids = std::collections::HashSet::new();
    for b in &batches {
        rows += b.num_rows();
        let col = b
            .column_by_name("id")
            .and_then(|c| c.as_any().downcast_ref::<arrow_array::StringArray>())
            .expect("an id column");
        for i in 0..b.num_rows() {
            ids.insert(col.value(i).to_string());
        }
    }
    (rows, ids.len())
}

/// A recall of `uri` as this host's bound memory, pooled with its turns not yet pushed there: what
/// a read naming no memory runs on a host bound to it.
async fn recall_bound(uri: &str, query: &str) -> String {
    recall::recall(
        Target::bound(Memory::parse(uri)),
        query.into(),
        5,
        30,
        0,
        Default::default(),
    )
    .await
    .unwrap()
}

/// Push confirmations, recorded so the test can assert whether — and with what pending-chunk count —
/// the no-overlap gate consulted the prompt. Safe as process globals: this binary runs one test.
static PROMPTS: AtomicUsize = AtomicUsize::new(0);
static LAST_CHUNKS: AtomicUsize = AtomicUsize::new(0);

/// A confirmation that records the call and declines.
fn decline(_label: &str, chunks: usize, _why: Disjoint) -> bool {
    PROMPTS.fetch_add(1, Ordering::SeqCst);
    LAST_CHUNKS.store(chunks, Ordering::SeqCst);
    false
}

/// A confirmation that records the call and accepts: only a first publish reaches it here.
fn accept(_label: &str, chunks: usize, why: Disjoint) -> bool {
    assert_eq!(why, Disjoint::FirstPublish, "the create is a first publish");
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
    if !support::tool_ok("trufflehog", "--version") {
        eprintln!("skip: trufflehog not installed (push's secret gate is fail-closed)");
        return;
    }
    std::env::set_var("HF_TOKEN", &token);
    let client = HFClient::builder().token(token).build().unwrap();
    let repo = client.dataset(OWNER, NAME);

    // The memory lives under a prefix, so no push here may ever touch the repo-root README (the
    // dataset card is root-stores-only). Snapshot it now, compare after every push ran.
    let readme_before = support::root_readme(&repo).await;

    // Unique scratch path so concurrent/repeated runs don't collide.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let prefix = format!("_synctest/{}-{nanos}", std::process::id());
    let uri = format!("hf://datasets/{OWNER}/{NAME}/{prefix}/lancedb");

    // Synthetic local memory with one marked turn.
    let db_dir = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    support::write_session(src.path(), "sess", &["SYNCSMOKE parsing transcripts into turns"]);
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();

    // Gate: the local index shares nothing with the (empty) remote, so a first publish is prompted.
    // Declining must abort and upload nothing — the "don't publish to the wrong memory" promise.
    let declined = funes::commands::push::run_push(Memory::parse(&uri), false, Confirm::Ask(decline), &[]).await;
    // Verify via a *successful* Hub query that the declined push published nothing: get_paths_info
    // returns the entries present under the path (Ok([]) when absent) and Err on a transport failure,
    // so an unreachable remote fails the test loudly instead of masquerading as "nothing uploaded".
    let after_decline = repo
        .get_paths_info()
        .paths(vec![prefix.clone()])
        .send()
        .await
        .expect("querying the remote for the declined scratch path");

    // create (first publish): accept the same gate → grow → append (data-only, no compaction) →
    // recall both. The appended turn is left unindexed, so recalling it back exercises Lance's
    // brute-force fallback. Then force a compaction and recall it again, now served by the index.
    let create = funes::commands::push::run_push(Memory::parse(&uri), false, Confirm::Ask(accept), &[]).await;
    support::write_session(
        src.path(),
        "sess",
        &[
            "SYNCSMOKE parsing transcripts into turns",
            "SYNCSMOKE2 the continuation adds only this new turn",
        ],
    );
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    // append: the grown local memory now shares chunks with the remote, so the gate must NOT fire —
    // pass a prompt that would decline and assert it is never consulted.
    let prompts_before_append = PROMPTS.load(Ordering::SeqCst);
    let append = funes::commands::push::run_push(Memory::parse(&uri), false, Confirm::Ask(decline), &[]).await;
    let prompts_after_append = PROMPTS.load(Ordering::SeqCst);
    let recall_base = support::recall_remote(&uri, "SYNCSMOKE parsing").await;
    let recall_new = support::recall_remote(&uri, "SYNCSMOKE2 continuation").await;
    // Nothing new to push, so this is a pure forced compaction: fold the unindexed appended turn
    // into the index in commits of its own, then recall it again.
    let compaction = funes::commands::push::run_push(Memory::parse(&uri), true, Confirm::Yes, &[]).await;
    let recall_compacted = support::recall_remote(&uri, "SYNCSMOKE2 continuation").await;
    let versions_compacted = match Memory::parse(&uri).open().await {
        Ok(ds) => ds.versions().await.map(|v| v.len()).unwrap_or(0),
        Err(_) => 0,
    };
    // Two pushes at once — what N subagents ending together do.
    support::write_session(
        src.path(),
        "sess",
        &[
            "SYNCSMOKE parsing transcripts into turns",
            "SYNCSMOKE2 the continuation adds only this new turn",
            "SYNCSMOKE3 the turn two pushes race to publish",
        ],
    );
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    let (race_a, race_b) = tokio::join!(
        funes::commands::push::run_push(Memory::parse(&uri), false, Confirm::Yes, &[]),
        funes::commands::push::run_push(Memory::parse(&uri), false, Confirm::Yes, &[]),
    );
    let (remote_rows, remote_ids) = remote_rows_and_ids(&uri).await;
    // A turn indexed after the last push: read as the bound memory, the remote recalls it from the
    // local memory; read as it stands, it does not.
    support::write_session(
        src.path(),
        "sess",
        &[
            "SYNCSMOKE parsing transcripts into turns",
            "SYNCSMOKE2 the continuation adds only this new turn",
            "SYNCSMOKE3 the turn two pushes race to publish",
            "SYNCSMOKE4 indexed here but not yet pushed",
        ],
    );
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    let recall_pooled = recall_bound(&uri, "SYNCSMOKE4 not yet pushed").await;
    let recall_exact = support::recall_remote(&uri, "SYNCSMOKE4 not yet pushed").await;
    // The session it grew is owed, so the bound memory lists and reads it from the local memory,
    // whole; read as it stands, the remote shows its shorter copy.
    let all_sessions = || recall::SessionFilter {
        repo: None,
        since: None,
        until: None,
        limit: None,
        offset: 0,
    };
    let sessions_pooled = recall::sessions(Target::bound(Memory::parse(&uri)), all_sessions())
        .await
        .unwrap();
    let sessions_exact = recall::sessions(Memory::parse(&uri), all_sessions()).await.unwrap();
    let whole = recall::TurnRange { from: None, to: None };
    let get_pooled = recall::get(Target::bound(Memory::parse(&uri)), "sess".into(), whole)
        .await
        .unwrap();
    let whole = recall::TurnRange { from: None, to: None };
    let get_exact = recall::get(Memory::parse(&uri), "sess".into(), whole).await.unwrap();
    // This host holds a receipt for the remote either way. Only a memory an agent here is bound to
    // is one it pushes to, so only that one reports what is pending.
    let status_bound = recall::status(Target::bound(Memory::parse(&uri))).await.unwrap();
    let status_unbound = recall::status(Memory::parse(&uri)).await.unwrap();

    let readme_after = support::root_readme(&repo).await;
    // The model id must travel with the memory (stamped in the schema metadata, uploaded by push).
    let remote_model = match Memory::parse(&uri).open().await {
        Ok(t) => t.schema().metadata.get("embedding_model").cloned(),
        Err(_) => None,
    };
    let remote_languages = match Memory::parse(&uri).open().await {
        Ok(ds) => funes::memory::dataset::languages(&ds).unwrap(),
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
    // Gate: a memory that already shares chunks with the local index is not prompted.
    assert_eq!(
        prompts_after_append, prompts_before_append,
        "an append to a memory you already share chunks with must not trigger the confirmation"
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
    let compaction = compaction.expect("forced compaction").report;
    assert!(
        compaction.contains("compacted"),
        "a forced compaction should commit an index delta: {compaction}"
    );
    assert!(
        recall_compacted.contains("SYNCSMOKE2"),
        "remote recall should still surface the turn after compaction: {recall_compacted}"
    );
    assert_eq!(versions_compacted, 1, "the compaction deletes the old versions");
    let mut published = 0;
    for (who, r) in [("a", race_a), ("b", race_b)] {
        let report = r.unwrap_or_else(|e| panic!("racing push {who} failed: {e}")).report;
        eprintln!("racing push {who}: {}", report.trim_end());
        published += usize::from(report.contains("pushed"));
        assert!(
            report.contains("pushed") || report.contains("skipped") || report.contains("up to date"),
            "racing push {who} reported: {report}"
        );
    }
    assert_eq!(published, 1, "one racer publishes the new chunk, the other stands down");
    assert_eq!(
        remote_rows, remote_ids,
        "two pushes racing must not leave duplicate rows on the remote"
    );
    let unpushed_hit = recall_pooled
        .split("---")
        .find(|hit| hit.contains("SYNCSMOKE4"))
        .unwrap_or_else(|| panic!("pooling should surface the unpushed turn: {recall_pooled}"));
    assert!(
        !unpushed_hit.contains("--memory"),
        "a read naming no memory hints none, so its drill-down reads the same way: {unpushed_hit}"
    );
    assert!(
        recall_exact.contains(&format!("--memory {uri}")),
        "a read naming the memory hints it: {recall_exact}"
    );
    assert_eq!(
        recall_pooled.matches("SYNCSMOKE3 the turn").count(),
        1,
        "a pushed turn should come back once, not once per memory: {recall_pooled}"
    );
    assert!(
        !recall_exact.contains("SYNCSMOKE4"),
        "an exact read of the remote must not see the unpushed turn: {recall_exact}"
    );
    assert!(
        sessions_pooled.contains(" 4 turns sess") && !sessions_pooled.contains(" 3 turns sess"),
        "the owed session should be listed once, from the local memory: {sessions_pooled}"
    );
    assert!(
        sessions_exact.contains(" 3 turns sess"),
        "an exact listing should show the remote's copy: {sessions_exact}"
    );
    assert!(
        get_pooled.contains("SYNCSMOKE4"),
        "the owed session should be read from the local memory: {get_pooled}"
    );
    assert!(
        !get_exact.contains("SYNCSMOKE4"),
        "an exact read of the remote must not see the unpushed turn: {get_exact}"
    );
    assert!(
        status_bound.contains(&format!("local push: 1 of 1 session pending — run `funes push {uri}`")),
        "a bound memory should report the owed session: {status_bound}"
    );
    assert!(
        !status_unbound.contains("local push"),
        "a memory no agent here is bound to should report no push coverage: {status_unbound}"
    );
    assert_eq!(
        remote_model.as_deref(),
        Some(funes::memory::dataset::model::ENGLISH.id()),
        "the model id should travel with the memory via push"
    );
    assert_eq!(
        remote_languages.map(|languages| languages.rows),
        Some(remote_rows as u64),
        "every push, the compaction and the race should leave the language counts covering every row"
    );
    assert_eq!(
        readme_before, readme_after,
        "a prefixed push must never touch the repo-root README (the card is root-stores-only)"
    );
}
