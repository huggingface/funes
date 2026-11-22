//! An integration funes installed and recorded runs again with nothing asked of a terminal: `funes
//! add` hands its `setup` the contract's arguments and environment, indexes what it converted into
//! its spool, and `funes remove` runs it once more and takes its files. No pty, so this is the run
//! that reaches a `setup` on every platform — on Windows through the shell, where
//! `tests/add_fifth_integration.rs`, which answers the prompts, cannot go. Drives the binary; each
//! run has its own home.

mod support;

use funes::agents::registry::{self, Files, Installed, Manifest, Origin};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// What the agent said before funes was added to it, as its integration converts it.
const HISTORY: &str = r#"{"format":1,"session_id":"h-1","cwd":"/home/u/dev/demo","turn_uuid":"t1","seq":0,"ts":"2026-01-01T00:00:00Z","role":"user","blocks":[{"block_type":"text","text":"the clyde integration converts what was said before it was added"}],"harness":"clyde"}
"#;

/// Records its arguments and the id it was handed, and on `add` converts the history beside it
/// into the spool funes named.
const SETUP: &str = r#"#!/bin/sh
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$*" "FUNES_AGENT_ID=$FUNES_AGENT_ID" >>"$FUNES_TEST_SETUP_LOG"
if [ "$1" = add ]; then
    mkdir -p "$FUNES_HOME/spool/$FUNES_AGENT_ID"
    cp "$here/history.funes.jsonl" "$FUNES_HOME/spool/$FUNES_AGENT_ID/h-1.funes.jsonl"
fi
"#;

/// Close `path` to other writers, as funes leaves what it installs: a registry others could write
/// to is refused.
fn close(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(windows)]
    let _ = (path, mode);
}

/// `funes <args>` off a terminal, at `home`, answering `answers` on its stdin.
fn funes(home: &Path, funes_home: &Path, log: &Path, args: &[&str], answers: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_funes"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("FUNES_HOME", funes_home)
        .env("FUNES_TEST_SETUP_LOG", log)
        .env("HF_HOME", support::hf_home())
        .env("HF_HUB_DISABLE_IMPLICIT_TOKEN", "1")
        .env("LANG", "en_US.UTF-8")
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env_remove("FUNES_BIN")
        .env_remove("FUNES_INTEGRATIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(answers.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn an_installed_integration_is_added_and_removed_off_a_terminal() {
    // A home whose name a shell would split, were it handed over unquoted.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("the home");
    let funes_home = tmp.path().join("funes home");
    let log = tmp.path().join("setup.log");
    let root = home.join(".funes").join("agents");
    let dir = root.join("clyde");
    fs::create_dir_all(&dir).unwrap();
    for made in [&home, &home.join(".funes"), &root, &dir] {
        close(made, 0o755);
    }

    // The package, and the record funes writes of it once its source was confirmed at a terminal.
    let manifest = r#"{"contract_version":1,"id":"clyde","label":"Clyde","repo":"example/clyde","version":"1.0.0"}"#;
    let mut files = Files::new();
    for (name, text, mode) in [
        ("manifest.json", manifest, 0o644),
        ("setup", SETUP, 0o755),
        ("history.funes.jsonl", HISTORY, 0o644),
    ] {
        fs::write(dir.join(name), text).unwrap();
        close(&dir.join(name), mode);
        files.insert(name.to_string(), hex::encode(Sha256::digest(text.as_bytes())));
    }
    let declared: Manifest = serde_json::from_str(manifest).unwrap();
    let origin = Origin::Directory {
        path: tmp.path().join("source"),
    };
    registry::record(&root, &Installed::new(&declared, origin, files)).unwrap();

    // Added: nothing to confirm, the first index agreed to on stdin.
    let out = funes(&home, &funes_home, &log, &["add", "clyde"], "y\n");
    support::assert_success(&out);
    assert_eq!(fs::read_to_string(&log).unwrap(), "add\nFUNES_AGENT_ID=clyde\n");

    // What setup converted into the spool funes named is indexed, and drained.
    let out = funes(&home, &funes_home, &log, &["recall", "what was said before"], "");
    support::assert_success(&out);
    let reply = String::from_utf8_lossy(&out.stdout);
    assert!(reply.contains("converts what was said before it was added"), "{reply}");
    let spool = funes_home.join("spool").join("clyde");
    assert!(spool.is_dir(), "the spool is where funes reads it");
    assert_eq!(fs::read_dir(&spool).unwrap().count(), 0, "indexed files are drained");

    // Removed: setup runs once more, and its files go with the record.
    let out = funes(&home, &funes_home, &log, &["remove", "clyde"], "");
    support::assert_success(&out);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "add\nFUNES_AGENT_ID=clyde\nremove\nFUNES_AGENT_ID=clyde\n"
    );
    assert!(!dir.exists() && !root.join("clyde.json").exists());
}
