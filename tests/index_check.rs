//! `funes index --check`: the dry run over turns files reports what an index would do — turns,
//! chunks, rejected files with their first bad line, ids a file produces twice — and writes nothing.
//! Own test binary: it sets `$FUNES_HOME`.

use std::path::{Path, PathBuf};
use std::process::Command;

use funes::memory::dataset;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/funes_jsonl")
        .join(name)
}

/// The rows the local memory holds.
async fn stored_rows() -> usize {
    let ds = dataset::open(&dataset::table_uri(&dataset::local_memory_dir()), Default::default())
        .await
        .expect("the memory exists");
    dataset::scan_rows(&ds, &["id"], None, None)
        .await
        .unwrap()
        .iter()
        .map(|b| b.num_rows())
        .sum()
}

#[tokio::test]
async fn check_reports_without_writing() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());

    let report = funes::commands::index::check(&fixture(""), false, None).unwrap();
    assert!(!report.is_clean());
    assert_eq!((report.rejected, report.duplicate_ids), (2, 1));
    for want in [
        "bad_line.funes.jsonl:2:",
        "unknown format 2",
        "valid.funes.jsonl — 3 turns, 5 chunks",
        "duplicate id",
        "session dup-turn turn t-0001",
        "elided.funes.jsonl — 1 turns, 1 chunks",
        "github_issue.funes.jsonl — 3 turns, 3 chunks",
        "checked 6 unit(s): 9 turns, 11 chunks, 2 rejected, 1 duplicate id(s)",
    ] {
        assert!(report.text.contains(want), "missing {want:?} in:\n{}", report.text);
    }

    let report = funes::commands::index::check(&fixture("valid.funes.jsonl"), false, None).unwrap();
    assert!(report.is_clean(), "{}", report.text);
    assert!(report.text.contains("3 turns, 5 chunks"), "{}", report.text);

    // Nothing was written: the home is as empty as it was created.
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);

    // The CLI prints the report and exits non-zero on a problem.
    let out = Command::new(env!("CARGO_BIN_EXE_funes"))
        .args(["index", "--check"])
        .arg(fixture(""))
        .env("FUNES_HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("2 rejected, 1 duplicate id(s)"), "{stdout}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("2 rejected, 1 duplicate id(s)"));
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);

    // A clean check then an index of the same file agree: the check counts the chunks the run
    // writes. The fixture's data URI splits into three chunks raw and one once elided, so the two
    // only agree if the check elides the way the run does.
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let elided = fixture("elided.funes.jsonl");
    let report = funes::commands::index::check(&elided, false, None).unwrap();
    assert!(report.is_clean(), "{}", report.text);
    assert!(report.text.contains("1 turns, 1 chunks"), "{}", report.text);
    funes::commands::index::run_index(&elided, false, None).await.unwrap();
    assert_eq!(stored_rows().await, 1);
}
