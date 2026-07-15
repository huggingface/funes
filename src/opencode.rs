//! `funes add opencode`: register funes recall as an MCP server with opencode.
//!
//! opencode merges several config layers; funes writes the user config (`$OPENCODE_CONFIG` if
//! set, else `~/.config/opencode/opencode.json`) and adds itself as a `local` (stdio) MCP
//! server (`funes mcp`), preserving any existing settings. Both `.json` and `.jsonc` are
//! honored; a config with comments is never rewritten (serde can't round-trip comments, so we
//! print the block to add by hand rather than dropping them). The command is `funes` from PATH
//! (override with `FUNES_BIN`).

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The `local` MCP-server block funes writes: `funes mcp [store]` over stdio. A non-local `store`
/// is appended to the command, pinning this agent's recall to it.
fn server_json(funes: &str, store: Option<&str>) -> Value {
    let mut command = vec![funes.to_string(), "mcp".to_string()];
    if let Some(s) = store {
        command.push(s.to_string());
    }
    json!({ "type": "local", "command": command, "enabled": true })
}

pub fn install(store: Option<String>) -> Result<()> {
    let path = target_path()?;
    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let server = server_json(&funes, store.as_deref());

    // An existing config may carry comments (JSONC), which serde can't round-trip. If the file
    // is there but doesn't parse as plain JSON, leave it untouched and tell the user exactly
    // what to add — clobbering it would silently drop their settings and comments.
    let mut cfg = match std::fs::read_to_string(&path).ok().as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => match serde_json::from_str::<Value>(s) {
            Ok(v) if v.is_object() => v,
            _ => return manual_instructions(&path, &server),
        },
        _ => json!({ "$schema": "https://opencode.ai/config.json" }),
    };

    let mcp = cfg
        .as_object_mut()
        .expect("cfg is an object")
        .entry("mcp")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`mcp` in the opencode config is not an object")?;
    mcp.insert("funes".to_string(), server);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&cfg)?))
        .with_context(|| format!("writing {}", path.display()))?;

    println!(
        "installed funes recall into opencode (user scope) at {} — `funes_recall`/`funes_get` are now available (restart opencode if it's running).",
        path.display()
    );
    Ok(())
}

/// The user config file to edit: `$OPENCODE_CONFIG` if set, else the user config dir. In that
/// dir an existing `opencode.json` wins over an existing `opencode.jsonc`, which wins over
/// creating a fresh `opencode.json`.
fn target_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("OPENCODE_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    Ok(choose(&config_home().join("opencode")))
}

/// Pick the config file in `dir`: an existing `opencode.json`, else an existing
/// `opencode.jsonc`, else a new `opencode.json`.
fn choose(dir: &Path) -> PathBuf {
    let json = dir.join("opencode.json");
    let jsonc = dir.join("opencode.jsonc");
    if !json.exists() && jsonc.exists() {
        jsonc
    } else {
        json
    }
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"))
}

/// When an existing config can't be parsed (e.g. JSONC with comments), don't clobber it —
/// print the MCP-server block for the user to merge by hand.
fn manual_instructions(path: &Path, server: &Value) -> Result<()> {
    let block = serde_json::to_string_pretty(&json!({ "mcp": { "funes": server.clone() } }))?;
    println!(
        "{} has comments or isn't plain JSON — leaving it untouched. Merge this in to enable funes recall:\n{block}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{choose, server_json};
    use serde_json::json;

    #[test]
    fn server_json_bakes_the_store_only_when_present() {
        assert_eq!(server_json("funes", None)["command"], json!(["funes", "mcp"]));
        assert_eq!(
            server_json("funes", Some("acme/kb"))["command"],
            json!(["funes", "mcp", "acme/kb"])
        );
    }

    #[test]
    fn choose_prefers_json_then_jsonc_then_new_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();

        // Nothing present → create opencode.json.
        assert_eq!(choose(p), p.join("opencode.json"));

        // Only a .jsonc present → edit it (the file opencode actually reads).
        std::fs::write(p.join("opencode.jsonc"), "{}").unwrap();
        assert_eq!(choose(p), p.join("opencode.jsonc"));

        // Both present → .json wins.
        std::fs::write(p.join("opencode.json"), "{}").unwrap();
        assert_eq!(choose(p), p.join("opencode.json"));
    }
}
