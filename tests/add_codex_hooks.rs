//! `funes add codex` writes the automation hooks into `~/.codex/hooks.json` + the scripts, and its
//! append-or-replace merge leaves any hooks already there alone. Own test binary: it sets `$HOME`
//! (process-global), so it can't share a binary with other env-setting tests.

use funes::hooks::{self, Agent};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn add_codex_installs_hooks_and_preserves_existing() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    let config = home.path().join(".codex/hooks.json");

    // A hook the user already had must survive funes's merge.
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        r#"{ "hooks": { "PreToolUse": [ { "hooks": [ { "type": "command", "command": "guard.sh" } ] } ] } }"#,
    )
    .unwrap();

    hooks::install(Agent::Codex, Some("acme/kb")).unwrap();

    // Scripts written and executable.
    let hooks_dir = home.path().join(".codex/hooks");
    for name in ["funes-index.sh", "funes-push.sh"] {
        let p = hooks_dir.join(name);
        assert!(p.exists(), "{name} written");
        assert!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o111 != 0,
            "{name} executable"
        );
    }

    let cfg: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    // funes's hooks: Stop (index codex) + SessionStart (push acme/kb); Codex has no SessionEnd.
    let stop = cfg["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(
        stop.contains("funes-index.sh") && stop.contains("codex"),
        "stop: {stop}"
    );
    let start = cfg["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(
        start.contains("funes-push.sh") && start.contains("acme/kb"),
        "start: {start}"
    );
    assert!(
        cfg["hooks"].get("SessionEnd").is_none(),
        "codex has no SessionEnd publish"
    );
    // The user's own hook is untouched.
    assert_eq!(cfg["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "guard.sh");

    // Re-run local: funes's push hook is dropped, its index hook stays (no dup), user hook survives.
    hooks::install(Agent::Codex, None).unwrap();
    let cfg2: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert!(
        cfg2["hooks"].get("SessionStart").is_none(),
        "local re-run drops the push hook"
    );
    assert_eq!(
        cfg2["hooks"]["Stop"].as_array().unwrap().len(),
        1,
        "no duplicate index hook"
    );
    assert_eq!(
        cfg2["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "guard.sh",
        "user hook still there"
    );
}
