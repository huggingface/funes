//! A per-turn hook converts the transcript its payload names, then every transcript changed since
//! the hook last ran — the sessions whose own hook never fired. Drives the claude and codex workers
//! against a fake agent home, with a fake `funes` on PATH so only the conversion is under test.

mod support;

use funes::agents::registry;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

/// Filesystems stamp to the second at worst, and `find -newer` compares stamps.
const TICK: Duration = Duration::from_millis(1100);

/// Install `id` from the checkout into a registry under `home`, with the `spool` record `setup add`
/// writes beside its scripts; returns the scripts directory.
async fn install(home: &Path, id: &str, scripts: &str, spool: &Path) -> PathBuf {
    let root = home.join(".funes/agents");
    registry::provision(&root, id, false).await.unwrap();
    let scripts = root.join(id).join(scripts);
    fs::create_dir_all(spool).unwrap();
    fs::write(scripts.join("spool"), format!("{}\n", spool.display())).unwrap();
    scripts
}

/// Run the worker half of the hook, as the foreground half would, with `payload`; `mode` is
/// `--publish` at a session boundary and empty per turn.
fn fire(scripts: &Path, home: &Path, bin: &Path, payload: &str, mode: &str) {
    let out = Command::new("sh")
        .arg(scripts.join("funes-index.sh"))
        .arg("--worker")
        .arg(payload)
        .arg(mode)
        .env("HOME", home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("FUNES_TEST_CLI_LOG", home.join("cli.log"))
        .env_remove("CODEX_HOME")
        .output()
        .unwrap();
    support::assert_success(&out);
}

/// A transcript written now: `fs::copy` would keep the fixture's stamp, and the sweep goes by stamps.
fn transcript(dir: &Path, name: &str, fixture: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, fs::read(fixture).unwrap()).unwrap();
    path
}

fn converted(spool: &Path, stem: &str) -> bool {
    spool.join(format!("{stem}.funes.jsonl")).is_file()
}

/// The journey both hooks share: a session whose hook never fired is converted by the next one; a
/// session converted already is not converted again; one written after that is; and a boundary
/// converts before it publishes.
async fn catches_up(
    id: &str,
    scripts_rel: &str,
    fixture: &str,
    tree: &str,
    names: [&str; 4],
    payload: fn(&Path) -> String,
) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let bin = support::fake_cli(tmp.path(), "funes");
    let spool = tmp.path().join("spool");
    let scripts = install(&home, id, scripts_rel, &spool).await;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture);
    let tree = home.join(tree);
    let [a, b, c, d] = names;
    let stem = |name: &str| name.trim_end_matches(".jsonl").to_string();

    // Two sessions written after the install: the hook fires for one of them only.
    sleep(TICK);
    let named = transcript(&tree, a, &fixture);
    transcript(&tree, b, &fixture);
    fire(&scripts, &home, &bin, &payload(&named), "");
    assert!(converted(&spool, &stem(a)), "the session the payload named");
    assert!(converted(&spool, &stem(b)), "and the one whose hook never fired");
    assert!(scripts.join("swept").is_file(), "the sweep left its mark");

    // A third, written after that run; the second, drained by funes meanwhile, is not re-emitted.
    sleep(TICK);
    transcript(&tree, c, &fixture);
    fs::remove_file(spool.join(format!("{}.funes.jsonl", stem(b)))).unwrap();
    fire(&scripts, &home, &bin, &payload(&named), "");
    assert!(converted(&spool, &stem(c)), "written since the last sweep");
    assert!(!converted(&spool, &stem(b)), "unchanged since the last sweep");

    // A boundary: what was written since is converted first, then the publish worker indexes and
    // pushes to the memory recorded beside the scripts.
    sleep(TICK);
    transcript(&tree, d, &fixture);
    fs::write(scripts.join("memory"), "acme/kb\n").unwrap();
    fire(&scripts, &home, &bin, &payload(&named), "--publish");
    assert!(converted(&spool, &stem(d)), "converted before the publish");

    let log = fs::read_to_string(home.join("cli.log")).unwrap();
    assert_eq!(
        log,
        format!("index --harness {id}\nindex --harness {id}\nindex --harness {id}\npush acme/kb\n")
    );
}

#[tokio::test]
async fn the_claude_hook_converts_the_transcripts_its_predecessors_missed() {
    catches_up(
        "claude",
        "claude-plugin/funes/scripts",
        "integrations/claude/claude-plugin/funes/test/session.jsonl",
        ".claude/projects/-Users-me-repo",
        ["a.jsonl", "b.jsonl", "c.jsonl", "d.jsonl"],
        |named| format!(r#"{{"transcript_path":"{}"}}"#, named.display()),
    )
    .await;
}

#[tokio::test]
async fn the_codex_hook_converts_the_rollouts_its_predecessors_missed() {
    catches_up(
        "codex",
        "codex-plugin/plugins/funes/scripts",
        "integrations/codex/codex-plugin/plugins/funes/test/session.jsonl",
        ".codex/sessions/2026/09/24",
        [
            "rollout-a.jsonl",
            "rollout-b.jsonl",
            "rollout-c.jsonl",
            "rollout-d.jsonl",
        ],
        |named| format!(r#"{{"transcript_path":"{}","session_id":"s"}}"#, named.display()),
    )
    .await;
}
