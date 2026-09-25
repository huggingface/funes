//! A directory of many small turns files is scanned for secrets a batch at a time: the scanner is
//! spawned once per batch, not once per file, by `index --check` and by the index itself. Own test
//! binary: it sets `$FUNES_HOME` and `$FUNES_TRUFFLEHOG`.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn a_directory_is_scanned_a_batch_at_a_time() {
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());

    // A scanner that finds nothing and counts its spawns.
    let spawns = bin.path().join("spawns");
    let scanner = bin.path().join("trufflehog");
    std::fs::write(&scanner, format!("#!/bin/sh\necho spawn >> {}\n", spawns.display())).unwrap();
    std::fs::set_permissions(&scanner, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("FUNES_TRUFFLEHOG", &scanner);
    let spawned = || std::fs::read_to_string(&spawns).map_or(0, |s| s.lines().count());

    // More units than one batch holds, each a one-turn session.
    let units = 40;
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
        let mut f = std::fs::File::create(source.path().join(format!("{session}.funes.jsonl"))).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    let report = funes::commands::index::check(source.path(), false, None).unwrap();
    assert!(report.all_accepted(), "{}", report.text);
    assert!(
        report.text.contains(&format!("checked {units} unit(s): {units} turns")),
        "{}",
        report.text
    );
    assert_eq!(spawned(), 2, "40 units scan in two batches, not 40 spawns");

    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();
    assert_eq!(spawned(), 4, "the index scans the same two batches");
}
