//! `funes install opencode`: register funes recall as an MCP server with opencode.
//!
//! opencode reads MCP servers from `opencode.json` and merges a project-level config
//! from the cwd. This adds funes as a `local` (stdio) MCP server (`funes mcp`) to that
//! config, merging into any existing one rather than clobbering it. Defaults to the cwd
//! (project); `global` writes the user config (`~/.config/opencode/opencode.json`). The
//! command is `funes` from PATH (override with `FUNES_BIN`).

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn install(global: bool) -> Result<()> {
    let path = if global {
        config_home().join("opencode").join("opencode.json")
    } else {
        std::env::current_dir()
            .context("resolving the current directory")?
            .join("opencode.json")
    };

    // Merge into an existing config so we don't drop the user's other settings.
    let mut cfg = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "$schema": "https://opencode.ai/config.json" }));

    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let obj = cfg.as_object_mut().expect("cfg is an object");
    let mcp = obj
        .entry("mcp")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`mcp` in opencode.json is not an object")?;
    mcp.insert(
        "funes".to_string(),
        json!({ "type": "local", "command": [funes, "mcp"], "enabled": true }),
    );

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&cfg)?))
        .with_context(|| format!("writing {}", path.display()))?;

    let scope = if global { "user" } else { "project" };
    println!(
        "installed funes recall into opencode ({scope} scope) at {} — `funes_recall`/`funes_get` are now available (restart opencode if it's running).",
        path.display()
    );
    Ok(())
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"))
}
