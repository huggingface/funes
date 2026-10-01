//! `since`/`until` on a recall keep the turns of those days, both ends inclusive, before the
//! ranking. Own test binary: it sets `$FUNES_HOME`.

use std::path::{Path, PathBuf};

use funes::commands::recall::{self, RecallFilter};
use funes::memory::Memory;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/funes_jsonl")
        .join(name)
}

/// The distinct days the hits of one recall fall on.
async fn days(filter: RecallFilter) -> Vec<String> {
    let (_, hits) = recall::recall_hits(Memory::local(), "static cache build".into(), 50, 50, 0, filter, &|_| ())
        .await
        .unwrap();
    let mut days: Vec<String> = hits.iter().map(|(h, _)| h.ts[..10].to_string()).collect();
    days.sort();
    days.dedup();
    days
}

fn on(since: Option<&str>, until: Option<&str>) -> RecallFilter {
    RecallFilter {
        since: since.map(str::to_string),
        until: until.map(str::to_string),
        ..Default::default()
    }
}

#[tokio::test]
async fn since_and_until_keep_the_turns_of_those_days() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    // An issue thread of 2026-06-02 and 2026-06-03, and an agent session of 2026-09-18.
    funes::commands::index::run_index(&fixture("github_issue.funes.jsonl"), false, None)
        .await
        .unwrap();
    funes::commands::index::run_index(&fixture("valid.funes.jsonl"), false, None)
        .await
        .unwrap();

    assert_eq!(days(on(None, None)).await, ["2026-06-02", "2026-06-03", "2026-09-18"]);
    assert_eq!(days(on(Some("2026-09-01"), None)).await, ["2026-09-18"]);
    assert_eq!(days(on(None, Some("2026-06-30"))).await, ["2026-06-02", "2026-06-03"]);
    // Both ends are inclusive: one day is a window.
    assert_eq!(days(on(Some("2026-06-03"), Some("2026-06-03"))).await, ["2026-06-03"]);
    // The date bounds combine with the other filters.
    let filter = RecallFilter {
        block_type: Some("text".into()),
        ..on(Some("2026-06-03"), None)
    };
    assert_eq!(days(filter).await, ["2026-06-03", "2026-09-18"]);
}
