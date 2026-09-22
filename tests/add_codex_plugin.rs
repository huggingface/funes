//! `funes add codex` installs the plugin carrying the skill and the hooks, leaves the memory in a
//! file beside the scripts, and registers the MCP server with Codex. Own test binary: it sets
//! `$HOME` and `$PATH` (both process-global) so the integration's script runs against a fake `codex`.

mod support;

use funes::agents::registry;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn add_codex_installs_the_plugin_and_clears_a_pre_plugin_install() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "codex");

    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    std::env::set_var("FUNES_TEST_CLI_LOG", &log);
    // The variables that name real state elsewhere: Codex's own home, which the script cleans a
    // pre-plugin install out of, the memory this install would seed, and the funes binary it records.
    std::env::remove_var("CODEX_HOME");
    std::env::remove_var("FUNES_HOME");
    std::env::remove_var("FUNES_BIN");

    // What an install before the plugin left: hook entries funes wrote in Codex's own file, the
    // scripts they ran, and a skill in Codex's tree and in the shared one an earlier install used.
    let codex_dir = home.join(".codex");
    fs::create_dir_all(codex_dir.join("hooks")).unwrap();
    fs::write(
        codex_dir.join("hooks.json"),
        r#"{
          "hooks": {
            "Stop": [
              { "hooks": [{ "type": "command", "command": "bash \"/old/funes-index.sh\" \"codex\"" }] }
            ]
          }
        }"#,
    )
    .unwrap();
    fs::write(codex_dir.join("hooks/funes-index.sh"), "old").unwrap();
    fs::write(codex_dir.join("hooks/funes-sync.log"), "old").unwrap();
    for skill in [
        codex_dir.join("skills/funes/SKILL.md"),
        home.join(".agents/skills/funes/SKILL.md"),
    ] {
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "stale").unwrap();
    }

    let root = registry::default_root().unwrap();
    registry::provision(&root, "codex", false).await.unwrap();
    registry::open(&root, "codex").unwrap().add(Some("acme/kb")).unwrap();

    let marketplace = home.join(".funes/agents/codex/codex-plugin");
    let plugin = marketplace.join("plugins/funes");
    assert!(
        marketplace.join(".agents/plugins/marketplace.json").exists(),
        "marketplace manifest"
    );
    assert!(plugin.join(".codex-plugin/plugin.json").exists(), "plugin manifest");
    // The skill Codex lists before loading any tool now rides in the plugin.
    let skill = fs::read_to_string(plugin.join("skills/funes/SKILL.md")).unwrap();
    assert!(skill.contains("name: funes"), "{skill}");

    // The scripts the hooks drive are symlinks in the checkout and must arrive as executable files.
    for name in ["funes-index.sh", "funes-push.sh"] {
        let path = plugin.join("scripts").join(name);
        assert!(
            !path.symlink_metadata().unwrap().file_type().is_symlink(),
            "{name} is a file"
        );
        assert!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o111 != 0,
            "{name} executable"
        );
    }

    let cfg: Value = serde_json::from_str(&fs::read_to_string(plugin.join("hooks.json")).unwrap()).unwrap();
    let stop = cfg["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(
        stop.contains("${PLUGIN_ROOT}/scripts/funes-index.sh") && stop.contains("codex"),
        "stop: {stop}"
    );
    assert!(
        cfg["hooks"].get("SessionStart").is_some(),
        "codex publishes on SessionStart too"
    );
    let end = cfg["hooks"]["SessionEnd"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(end.contains("funes-push.sh"), "end: {end}");
    // The memory rides in a file, not the hook command, so the hooks stay static files.
    assert!(!end.contains("acme/kb"), "end: {end}");
    assert_eq!(fs::read_to_string(plugin.join("scripts/memory")).unwrap(), "acme/kb\n");

    // The pre-plugin install is gone: funes wrote every hook in that file, so the file goes with the
    // scripts its entries ran, and both skill copies with it.
    assert!(!codex_dir.join("hooks.json").exists(), "hook entries dropped");
    assert!(!codex_dir.join("hooks").exists(), "and the scripts they ran");
    assert!(!codex_dir.join("skills/funes").exists(), "Codex's own skill copy");
    assert!(!home.join(".agents").exists(), "and the shared tree's");

    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!(
            "plugin marketplace add {}\n\
             plugin add funes@huggingface\n\
             mcp add funes -- funes mcp acme/kb\n",
            marketplace.display()
        )
    );
}
