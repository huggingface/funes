//! `funes add` on a fresh machine: the integration converts the agent's history into its spool,
//! then funes indexes it, so recall has content before the first turn — and declining the first
//! index installs nothing. Drives the real claude bundle against a fake `claude` through the
//! binary; every process-global path (`$HOME`, `$PATH`, `$FUNES_HOME`) is set per run, so the two
//! runs share nothing.

mod support;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The converter's own fixture, placed where Claude Code keeps its transcripts. The session id is
/// the transcript's stem.
const SESSION: &str = "06ec42c3-2184-40c5-b0ee-98c3235b4c4c";

fn write_history(home: &Path) {
    let project = home.join(".claude/projects/D--physics-earth");
    fs::create_dir_all(&project).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/claude/claude-plugin/funes/test/session.jsonl"),
        project.join(format!("{SESSION}.jsonl")),
    )
    .unwrap();
}

/// The embedder's weights live in the real cache; `$HOME` is fake below.
fn hf_home() -> PathBuf {
    std::env::var_os("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("a home")).join(".cache/huggingface"))
}

fn funes(home: &Path, funes_home: &Path, bin: &Path, log: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_funes"));
    cmd.env("HOME", home)
        .env("FUNES_HOME", funes_home)
        // The fake `claude` first, then the system utilities the bundle's scripts need.
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("FUNES_TEST_CLI_LOG", log)
        .env("HF_HOME", hf_home())
        // Nothing that would offer a memory or record a funes other than the default.
        .env_remove("HF_TOKEN")
        .env_remove("HUGGING_FACE_HUB_TOKEN")
        .env_remove("HUGGINGFACE_TOKEN")
        .env_remove("FUNES_BIN");
    cmd
}

/// `funes add claude`, answering the first-index prompt with `answer`.
fn run_add(home: &Path, funes_home: &Path, bin: &Path, log: &Path, answer: &str) -> Output {
    let mut child = funes(home, funes_home, bin, log)
        .args(["add", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(answer.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn a_first_add_converts_the_history_then_indexes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "claude");
    write_history(&home);

    let out = run_add(&home, &funes_home, &bin, &log, "y\n");
    support::assert_success(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("installed funes into Claude Code"),
        "setup ran"
    );
    assert!(
        stderr.contains("indexing your recent claude sessions"),
        "the seed found the spool setup wrote: {stderr}"
    );
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .ends_with("mcp add funes -s user -- funes mcp\n"),
        "local memory bound"
    );
    // The whole of the converted session reached the memory, so its spool copy is gone…
    assert!(
        !funes_home
            .join("spool/claude")
            .join(format!("{SESSION}.funes.jsonl"))
            .exists(),
        "the seed drained the spool"
    );
    // …and it is what recall finds.
    let out = funes(&home, &funes_home, &bin, &log)
        .args(["recall", "a 3D digital twin of Earth"])
        .output()
        .unwrap();
    support::assert_success(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&SESSION[..8]),
        "recall reaches the seeded session: {stdout}"
    );
}

#[test]
fn declining_the_first_index_installs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "claude");
    write_history(&home);

    let out = run_add(&home, &funes_home, &bin, &log, "n\n");
    support::assert_success(&out);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nothing was wired up"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!log.exists(), "claude was never called");
    assert!(
        !home.join(".funes/agents/claude/claude-plugin/funes/hooks").exists(),
        "no hooks written"
    );
    assert!(!funes_home.join("spool").exists(), "nothing converted");
    assert!(!funes_home.join("memory").exists(), "nothing indexed");
}
