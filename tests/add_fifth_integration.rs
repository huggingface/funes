//! A fifth integration, present only under `~/.funes/agents` with nothing to refresh it from — the
//! drop-in path docs/add.md promises. `funes add` resolves it without a checkout, a published copy
//! or the network, and still stops at every check: an invalid manifest before anything runs, and
//! the trust confirmation before its setup. Confirmed at a terminal — `expect` gives funes a pty
//! and answers its prompts — it adds and removes. Drives the binary; each run sets its own `$HOME`.

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
/// environment in `$FUNES_TEST_SETUP_LOG`.
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
        "#!/bin/sh\nprintf '%s\\n' \"$*\" \"FUNES_BIN=$FUNES_BIN\" \"FUNES_HOME=$FUNES_HOME\" \
         \"FUNES_AGENT_ID=$FUNES_AGENT_ID\" >> \"$FUNES_TEST_SETUP_LOG\"\n",
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
