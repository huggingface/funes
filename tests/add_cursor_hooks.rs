//! funes add cursor owns direct Cursor hook entries and one global MCP server, while preserving
//! the user's other entries. This test sets process-global HOME, so it has its own integration
//! test binary.

use funes::agents::cursor;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn command_for<'a>(cfg: &'a Value, event: &str, needle: &str) -> &'a str {
    cfg["hooks"][event]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["command"].as_str().unwrap_or_default().contains(needle))
        .and_then(|entry| entry["command"].as_str())
        .unwrap()
}

#[test]
fn add_cursor_installs_stop_hook_and_mcp_without_clobbering_user_config() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    std::env::set_var("PATH", "");
    std::env::remove_var("FUNES_BIN");

    let root = home.path().join(".cursor");
    let hooks_config = root.join("hooks.json");
    let mcp_config = root.join("mcp.json");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        &hooks_config,
        r#"{
          "version": 1,
          "hooks": {
            "stop": [
              {"command": "guard.sh"},
              {"command": "bash \"/old/funes-index.sh\" \"cursor\""}
            ],
            "postToolUse": [{"command": "audit.sh"}]
          }
        }"#,
    )
    .unwrap();
    fs::write(
        &mcp_config,
        r#"{
          "mcpServers": {
            "other": {"command": "other"},
            "funes": {"type": "stdio", "command": "/old/funes", "args": ["mcp"]}
          }
        }"#,
    )
    .unwrap();

    cursor::install(Some("acme/kb".to_string())).unwrap();

    let hooks_dir = root.join("hooks");
    for name in ["funes-index.sh", "funes-push.sh"] {
        let path = hooks_dir.join(name);
        assert!(path.exists(), "{name} written");
        assert!(fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0);
    }

    let hooks: Value = serde_json::from_str(&fs::read_to_string(&hooks_config).unwrap()).unwrap();
    assert_eq!(hooks["version"], 1);
    assert_eq!(hooks["hooks"]["stop"].as_array().unwrap().len(), 2);
    let stop = command_for(&hooks, "stop", "funes-index.sh");
    assert!(stop.contains("cursor"), "stop: {stop}");
    assert_eq!(hooks["hooks"]["stop"][0]["command"], "guard.sh");
    assert_eq!(hooks["hooks"]["postToolUse"][0]["command"], "audit.sh");
    for event in ["sessionStart", "sessionEnd"] {
        let command = command_for(&hooks, event, "funes-push.sh");
        assert!(
            command.contains("acme/kb") && command.contains("cursor"),
            "{event}: {command}"
        );
    }
    let funes_stop = hooks["hooks"]["stop"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["command"].as_str().unwrap_or_default().contains("funes-index.sh"))
        .unwrap();
    assert_eq!(funes_stop["type"], "command");
    assert_eq!(funes_stop["timeout"], 15);

    let mcp: Value = serde_json::from_str(&fs::read_to_string(&mcp_config).unwrap()).unwrap();
    assert_eq!(mcp["mcpServers"]["other"]["command"], "other");
    assert_eq!(mcp["mcpServers"]["funes"]["type"], "stdio");
    assert_eq!(mcp["mcpServers"]["funes"]["command"], "funes");
    assert_eq!(
        mcp["mcpServers"]["funes"]["args"],
        serde_json::json!(["mcp", "acme/kb"])
    );

    // A local re-run replaces the binding and removes stale publishing events without duplicating
    // the stop hook or touching user entries.
    cursor::install(None).unwrap();
    let hooks2: Value = serde_json::from_str(&fs::read_to_string(&hooks_config).unwrap()).unwrap();
    assert_eq!(hooks2["hooks"]["stop"].as_array().unwrap().len(), 2);
    assert!(hooks2["hooks"].get("sessionStart").is_none());
    assert!(hooks2["hooks"].get("sessionEnd").is_none());
    assert_eq!(hooks2["hooks"]["stop"][0]["command"], "guard.sh");
    assert_eq!(hooks2["hooks"]["postToolUse"][0]["command"], "audit.sh");
    let mcp2: Value = serde_json::from_str(&fs::read_to_string(&mcp_config).unwrap()).unwrap();
    assert_eq!(mcp2["mcpServers"]["other"]["command"], "other");
    assert_eq!(mcp2["mcpServers"]["funes"]["args"], serde_json::json!(["mcp"]));

    cursor::uninstall().unwrap();
    let hooks3: Value = serde_json::from_str(&fs::read_to_string(&hooks_config).unwrap()).unwrap();
    assert_eq!(hooks3["hooks"]["stop"][0]["command"], "guard.sh");
    assert_eq!(hooks3["hooks"]["postToolUse"][0]["command"], "audit.sh");
    assert!(hooks3["hooks"].get("stop").unwrap().as_array().unwrap().len() == 1);
    assert!(!hooks_dir.exists(), "only the funes-owned hooks directory is removed");
    let mcp3: Value = serde_json::from_str(&fs::read_to_string(&mcp_config).unwrap()).unwrap();
    assert_eq!(mcp3["mcpServers"]["other"]["command"], "other");
    assert!(mcp3["mcpServers"].get("funes").is_none());
}
