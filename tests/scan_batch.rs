//! A directory of many small turns files is scanned for secrets a batch at a time: the scanner is
//! spawned once per batch, not once per file, by `index --check` and by the index itself, and not
//! at all by a rerun that owes nothing. Own test binary: it sets `$FUNES_HOME` and
//! `$FUNES_TRUFFLEHOG`.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use funes::memory::dataset;

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

/// Write `line` to `path`, stamped `age` seconds before `now` — a directory lists its files
/// newest first, so the stamp fixes where the unit sits.
fn write_unit(path: &Path, line: &str, now: SystemTime, age: u64) {
    let mut f = std::fs::File::create(path).unwrap();
    writeln!(f, "{line}").unwrap();
    f.set_modified(now - Duration::from_secs(age)).unwrap();
}

#[tokio::test]
async fn a_directory_is_scanned_a_batch_at_a_time() {
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());

    // A scanner that finds nothing and counts its spawns in a file beside itself.
    let scanner = bin.path().join("trufflehog");
    std::fs::write(&scanner, "#!/bin/sh\necho spawn >> \"$0.spawns\"\n").unwrap();
    std::fs::set_permissions(&scanner, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("FUNES_TRUFFLEHOG", &scanner);
    let spawns = bin.path().join("trufflehog.spawns");
    let spawned = || std::fs::read_to_string(&spawns).map_or(0, |s| s.lines().count());

    // More units than one batch holds, each a one-turn session, newest last.
    let now = SystemTime::now();
    let units = 40u64;
    for n in 0..units {
        let session = format!("batch-session-{n:04}");
        let line = serde_json::json!({
            "format": 1,
            "session_id": session,
            "cwd": "/home/u/dev/demo",
            "turn_uuid": "t1",
            "seq": 0,
            "ts": "2026-01-01T00:00:00Z",
            "role": "user",
            "blocks": [{"block_type": "text", "text": format!("note {n} for the record")}],
            "harness": "claude",
        })
        .to_string();
        write_unit(
            &source.path().join(format!("{session}.funes.jsonl")),
            &line,
            now,
            2 * (units - n),
        );
    }
    // A file funes rejects, listed between sessions 21 and 20.
    let bad = source.path().join("batch-session-bad.funes.jsonl");
    write_unit(&bad, "{not a turn", now, 2 * (units - 20) - 1);

    let report = funes::commands::index::check(source.path(), false, None).unwrap();
    assert_eq!(report.rejected, 1, "{}", report.text);
    assert!(
        report
            .text
            .contains(&format!("checked {} unit(s): {units} turns", units + 1)),
        "{}",
        report.text
    );
    let at = |s: &str| {
        report
            .text
            .find(s)
            .unwrap_or_else(|| panic!("missing {s:?} in:\n{}", report.text))
    };
    assert!(
        at("batch-session-0021.funes.jsonl — ") < at("batch-session-bad.funes.jsonl — rejected")
            && at("batch-session-bad.funes.jsonl — rejected") < at("batch-session-0020.funes.jsonl — "),
        "a rejected unit is reported where it sits:\n{}",
        report.text
    );
    assert_eq!(
        spawned(),
        2,
        "41 units scan in two batches, the rejected one costing none"
    );

    // The index scans the same two batches; a rerun that owes nothing scans nothing.
    std::fs::remove_file(&bad).unwrap();
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();
    assert_eq!(spawned(), 4);
    assert_eq!(stored_rows().await, units as usize);
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();
    assert_eq!(spawned(), 4, "an unchanged rerun spawns no scanner");
    assert_eq!(stored_rows().await, units as usize, "and adds no rows");
}
