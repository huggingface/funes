//! `funes remove` reverses each supported `add` integration while preserving memories and
//! unrelated agent configuration.

mod support;

use serde_json::Value;
use std::fs;

#[test]
fn remove_claude_unregisters_both_surfaces_and_deletes_the_installed_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "claude");
    // Installed on demand from the checkout, and taken with the integration.
    let plugin = home.join(".funes/agents/claude");
    // A pre-registry install is deleted too, without a second marketplace call: the registration is
    // by name, and the one below covers it.
    let legacy = home.join(".funes/integrations/claude-plugin");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("marker"), "owned").unwrap();
    let memory = home.join(".funes/memory/chunks.lance");
    fs::create_dir_all(&memory).unwrap();
    fs::write(memory.join("keep"), "memory").unwrap();

    let first = support::run_remove(&home, &bin, &log, "claude");
    support::assert_success(&first);
    assert!(!plugin.exists());
    assert!(!legacy.exists());
    assert_eq!(fs::read_to_string(memory.join("keep")).unwrap(), "memory");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "mcp remove funes -s user\n\
         plugin uninstall funes@huggingface\n\
         plugin marketplace remove huggingface\n"
    );

    // Already absent remains a successful no-op locally.
    let second = support::run_remove(&home, &bin, &log, "claude");
    support::assert_success(&second);
}

#[test]
fn remove_codex_unregisters_the_plugin_and_leaves_a_shared_hooks_file() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "codex");
    // Installed on demand from the checkout, and taken with the integration.
    let plugin = home.join(".funes/agents/codex");
    // A pre-plugin install that shares Codex's hooks file with a hook of the user's: taking funes's
    // entries out of it needs a parser, so the file stays, and so do the scripts it still runs.
    let codex_dir = home.join(".codex");
    let hooks = codex_dir.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("funes-index.sh"), "owned").unwrap();
    fs::write(hooks.join("user-hook.sh"), "keep").unwrap();
    fs::write(
        codex_dir.join("hooks.json"),
        r#"{
          "hooks": {
            "Stop": [
              { "hooks": [{ "type": "command", "command": "make lint" }] },
              { "hooks": [{ "type": "command", "command": "bash \"/old/funes-index.sh\" \"codex\"" }] }
            ]
          }
        }"#,
    )
    .unwrap();
    let memory = home.join(".funes/memory/chunks.lance");
    fs::create_dir_all(&memory).unwrap();
    fs::write(memory.join("keep"), "memory").unwrap();

    let first = support::run_remove(&home, &bin, &log, "codex");
    support::assert_success(&first);
    assert!(!plugin.exists());
    assert_eq!(fs::read_to_string(memory.join("keep")).unwrap(), "memory");
    let config: Value = serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap()).unwrap();
    assert_eq!(config["hooks"]["Stop"].as_array().unwrap().len(), 2, "left as it is");
    assert!(hooks.join("funes-index.sh").exists(), "the script its entry runs");
    assert!(hooks.join("user-hook.sh").exists());
    assert!(String::from_utf8_lossy(&first.stderr).contains("delete the groups"));
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "mcp remove funes\n\
         plugin remove funes@huggingface\n\
         plugin marketplace remove huggingface\n"
    );

    // Already absent remains a successful no-op locally.
    let second = support::run_remove(&home, &bin, &log, "codex");
    support::assert_success(&second);
}

#[test]
fn remove_hermes_disables_the_plugin_and_revokes_a_pre_plugin_installs_consent() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "hermes");
    let base = home.join(".hermes");
    let plugin = base.join("plugins/funes");
    fs::create_dir_all(&plugin).unwrap();
    fs::write(plugin.join("__init__.py"), "owned").unwrap();
    // An install from before the plugin: its scripts are funes's, its hook entries the user's.
    let hooks = base.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("funes-index.sh"), "owned").unwrap();
    fs::write(hooks.join("funes-sync.log"), "owned").unwrap();
    fs::write(hooks.join("user-hook.sh"), "keep").unwrap();
    fs::write(
        base.join("config.yaml"),
        "model: hermes-4\n\
         hooks:\n  \
           post_llm_call:\n  \
           - command: make lint\n  \
           - command: bash \"/old/funes-index.sh\" \"hermes\"\n",
    )
    .unwrap();

    let first = support::run_remove(&home, &bin, &log, "hermes");
    support::assert_success(&first);
    assert!(!plugin.exists(), "the plugin goes");
    assert!(!hooks.join("funes-index.sh").exists(), "and funes's own scripts");
    assert!(!hooks.join("funes-sync.log").exists());
    assert!(hooks.join("user-hook.sh").exists());
    // The entries live in the file that holds the user's configuration, so they are named, not cut.
    let config = fs::read_to_string(base.join("config.yaml")).unwrap();
    assert!(
        config.contains("make lint") && config.contains("funes-index.sh"),
        "{config}"
    );
    assert!(String::from_utf8_lossy(&first.stderr).contains("delete the entries"));
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "config path\n\
         plugins disable funes\n\
         hooks revoke bash \"/old/funes-index.sh\" \"hermes\"\n\
         mcp remove funes\n"
    );

    // Already absent remains a successful no-op locally.
    let second = support::run_remove(&home, &bin, &log, "hermes");
    support::assert_success(&second);
}

#[test]
fn remove_pi_unregisters_the_extension_and_deletes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "pi");
    // The integration is installed on demand from the checkout, so `remove` can uninstall what an
    // older funes left behind.
    let extension = home.join(".funes/agents/pi");

    let first = support::run_remove(&home, &bin, &log, "pi");
    support::assert_success(&first);
    assert!(!extension.exists());
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("remove {}\n", extension.display())
    );

    let second = support::run_remove(&home, &bin, &log, "pi");
    support::assert_success(&second);
}

#[test]
fn missing_agent_cli_still_removes_owned_claude_and_pi_files() {
    let tmp = tempfile::tempdir().unwrap();
    let empty_bin = tmp.path().join("empty-bin");
    fs::create_dir_all(&empty_bin).unwrap();

    let claude_home = tmp.path().join("claude-home");
    let plugin = claude_home.join(".funes/integrations/claude-plugin");
    fs::create_dir_all(&plugin).unwrap();
    fs::write(plugin.join("owned"), "plugin").unwrap();
    let claude = support::run_remove(
        &claude_home,
        &empty_bin,
        &tmp.path().join("unused-claude.log"),
        "claude",
    );
    support::assert_success(&claude);
    assert!(!plugin.exists());
    assert!(String::from_utf8_lossy(&claude.stdout).contains("remove the registrations manually"));

    let pi_home = tmp.path().join("pi-home");
    let extension = pi_home.join(".funes/integrations/pi");
    fs::create_dir_all(&extension).unwrap();
    fs::write(extension.join("index.ts"), "extension").unwrap();
    let pi = support::run_remove(&pi_home, &empty_bin, &tmp.path().join("unused-pi.log"), "pi");
    support::assert_success(&pi);
    assert!(!extension.exists());
    assert!(String::from_utf8_lossy(&pi.stdout).contains("remove the registration manually"));
}
