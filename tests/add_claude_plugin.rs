//! `funes add claude` installs the hooks-only plugin, bakes the memory into its `hooks.json`, and
//! registers both surfaces with Claude. Own test binary: it sets `$HOME` and `$PATH` (both
//! process-global) so the integration's script runs against a fake `claude`.

mod support;

use funes::agents::registry;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn add_claude_installs_the_plugin_and_registers_both_surfaces() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "claude");

    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    std::env::set_var("FUNES_TEST_CLI_LOG", &log);
    // The variables that name real state elsewhere: the memory this install would seed, and the
    // funes binary it would record.
    std::env::remove_var("FUNES_HOME");
    std::env::remove_var("FUNES_BIN");

    let root = registry::default_root().unwrap();
    registry::provision(&root, "claude", false).await.unwrap();
    registry::open(&root, "claude").unwrap().add(Some("acme/kb")).unwrap();

    let plugin = home.join(".funes/agents/claude/claude-plugin");
    assert!(
        plugin.join(".claude-plugin/marketplace.json").exists(),
        "marketplace manifest"
    );
    assert!(
        plugin.join("funes/.claude-plugin/plugin.json").exists(),
        "plugin manifest"
    );

    // The scripts the hooks drive are symlinks in the checkout and must arrive as executable files.
    for name in ["funes-index.sh", "funes-push.sh"] {
        let path = plugin.join("funes/scripts").join(name);
        assert!(
            !path.symlink_metadata().unwrap().file_type().is_symlink(),
            "{name} is a file"
        );
        assert!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o111 != 0,
            "{name} executable"
        );
    }

    let cfg: Value = serde_json::from_str(&fs::read_to_string(plugin.join("funes/hooks/hooks.json")).unwrap()).unwrap();
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

    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!(
            "plugin marketplace add {}\n\
             plugin uninstall funes@huggingface\n\
             plugin install funes@huggingface\n\
             mcp remove funes\n\
             mcp add funes -s user -- funes mcp acme/kb\n",
            plugin.display()
        )
    );
}
