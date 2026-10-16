//! Gated live e2e for the dataset card. The card is root-stores-only, so this test owns a whole
//! throwaway repo (created and deleted here): a first publish must create the card, an append
//! must refresh its stats in the same data commit, and once the README is hand-written funes
//! must keep its hands off it.
//!
//! Skipped unless `HF_FUNES_TEST_TOKEN` is set (it provides `HF_TOKEN`) AND `trufflehog` is on
//! PATH (push's pre-publish gate is fail-closed) — and skipped with a note if the token cannot
//! create a scratch repo under the test org. Needs a bigger thread stack than the default. To
//! run:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>
//!   RUST_MIN_STACK=16777216 cargo test --test push_card -- --nocapture

mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use funes::commands::push::Confirm;
use funes::memory::Memory;
use hf_hub::repository::CommitOperation;
use hf_hub::{HFClient, RepoTypeDataset};

const OWNER: &str = "optimum-internal-testing";

#[tokio::test]
async fn card_created_refreshed_and_a_hand_card_respected() {
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

    // A throwaway ROOT memory: unique repo name so concurrent/repeated runs don't collide.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("funes-test-card-{}-{nanos}", std::process::id());
    if let Err(e) = funes::hub::create_dataset_repo(OWNER, &name).await {
        eprintln!("skip: cannot create a scratch repo under {OWNER}: {e}");
        return;
    }
    let repo = client.dataset(OWNER, name.clone());
    let uri = format!("hf://datasets/{OWNER}/{name}");

    // Synthetic local memory with one turn.
    let db_dir = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());
    support::write_session(src.path(), "sess", &["CARDSMOKE the first turn"]);
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();

    // First publish → the card rides the initial commit.
    let create = funes::commands::push::run_push(Memory::parse(&uri), false, Confirm::Yes, &[]).await;
    let card_created = support::root_readme(&repo).await;

    // Grow by one turn → the append must refresh the stats in the same data commit.
    support::write_session(
        src.path(),
        "sess",
        &["CARDSMOKE the first turn", "CARDSMOKE2 the second turn"],
    );
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    let append = funes::commands::push::run_push(Memory::parse(&uri), false, Confirm::Yes, &[]).await;
    let card_refreshed = support::root_readme(&repo).await;

    // Hand-write the README: from here on funes must keep its hands off.
    let hand = "# hand-written card\n";
    let hand_dir = tempfile::tempdir().unwrap();
    let hand_path = hand_dir.path().join("README.md");
    std::fs::write(&hand_path, hand).unwrap();
    repo.create_commit()
        .operations(vec![CommitOperation::add_file("README.md".to_string(), hand_path)])
        .commit_message("hand-edit the card".to_string())
        .send()
        .await
        .expect("hand-editing the card");

    support::write_session(
        src.path(),
        "sess",
        &[
            "CARDSMOKE the first turn",
            "CARDSMOKE2 the second turn",
            "CARDSMOKE3 the third turn",
        ],
    );
    funes::commands::index::run_index(src.path(), false, None)
        .await
        .unwrap();
    let append_past_hand = funes::commands::push::run_push(Memory::parse(&uri), false, Confirm::Yes, &[]).await;
    let card_after_hand = support::root_readme(&repo).await;

    // Cleanup before asserting, so a failed assertion can't leave the scratch repo behind.
    let _ = client
        .delete_repository()
        .repo_id(format!("{OWNER}/{name}"))
        .repo_type(RepoTypeDataset)
        .send()
        .await;

    let create = create.expect("create push").report;
    assert!(
        create.contains("dataset card created"),
        "first publish should report the card: {create}"
    );
    let card_created = card_created.expect("first publish should create the card");
    assert!(
        card_created.contains("<!-- funes:stats -->"),
        "the card should carry the stats markers: {card_created}"
    );
    assert!(
        card_created.contains("| Chunks | 1 |"),
        "the card should count the pushed chunk: {card_created}"
    );
    assert!(
        card_created.contains(&format!("--memory {OWNER}/{name}")),
        "the recall example should name this memory: {card_created}"
    );

    let append = append.expect("append push").report;
    assert!(
        append.contains("dataset card refreshed"),
        "the append should report the refresh: {append}"
    );
    let card_refreshed = card_refreshed.expect("the refreshed card should still exist");
    assert!(
        card_refreshed.contains("| Chunks | 2 |"),
        "the refresh should update the chunk count: {card_refreshed}"
    );

    let append_past_hand = append_past_hand.expect("append past a hand card").report;
    assert!(
        append_past_hand.contains("pushed 1 chunks"),
        "the data push should proceed: {append_past_hand}"
    );
    assert!(
        !append_past_hand.contains("dataset card"),
        "a hand-written card must not be reported on: {append_past_hand}"
    );
    assert_eq!(
        card_after_hand.as_deref(),
        Some(hand),
        "the hand-written card must survive an append untouched"
    );
}
