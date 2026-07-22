//! Gated live e2e for project-memory gating: a project memory ships only the sessions this host
//! has marked `include`. Owns a whole throwaway repo (created and deleted here): naming its
//! project creates the dataset empty; a push before any decision holds everything back; recording
//! one `include` and pushing again ships exactly that session's chunks, and no others.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set (it provides `HF_TOKEN`) AND `trufflehog` is on
//! PATH (push's pre-publish gate is fail-closed) — and skipped with a note if the token cannot
//! create a scratch repo under the test org. Needs a bigger thread stack than the default. To
//! run:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>
//!   RUST_MIN_STACK=16777216 cargo test --test curate_gating -- --nocapture

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::{Array, StringArray};
use funes::hub::Memory;
use funes::push::Confirm;
use hf_hub::{HFClient, RepoTypeDataset};

const OWNER: &str = "optimum-internal-testing";

/// Write `projects/<proj>/<stem>.jsonl` (one session, keyed by the file stem) with the given
/// (uuid, text) user turns.
fn write_session(source: &Path, stem: &str, turns: &[(&str, &str)]) {
    let dir = source.join("projects").join("-curatetest-proj");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join(format!("{stem}.jsonl"))).unwrap();
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

/// The distinct `session_id`s stored on the remote dataset at `uri`.
async fn remote_sessions(uri: &str) -> HashSet<String> {
    let ds = Memory::parse(uri).open().await.expect("opening the remote dataset");
    let batches = funes::dataset::scan_rows(&ds, &["session_id"], None, None)
        .await
        .expect("scanning the remote dataset");
    let mut set = HashSet::new();
    for batch in batches {
        let col = batch
            .column_by_name("session_id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .expect("a session_id column");
        for i in 0..batch.num_rows() {
            set.insert(col.value(i).to_string());
        }
    }
    set
}

#[tokio::test]
async fn a_project_memory_ships_only_included_sessions() {
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

    // A throwaway memory: unique repo name so concurrent/repeated runs don't collide.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("funes-test-curate-{}-{nanos}", std::process::id());
    if let Err(e) = funes::hub::create_dataset_repo(OWNER, &name).await {
        eprintln!("skip: cannot create a scratch repo under {OWNER}: {e}");
        return;
    }
    let uri = format!("hf://datasets/{OWNER}/{name}");
    let project = format!("{OWNER}/curate-project");

    // Synthetic local memory: two sessions, one to include and one to hold back.
    let db_dir = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    write_session(src.path(), "keep", &[("k1", "KEEPME a decision worth publishing")]);
    write_session(src.path(), "hold", &[("h1", "HOLDME a note that stays local")]);
    funes::index::run_index(src.path(), false, None).await.unwrap();

    // Name the memory the project memory — this creates the dataset empty. (In the CLI this is the
    // interactive review's deferred, consented step; here we call it directly.)
    let named = funes::curate::name_project(&Memory::parse(&uri), &project).await;

    // Push before any decision: everything is held back, nothing ships.
    let push_ungated = funes::push::run_push(Memory::parse(&uri), false, Confirm::Yes).await;
    let sessions_before = remote_sessions(&uri).await;

    // Record an `include` decision for exactly one session, then push again.
    let recorded = funes::curate::run(&Memory::parse(&uri), None, &["keep".to_string()], &[], None).await;
    let push_gated = funes::push::run_push(Memory::parse(&uri), false, Confirm::Yes).await;
    let sessions_after = remote_sessions(&uri).await;

    // Cleanup before asserting, so a failed assertion can't leave the scratch repo behind.
    let _ = client
        .delete_repository()
        .repo_id(format!("{OWNER}/{name}"))
        .repo_type(RepoTypeDataset)
        .send()
        .await;

    assert!(
        matches!(named.expect("name the project"), funes::curate::Named::Created),
        "naming a fresh memory should create the empty project memory"
    );

    let push_ungated = push_ungated.expect("ungated push").report;
    assert!(
        push_ungated.contains("nothing published"),
        "a push with no decisions must ship nothing: {push_ungated}"
    );
    assert!(
        sessions_before.is_empty(),
        "the remote must be empty before any decision: {sessions_before:?}"
    );

    let recorded = recorded.expect("record decision");
    assert!(
        recorded.contains("recorded 1 include"),
        "one include should be recorded: {recorded}"
    );

    let push_gated = push_gated.expect("gated push").report;
    assert!(
        push_gated.contains("pushed"),
        "the gated push should publish the included session: {push_gated}"
    );
    assert_eq!(
        sessions_after,
        HashSet::from(["keep".to_string()]),
        "only the included session's chunks may reach the remote"
    );
}
