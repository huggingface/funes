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

/// The same, declining the first index.
const DECLINE_INDEX: &str = r#"
set timeout 120
spawn {*}$argv
expect {
    -re {Trust it\? \[y/N\] $} { send "y\r"; exp_continue }
    -re {Proceed\? \[Y/n\] $} { send "n\r"; exp_continue }
    eof
}
lassign [wait] pid spawn_id os_error status
exit $status
"#;

/// An integration under `home`'s registry, as a drop-in places it.
fn install(home: &Path, id: &str, contract: u32) -> PathBuf {
    bundle(&home.join(".funes/agents").join(id), id, contract, "")
}

/// A bundle at `dir` whose `setup` records `tag` when given, then its argv and the contract
/// environment, in `$FUNES_TEST_SETUP_LOG`; keeps a `state` file beside itself; and converts the
/// history it finds at `~/.clyde/history.funes.jsonl` into its spool, as a converter does at install.
fn bundle(dir: &Path, id: &str, contract: u32, tag: &str) -> PathBuf {
    bundle_published_by(dir, id, "example/clyde", contract, tag)
}

/// [`bundle`], declaring `repo` as where it is published from.
fn bundle_published_by(dir: &Path, id: &str, repo: &str, contract: u32, tag: &str) -> PathBuf {
    let dir = dir.to_path_buf();
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("manifest.json"),
        format!(r#"{{"contract_version":{contract},"id":"{id}","label":"Clyde","repo":"{repo}","version":"1.0.0"}}"#),
    )
    .unwrap();
    let setup = dir.join("setup");
    let tagline = if tag.is_empty() {
        String::new()
    } else {
        format!("printf '%s\\n' {tag} >> \"$FUNES_TEST_SETUP_LOG\"\n")
    };
    fs::write(
        &setup,
        format!(
            r#"#!/bin/sh
{tagline}printf '%s\n' "$*" "FUNES_BIN=$FUNES_BIN" "FUNES_HOME=$FUNES_HOME" "FUNES_AGENT_ID=$FUNES_AGENT_ID" >> "$FUNES_TEST_SETUP_LOG"
printf kept > "$(dirname "$0")/state"
if [ "$1" = add ] && [ -f "$HOME/.clyde/history.funes.jsonl" ]; then
    mkdir -p "$FUNES_HOME/spool/$FUNES_AGENT_ID"
    cp "$HOME/.clyde/history.funes.jsonl" "$FUNES_HOME/spool/$FUNES_AGENT_ID/h-1.funes.jsonl"
fi
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&setup, fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// `funes <args>` against `home`, off a terminal.
fn funes(home: &Path, funes_home: &Path, log: &Path, args: &[&str]) -> Output {
    let mut argv = vec![env!("CARGO_BIN_EXE_funes")];
    argv.extend(args);
    run(&argv, home, funes_home, log)
}

/// `funes <args>` against `home`, off a terminal, with no `$FUNES_INTEGRATIONS`: only what is
/// installed can answer.
fn funes_pinned(home: &Path, funes_home: &Path, log: &Path, args: &[&str]) -> Output {
    let mut argv = vec![env!("CARGO_BIN_EXE_funes")];
    argv.extend(args);
    run_with(&argv, home, funes_home, log, None, None)
}

/// [`funes_at_a_terminal`], run from `cwd`.
fn funes_at_a_terminal_in(cwd: &Path, home: &Path, funes_home: &Path, log: &Path, args: &[&str]) -> Output {
    let script = home.join("answers.exp");
    fs::write(&script, ANSWER_YES).unwrap();
    let script = script.to_str().unwrap().to_string();
    let mut argv = vec!["expect", "-f", &script, "--", env!("CARGO_BIN_EXE_funes")];
    argv.extend(args);
    run_with(
        &argv,
        home,
        funes_home,
        log,
        Some(&home.join("integrations")),
        Some(cwd),
    )
}

/// `funes <args>` against `home`, at a terminal that says yes to what funes asks.
fn funes_at_a_terminal(home: &Path, funes_home: &Path, log: &Path, args: &[&str]) -> Output {
    funes_at_a_terminal_answering(home, funes_home, log, args, ANSWER_YES)
}

/// `funes <args>` against `home`, at a terminal answering as `answers` says.
fn funes_at_a_terminal_answering(home: &Path, funes_home: &Path, log: &Path, args: &[&str], answers: &str) -> Output {
    let script = home.join("answers.exp");
    fs::write(&script, answers).unwrap();
    let script = script.to_str().unwrap().to_string();
    let mut argv = vec!["expect", "-f", &script, "--", env!("CARGO_BIN_EXE_funes")];
    argv.extend(args);
    run(&argv, home, funes_home, log)
}

/// Runs `argv` under a umask of 002 — what a user-private-group distribution gives a login — so
/// the directories funes creates must be its own doing, not the umask's. `$FUNES_INTEGRATIONS`
/// names `home`'s own directory: authoritative, so only what it holds is consulted, never a
/// checkout or the bucket, and it is empty unless a test supplies a bundle there.
fn run(argv: &[&str], home: &Path, funes_home: &Path, log: &Path) -> Output {
    run_with(argv, home, funes_home, log, Some(&home.join("integrations")), None)
}

fn run_with(
    argv: &[&str],
    home: &Path,
    funes_home: &Path,
    log: &Path,
    integrations: Option<&Path>,
    cwd: Option<&Path>,
) -> Output {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", r#"umask 002; exec "$@""#, "sh"]).args(argv);
    match integrations {
        Some(dir) => cmd.env("FUNES_INTEGRATIONS", dir),
        None => cmd.env_remove("FUNES_INTEGRATIONS"),
    };
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.env("HOME", home)
        .env("FUNES_HOME", funes_home)
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

#[test]
fn an_integration_supplied_outside_the_checkout_is_refreshed_each_run_and_removed_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    let source = bundle(&home.join("integrations/clyde"), "clyde", 1, "v1");
    let installed = home.join(".funes/agents/clyde");

    // Provisioned from `$FUNES_INTEGRATIONS`, confirmed as such, and run from the installed copy.
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde"]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{transcript}");
    assert!(transcript.contains("$FUNES_INTEGRATIONS"), "{transcript}");
    assert!(fs::read_to_string(&log).unwrap().starts_with("v1\nadd\n"));
    assert_eq!(fs::read_to_string(installed.join("state")).unwrap(), "kept");

    // A changed source is what the next run executes; what setup keeps beside itself survives.
    bundle(&source, "clyde", 1, "v2");
    fs::remove_file(&log).unwrap();
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(fs::read_to_string(&log).unwrap().starts_with("v2\nadd\n"));
    assert!(installed.join("state").exists(), "a refresh prunes nothing");

    // Removal takes the installed directory whole and leaves the source alone.
    fs::remove_file(&log).unwrap();
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["remove", "clyde"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(fs::read_to_string(&log).unwrap().starts_with("v2\nremove\n"));
    assert!(!installed.exists());
    assert!(source.join("setup").exists(), "the source is not funes's to touch");
}

/// What `add` installed is recorded beside the directory; another publisher's files are refused
/// where it sits, and `remove` — which runs the installed copy's setup, not theirs — is how the
/// user says they mean it.
#[test]
fn another_publishers_integration_replaces_an_installed_one_only_after_its_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    let source = bundle(&home.join("integrations/clyde"), "clyde", 1, "v1");
    let installed = home.join(".funes/agents/clyde");
    let record = home.join(".funes/agents/clyde.json");
    let recorded = || -> serde_json::Value { serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap() };

    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let first = recorded();
    assert_eq!(first["contract_version"], 1);
    assert_eq!(first["id"], "clyde");
    assert_eq!(first["repo"], "example/clyde");
    assert_eq!(first["version"], "1.0.0");
    assert_eq!(first["origin"]["kind"], "directory");
    assert_eq!(first["origin"]["path"], source.to_str().unwrap());
    assert!(
        first["files"]["setup"].is_string() && first["files"]["manifest.json"].is_string(),
        "{first}"
    );
    assert!(first["installed_at"].is_string(), "{first}");

    // Another publisher's clyde at the same source: refused before setup, before a byte moves.
    bundle_published_by(&source, "clyde", "other/clyde", 1, "v2");
    fs::remove_file(&log).unwrap();
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde"]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{transcript}");
    assert!(
        transcript.contains("example's, from") && transcript.contains("`funes remove clyde` first"),
        "{transcript}"
    );
    assert!(!log.exists(), "setup did not run");
    assert!(
        fs::read_to_string(installed.join("manifest.json"))
            .unwrap()
            .contains("example/clyde"),
        "the installed files are untouched"
    );
    assert_eq!(recorded(), first);

    // `remove` runs the installed copy's setup, not the other publisher's — unasked, since the
    // copy is as funes installed and confirmed it — and takes the record.
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["remove", "clyde"]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{transcript}");
    assert!(!transcript.contains("Trust it?"), "{transcript}");
    assert!(fs::read_to_string(&log).unwrap().starts_with("v1\nremove\n"));
    assert!(!installed.exists() && !record.exists());

    // Now theirs installs, and is recorded as theirs.
    fs::remove_file(&log).unwrap();
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(fs::read_to_string(&log).unwrap().starts_with("v2\nadd\n"));
    assert_eq!(recorded()["repo"], "other/clyde");
}

/// A directory named on the command line is where the integration comes from, over
/// `$FUNES_INTEGRATIONS`, and the confirmation says what is about to run: the package by publisher
/// and version, and its source.
#[test]
fn an_integration_installs_from_a_directory_named_on_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    fs::create_dir_all(&home).unwrap();
    let elsewhere = bundle(&tmp.path().join("elsewhere/clyde"), "clyde", 1, "named");
    let from = elsewhere.to_str().unwrap();

    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde", "--from", from]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{transcript}");
    assert!(
        transcript.contains(&format!(
            "clyde 1.0.0 by example (example/clyde), from {from}. Trust it?"
        )),
        "{transcript}"
    );
    assert!(fs::read_to_string(&log).unwrap().starts_with("named\nadd\n"));
    let recorded: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".funes/agents/clyde.json")).unwrap()).unwrap();
    assert_eq!(recorded["origin"]["path"], from);

    // Another directory's files, off a terminal: refused, naming the same.
    let other = bundle(&tmp.path().join("other/clyde"), "clyde", 1, "other");
    fs::remove_file(&log).unwrap();
    let out = funes(
        &home,
        &funes_home,
        &log,
        &["add", "clyde", "--from", other.to_str().unwrap()],
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("clyde 1.0.0 by example (example/clyde) — comes from"),
        "{err}"
    );
    assert!(!log.exists(), "setup did not run");

    // Neither a directory nor an archive URL is an error before anything runs.
    let out = funes(&home, &funes_home, &log, &["add", "clyde", "--from", "/nowhere/clyde"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("is not a directory, nor an hf://buckets/… archive URL"),
        "{}",
        stderr(&out)
    );
}

/// A directory named relative to where the command ran is recorded absolute: what an update
/// follows later does not depend on where it runs then.
#[test]
fn a_relative_source_is_recorded_absolute() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    fs::create_dir_all(&home).unwrap();
    let elsewhere = bundle(&tmp.path().join("elsewhere/clyde"), "clyde", 1, "");

    let out = funes_at_a_terminal_in(
        tmp.path(),
        &home,
        &funes_home,
        &log,
        &["add", "clyde", "--from", "elsewhere/clyde"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let recorded: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".funes/agents/clyde.json")).unwrap()).unwrap();
    let path = PathBuf::from(recorded["origin"]["path"].as_str().unwrap());
    assert!(path.is_absolute(), "{}", path.display());
    assert_eq!(path.canonicalize().unwrap(), elsewhere.canonicalize().unwrap());
}

/// Setup ran, so the install is recorded — even when a later step of the bootstrap fails: the
/// files it installed are what the agent runs, and an unattended removal must find them recorded.
#[test]
fn a_failed_first_push_still_records_the_install() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    bundle(&home.join("integrations/clyde"), "clyde", 1, "v1");
    fs::create_dir_all(home.join(".clyde")).unwrap();
    fs::write(home.join(".clyde/history.funes.jsonl"), format!("{HISTORY}\n")).unwrap();
    let record = home.join(".funes/agents/clyde.json");

    // A path is no push target: the first push fails, after setup and the seed.
    let memory = tmp.path().join("team-memory");
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde", memory.to_str().unwrap()]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{transcript}");
    assert!(transcript.contains("push target must be a remote"), "{transcript}");
    assert!(fs::read_to_string(&log).unwrap().starts_with("v1\nadd"), "setup ran");
    assert!(record.exists(), "recorded all the same");

    // Recorded and intact, it removes unattended.
    fs::remove_file(&log).unwrap();
    let out = funes_pinned(&home, &funes_home, &log, &["remove", "clyde"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(fs::read_to_string(&log).unwrap().starts_with("v1\nremove\n"));
    assert!(!record.exists());
}

/// Installed, an integration runs as installed: nothing is fetched to rebind or remove it, and a
/// copy confirmed once runs unasked until one of its files changes — or its files are named again.
#[test]
fn an_installed_integration_runs_as_installed_and_unasked_until_it_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    let source = bundle(&home.join("integrations/clyde"), "clyde", 1, "v1");
    let installed = home.join(".funes/agents/clyde");
    let record = home.join(".funes/agents/clyde.json");
    // A history to seed, so the adds after the first are not first adds and run unattended.
    fs::create_dir_all(home.join(".clyde")).unwrap();
    fs::write(home.join(".clyde/history.funes.jsonl"), format!("{HISTORY}\n")).unwrap();

    // Installed from `$FUNES_INTEGRATIONS`, confirmed once.
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde"]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && transcript.contains("Trust it?"), "{transcript}");
    assert!(record.exists());

    // With no source in sight, the installed copy runs — unasked, off a terminal.
    fs::remove_file(&log).unwrap();
    let out = funes_pinned(&home, &funes_home, &log, &["add", "clyde"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(fs::read_to_string(&log).unwrap().starts_with("v1\nadd\n"));

    // A newer source is not fetched for it…
    bundle(&source, "clyde", 1, "v2");
    fs::remove_file(&log).unwrap();
    let out = funes_pinned(&home, &funes_home, &log, &["add", "clyde"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(fs::read_to_string(&log).unwrap().starts_with("v1\nadd\n"), "pinned");

    // …until its files are named again, when the changed ones are confirmed anew.
    fs::remove_file(&log).unwrap();
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde", "--update"]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && transcript.contains("Trust it?"), "{transcript}");
    assert!(fs::read_to_string(&log).unwrap().starts_with("v2\nadd\n"));

    // The same source again, unchanged: confirmed once is enough, even off a terminal.
    fs::remove_file(&log).unwrap();
    let out = funes(&home, &funes_home, &log, &["add", "clyde"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(fs::read_to_string(&log).unwrap().starts_with("v2\nadd\n"));

    // A file edited in place is not what funes installed: asked again, refused off a terminal.
    let setup = installed.join("setup");
    let mut edited = fs::read_to_string(&setup).unwrap();
    edited.push_str("# edited by hand\n");
    fs::write(&setup, edited).unwrap();
    fs::remove_file(&log).unwrap();
    let out = funes_pinned(&home, &funes_home, &log, &["add", "clyde"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("whose setup changed since funes installed it"), "{err}");
    assert!(err.contains("run this in a terminal"), "{err}");
    assert!(!log.exists(), "setup did not run");

    // Removal runs the installed copy, fetching nothing — the edited one once confirmed.
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["remove", "clyde"]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && transcript.contains("Trust it?"), "{transcript}");
    assert!(fs::read_to_string(&log).unwrap().starts_with("v2\nremove\n"));
    assert!(!installed.exists() && !record.exists());
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
    // The wording is stale_install_notice's to pin.
    assert!(
        stderr(&out).contains("the nope integration does not match"),
        "{}",
        stderr(&out)
    );
}

/// Declining the first index at its prompt installs nothing: setup never runs, so nothing is
/// converted, and nothing is indexed — and the manifest of the install already there is put
/// back, since that install is still what the agent runs.
#[test]
fn declining_the_first_index_installs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    bundle(&home.join("integrations/clyde"), "clyde", 1, "");
    fs::create_dir_all(home.join(".clyde")).unwrap();
    fs::write(home.join(".clyde/history.funes.jsonl"), format!("{HISTORY}\n")).unwrap();
    let installed = install(&home, "clyde", 1);
    let previous = r#"{"contract_version":1,"id":"clyde","label":"Clyde as installed","repo":"example/clyde"}"#;
    fs::write(installed.join("manifest.json"), previous).unwrap();

    let out = funes_at_a_terminal_answering(&home, &funes_home, &log, &["add", "clyde"], DECLINE_INDEX);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{transcript}");
    assert!(transcript.contains("nothing was wired up"), "{transcript}");
    assert!(!log.exists(), "setup did not run");
    assert!(!funes_home.join("spool").exists(), "nothing converted");
    assert!(!funes_home.join("memory").exists(), "nothing indexed");
    assert_eq!(
        fs::read_to_string(installed.join("manifest.json")).unwrap(),
        previous,
        "the manifest says what setup last installed"
    );
    assert!(!home.join(".funes/agents/clyde.json").exists(), "nothing recorded");
}

#[test]
fn an_installed_only_integration_adds_and_removes_once_trusted_at_a_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("setup.log");
    let dir = install(&home, "clyde", 1);
    let memory = tmp.path().join("team-memory");
    // What a hook from before this install left when it asked for a spool nothing wrote.
    let stamp = funes_home.join("spool/clyde.missing");
    let refused = || {
        fs::create_dir_all(stamp.parent().unwrap()).unwrap();
        fs::write(&stamp, "").unwrap();
    };
    refused();

    let out = funes_at_a_terminal(&home, &funes_home, &log, &["add", "clyde", memory.to_str().unwrap()]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{transcript}");
    assert!(transcript.contains("Trust it? [y/N]"), "{transcript}");
    assert!(!stamp.exists(), "add forgets the refusals");
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
    assert!(
        !home.join(".funes/agents/clyde.json").exists(),
        "nothing refreshed the files, so nothing is recorded"
    );

    fs::remove_file(&log).unwrap();
    refused();
    let out = funes_at_a_terminal(&home, &funes_home, &log, &["remove", "clyde"]);
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{transcript}");
    assert!(transcript.contains("Trust it? [y/N]"), "{transcript}");
    assert!(fs::read_to_string(&log).unwrap().starts_with("remove\n"));
    assert!(!dir.exists(), "the installed copy is taken with the integration");
    assert!(
        !stamp.exists(),
        "remove forgets the refusals with the hooks that left them"
    );

    // Gone, with no source to refresh it from: a second remove has nothing to do and says so.
    fs::remove_file(&log).unwrap();
    refused();
    let out = funes(&home, &funes_home, &log, &["remove", "clyde"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).starts_with("nothing to remove"), "{}", stderr(&out));
    assert!(!log.exists(), "no setup ran");
    assert!(!stamp.exists(), "and forgets the refusals all the same");
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
    assert!(err.contains("the installed copy, recorded by nothing"), "{err}");
    assert!(err.contains("run this in a terminal to confirm it"), "{err}");
    assert!(!log.exists(), "setup did not run");
    assert!(!funes_home.exists(), "no memory bootstrap started");
    assert!(dir.join("setup").exists(), "the installed copy is kept");

    // `remove` resolves the same way.
    let out = funes(&home, &funes_home, &log, &["remove", "clyde"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("the installed copy, recorded by nothing"), "{err}");
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
