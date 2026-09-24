//! A fifth integration, present only under `~/.funes/agents` with nothing to refresh it from — the
//! drop-in path docs/add.md promises. `funes add` resolves it without a checkout, a published copy
//! or the network, and still stops at every check: an invalid manifest before anything runs, and
//! the trust confirmation before its setup. Confirmed at a terminal — `expect` gives funes a pty
//! and answers its prompts — it adds and removes. Drives the binary; each run sets its own `$HOME`.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Runs `funes <args>` on a pty, answering the trust confirmation and the first-index prompt with
/// yes; exits as funes did. Anything else funes asks goes unanswered and times out.
const ANSWER_YES: &str = r#"
set timeout 120
spawn {*}$argv
expect {
    -re {Trust it\? \[y/N\] $} { send "y\r"; exp_continue }
    -re {Proceed\? \[Y/n\] $} { send "y\r"; exp_continue }
    eof
}
lassign [wait] pid spawn_id os_error status
exit $status
"#;

/// An integration under `home`'s registry whose `setup` records its argv and the contract
/// environment in `$FUNES_TEST_SETUP_LOG`, and converts the history it finds at
/// `~/.clyde/history.funes.jsonl` into its spool, as a converter does at install.
fn install(home: &Path, id: &str, contract: u32) -> PathBuf {
    let dir = home.join(".funes/agents").join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("manifest.json"),
        format!(r#"{{"contract_version":{contract},"id":"{id}","label":"Clyde","repo":"example/clyde"}}"#),
    )
    .unwrap();
    let setup = dir.join("setup");
    fs::write(
        &setup,
        r#"#!/bin/sh
printf '%s\n' "$*" "FUNES_BIN=$FUNES_BIN" "FUNES_HOME=$FUNES_HOME" "FUNES_AGENT_ID=$FUNES_AGENT_ID" >> "$FUNES_TEST_SETUP_LOG"
if [ "$1" = add ] && [ -f "$HOME/.clyde/history.funes.jsonl" ]; then
    mkdir -p "$FUNES_HOME/spool/$FUNES_AGENT_ID"
    cp "$HOME/.clyde/history.funes.jsonl" "$FUNES_HOME/spool/$FUNES_AGENT_ID/h-1.funes.jsonl"
fi
"#,
    )
    .unwrap();
    fs::set_permissions(&setup, fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// `funes <args>` against `home`, off a terminal.
fn funes(home: &Path, funes_home: &Path, log: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_funes"));
    cmd.args(args);
    run(cmd, home, funes_home, log)
}

/// `funes <args>` against `home`, at a terminal that says yes to what funes asks.
fn funes_at_a_terminal(home: &Path, funes_home: &Path, log: &Path, args: &[&str]) -> Output {
    let script = home.join("answer-yes.exp");
    fs::write(&script, ANSWER_YES).unwrap();
    let mut cmd = Command::new("expect");
    cmd.arg("-f")
        .arg(&script)
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_funes"))
        .args(args);
    run(cmd, home, funes_home, log)
}

fn run(mut cmd: Command, home: &Path, funes_home: &Path, log: &Path) -> Output {
    cmd.env("HOME", home)
        .env("FUNES_HOME", funes_home)
        // Authoritative and holding nothing: no checkout or bucket is consulted for clyde.
        .env("FUNES_INTEGRATIONS", home.join("no-integrations"))
        .env("FUNES_TEST_SETUP_LOG", log)
        .env("HF_HOME", support::hf_home())
        .env_remove("HF_TOKEN")
        .env_remove("HUGGING_FACE_HUB_TOKEN")
        .env_remove("HUGGINGFACE_TOKEN")
        .env_remove("FUNES_BIN")
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// One turn of clyde's history, under a facet of the integration's own choosing.
const HISTORY: &str = r#"{"format":1,"session_id":"h-1","turn_uuid":"t0","seq":0,"ts":"2026-03-01T00:00:00Z","role":"user","harness":"clydebot","blocks":[{"block_type":"text","text":"the widget cache must be invalidated on every deploy: stale entries broke checkout twice"}]}"#;

#[test]
fn a_fifth_integration_seeds_and_drains_its_spool_under_its_own_facet() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    install(&home, "clyde", 1);
    fs::create_dir_all(home.join(".clyde")).unwrap();
    fs::write(home.join(".clyde/history.funes.jsonl"), format!("{HISTORY}\n")).unwrap();

    // The seed finds the spool by the id funes handed setup, indexes it, and drains it.
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde"]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{transcript}");
    assert!(
        transcript.contains("indexing your recent clyde sessions"),
        "{transcript}"
    );
    assert!(
        !funes_home.join("spool/clyde/h-1.funes.jsonl").exists(),
        "the seed drained the spool"
    );

    // The facet is the turns', not the spool's: recall filters on what clyde wrote.
    let recall = |harness: &str| -> String {
        let out = funes(
            &home,
            &funes_home,
            &log,
            &["recall", "--harness", harness, "widget cache invalidated on deploy"],
        );
        format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out))
    };
    let hits = recall("clydebot");
    assert!(hits.contains("clydebot") && hits.contains("h-1"), "{hits}");
    assert!(!recall("clyde").contains("h-1"), "the spool's name is not a facet");

    // `--harness <id>` selects the spool with no list of agents to consult; a name no producer
    // created is refused, not swept.
    support::assert_success(&funes(&home, &funes_home, &log, &["index", "--harness", "clyde"]));
    let out = funes(&home, &funes_home, &log, &["index", "--harness", "nope"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no nope spool"), "{}", stderr(&out));
}

#[test]
fn an_installed_only_integration_adds_and_removes_once_trusted_at_a_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    let dir = install(&home, "clyde", 1);
    let memory = tmp.path().join("team-memory");

    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde", memory.to_str().unwrap()]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{transcript}");
    assert!(transcript.contains("Trust it? [y/N]"), "{transcript}");
    // Setup ran with the verb, the memory, and the contract environment.
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!(
            "add {}\nFUNES_BIN=funes\nFUNES_HOME={}\nFUNES_AGENT_ID=clyde\n",
            memory.display(),
            funes_home.display()
        )
    );
    // Clyde wrote no spool, so there was nothing to seed or publish — noted, not failed.
    assert!(transcript.contains("no clyde sessions to index yet"), "{transcript}");
    assert!(transcript.contains("nothing indexed yet"), "{transcript}");

    fs::remove_file(&log).unwrap();
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["remove", "clyde"]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{transcript}");
    assert!(transcript.contains("Trust it? [y/N]"), "{transcript}");
    assert!(fs::read_to_string(&log).unwrap().starts_with("remove\n"));
    assert!(!dir.exists(), "the installed copy is taken with the integration");
}

#[test]
fn an_installed_only_integration_reaches_the_trust_confirmation() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    let dir = install(&home, "clyde", 1);

    // Resolved from its installed copy, which funes can't vouch for, so off a terminal the run
    // stops at the confirmation — before setup and before any memory bootstrap.
    let out = funes(&home, &funes_home, &log, &["add", "clyde"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("could not be refreshed"), "{err}");
    assert!(err.contains("the installed copy, refreshed by nothing"), "{err}");
    assert!(err.contains("run this in a terminal to confirm it"), "{err}");
    assert!(!log.exists(), "setup did not run");
    assert!(!funes_home.exists(), "no memory bootstrap started");
    assert!(dir.join("setup").exists(), "the installed copy is kept");

    // `remove` resolves the same way.
    let out = funes(&home, &funes_home, &log, &["remove", "clyde"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("the installed copy, refreshed by nothing"), "{err}");
    assert!(!log.exists(), "setup did not run");
    assert!(dir.exists(), "an unconfirmed remove deletes nothing");
}

#[test]
fn an_invalid_installed_integration_fails_before_anything_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    install(&home, "clyde", 2);

    let out = funes(&home, &funes_home, &log, &["add", "clyde"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("contract version 2"), "{err}");
    assert!(!err.contains("Trust it?"), "refused before the confirmation: {err}");
    assert!(!log.exists(), "setup did not run");
    assert!(!funes_home.exists(), "no memory bootstrap started");
}
