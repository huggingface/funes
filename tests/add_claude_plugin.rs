//! `funes add claude` generates the hooks-only plugin tree — with the store baked into its
//! `hooks.json` — before it touches the `claude` CLI. Own test binary: it sets `$HOME` and clears
//! `$PATH` (both process-global) so the `claude`-absent branch runs deterministically; the plugin
//! files are written regardless, which is what we assert (registering with `claude` is exercised
//! manually, not here).

use funes::hooks::{self, Agent};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn add_claude_generates_the_plugin_tree() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    // No `claude` on PATH → funes extracts the plugin and returns Ok without registering it.
    std::env::set_var("PATH", "");

    hooks::install(Agent::Claude, Some("acme/kb")).unwrap();

    let root = home.path().join(".funes/integrations/claude-plugin");
    assert!(
        root.join(".claude-plugin/marketplace.json").exists(),
        "marketplace manifest"
    );
    assert!(
        root.join("funes/.claude-plugin/plugin.json").exists(),
        "plugin manifest"
    );

    // Scripts bundled in the plugin, executable.
    for name in ["funes-index.sh", "funes-push.sh"] {
        let p = root.join("funes/scripts").join(name);
        assert!(p.exists(), "{name} bundled");
        assert!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o111 != 0,
            "{name} executable"
        );
    }

    // hooks.json: plugin-root script refs, store baked, Stop + SessionStart + SessionEnd.
    let cfg: Value = serde_json::from_str(&fs::read_to_string(root.join("funes/hooks/hooks.json")).unwrap()).unwrap();
    let stop = cfg["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(
        stop.contains("${CLAUDE_PLUGIN_ROOT}/scripts/funes-index.sh") && stop.contains("claude"),
        "stop: {stop}"
    );
    assert!(
        cfg["hooks"].get("SessionStart").is_some(),
        "claude publishes on SessionStart too"
    );
    let end = cfg["hooks"]["SessionEnd"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(end.contains("funes-push.sh") && end.contains("acme/kb"), "end: {end}");
}
