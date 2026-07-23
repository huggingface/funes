//! `funes remove` reverses each supported `add` integration while preserving memories and
//! unrelated agent configuration.

mod support;

use serde_json::Value;
use std::fs;

#[test]
fn remove_claude_unregisters_both_surfaces_and_deletes_the_extracted_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "claude");
    let plugin = home.join(".funes/integrations/claude-plugin");
    fs::create_dir_all(plugin.join("funes/scripts")).unwrap();
    fs::write(plugin.join("funes/scripts/funes-index.sh"), "owned").unwrap();
    let memory = home.join(".funes/memory/chunks.lance");
    fs::create_dir_all(&memory).unwrap();
    fs::write(memory.join("keep"), "memory").unwrap();

    let first = support::run_remove(&home, &bin, &log, "claude");
    support::assert_success(&first);
    assert!(!plugin.exists());
    assert_eq!(fs::read_to_string(memory.join("keep")).unwrap(), "memory");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "mcp remove funes -s user\n\
         plugin uninstall funes@huggingface -s user\n\
         plugin marketplace remove huggingface --scope user\n"
    );

    // Already absent remains a successful no-op locally.
    let second = support::run_remove(&home, &bin, &log, "claude");
    support::assert_success(&second);
}

#[test]
fn remove_codex_preserves_user_hooks_and_files() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "codex");
    let base = home.join(".codex");
    let hooks = base.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("funes-index.sh"), "owned").unwrap();
    fs::write(hooks.join("funes-push.sh"), "owned").unwrap();
    fs::write(hooks.join("funes-sync.log"), "owned").unwrap();
    fs::write(hooks.join("user-hook.sh"), "keep").unwrap();
    fs::write(
        base.join("hooks.json"),
        r#"{
          "theme": "dark",
          "hooks": {
            "Stop": [
              { "hooks": [{ "type": "command", "command": "make lint" }] },
              { "hooks": [{ "type": "command", "command": "bash \"/old/funes-index.sh\" \"codex\"" }] }
            ],
            "SessionStart": [
              { "hooks": [{ "type": "command", "command": "bash \"/old/funes-push.sh\" \"acme/kb\"" }] }
            ]
          }
        }"#,
    )
    .unwrap();

    let first = support::run_remove(&home, &bin, &log, "codex");
    support::assert_success(&first);
    let config: Value = serde_json::from_str(&fs::read_to_string(base.join("hooks.json")).unwrap()).unwrap();
    assert_eq!(config["theme"], "dark");
    assert_eq!(config["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert_eq!(config["hooks"]["Stop"][0]["hooks"][0]["command"], "make lint");
    assert!(config["hooks"].get("SessionStart").is_none());
    assert!(hooks.join("user-hook.sh").exists());
    for owned in ["funes-index.sh", "funes-push.sh", "funes-sync.log"] {
        assert!(!hooks.join(owned).exists(), "{owned} removed");
    }
    assert_eq!(fs::read_to_string(&log).unwrap(), "mcp remove funes\n");

    let second = support::run_remove(&home, &bin, &log, "codex");
    support::assert_success(&second);
}

#[test]
fn remove_hermes_preserves_user_hooks_approvals_and_files() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "hermes");
    let base = home.join(".hermes");
    let hooks = base.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("funes-index.sh"), "owned").unwrap();
    fs::write(hooks.join("funes-push.sh"), "owned").unwrap();
    fs::write(hooks.join("funes-sync.log"), "owned").unwrap();
    fs::write(hooks.join("user-hook.sh"), "keep").unwrap();
    fs::write(
        base.join("config.yaml"),
        "model: hermes-4\n\
         hooks:\n  \
           post_llm_call:\n    \
             - command: make lint\n      \
               timeout: 10\n    \
             - command: bash \"/old/funes-index.sh\" \"hermes\"\n  \
           on_session_start:\n    \
             - command: bash \"/old/funes-push.sh\" \"acme/kb\"\n",
    )
    .unwrap();
    fs::write(
        base.join("shell-hooks-allowlist.json"),
        r#"{
          "trusted": true,
          "approvals": [
            { "event": "pre_tool_call", "command": "guard.sh" },
            { "event": "post_llm_call", "command": "bash \"/old/funes-index.sh\" \"hermes\"" }
          ]
        }"#,
    )
    .unwrap();

    let first = support::run_remove(&home, &bin, &log, "hermes");
    support::assert_success(&first);
    let config: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(base.join("config.yaml")).unwrap()).unwrap();
    assert_eq!(config["model"].as_str(), Some("hermes-4"));
    assert_eq!(config["hooks"]["post_llm_call"].as_sequence().unwrap().len(), 1);
    assert!(config["hooks"].get("on_session_start").is_none());
    let allowlist: Value =
        serde_json::from_str(&fs::read_to_string(base.join("shell-hooks-allowlist.json")).unwrap()).unwrap();
    assert_eq!(allowlist["trusted"], true);
    assert_eq!(allowlist["approvals"].as_array().unwrap().len(), 1);
    assert_eq!(allowlist["approvals"][0]["command"], "guard.sh");
    assert!(hooks.join("user-hook.sh").exists());
    for owned in ["funes-index.sh", "funes-push.sh", "funes-sync.log"] {
        assert!(!hooks.join(owned).exists(), "{owned} removed");
    }
    assert_eq!(fs::read_to_string(&log).unwrap(), "mcp remove funes\n");

    let second = support::run_remove(&home, &bin, &log, "hermes");
    support::assert_success(&second);
}

#[test]
fn remove_pi_unregisters_the_fixed_source_and_deletes_the_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "pi");
    let extension = home.join(".funes/integrations/pi");
    fs::create_dir_all(&extension).unwrap();
    fs::write(extension.join("index.ts"), "owned").unwrap();
    fs::write(extension.join("memory"), "acme/kb\n").unwrap();

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

#[test]
fn malformed_hook_configs_do_not_block_mcp_unregistration() {
    let tmp = tempfile::tempdir().unwrap();

    let codex_home = tmp.path().join("codex-home");
    fs::create_dir_all(codex_home.join(".codex")).unwrap();
    fs::write(codex_home.join(".codex/hooks.json"), "not json").unwrap();
    let codex_log = tmp.path().join("codex-cli.log");
    let codex_bin = support::fake_cli(&tmp.path().join("codex-fake"), "codex");
    let codex = support::run_remove(&codex_home, &codex_bin, &codex_log, "codex");
    assert!(!codex.status.success(), "malformed hooks.json remains an error");
    assert_eq!(fs::read_to_string(codex_log).unwrap(), "mcp remove funes\n");

    let hermes_home = tmp.path().join("hermes-home");
    fs::create_dir_all(hermes_home.join(".hermes")).unwrap();
    fs::write(hermes_home.join(".hermes/config.yaml"), "[not, a, mapping]\n").unwrap();
    let hermes_log = tmp.path().join("hermes-cli.log");
    let hermes_bin = support::fake_cli(&tmp.path().join("hermes-fake"), "hermes");
    let hermes = support::run_remove(&hermes_home, &hermes_bin, &hermes_log, "hermes");
    assert!(!hermes.status.success(), "malformed config.yaml remains an error");
    assert_eq!(fs::read_to_string(hermes_log).unwrap(), "mcp remove funes\n");
}
