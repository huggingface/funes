//! `funes add cursor` / `funes remove cursor`: install Cursor's user-level hooks and MCP entry.
//!
//! Cursor keeps global hooks in `~/.cursor/hooks.json` and global MCP servers in
//! `~/.cursor/mcp.json`. Hooks use direct command entries (unlike Codex's nested hook groups), so
//! the merge preserves Cursor's shape and every user-owned entry. The scripts live in
//! `~/.cursor/hooks/`, which is also Cursor's documented user-hook directory.

use super::hooks;
use super::{remove_empty_dir, remove_file};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const INDEX_STATUS: &str = "Indexing Cursor conversation into funes memory";
const PUSH_STATUS: &str = "Publishing funes memory";
const MCP_NAME: &str = "funes";

/// Install the global Cursor hooks and the stdio MCP server. Cursor watches hooks.json, so the
/// next agent session sees the hook without a separate CLI registration step.
pub fn install(memory: Option<String>) -> Result<()> {
    let root = cursor_home()?;
    let hooks_installed = install_hooks(&root, memory.as_deref())?;
    let mcp_installed = install_mcp(&root, memory.as_deref())?;

    if hooks_installed {
        let events = if memory.is_some() {
            "stop, sessionStart, sessionEnd"
        } else {
            "stop"
        };
        let what = if memory.is_some() {
            "indexes each agent loop and publishes at IDE session boundaries"
        } else {
            "indexes each agent loop (local only - pass a memory to also publish)"
        };
        println!(
            "installed funes hooks into {} ({events}) - {what}.",
            root.join("hooks.json").display()
        );
    }
    if mcp_installed {
        println!("installed funes recall into Cursor - recall/get are now available (restart Cursor if it's running).");
    }
    Ok(())
}

/// Remove only funes's Cursor hooks, scripts, and MCP entry. Other hooks and MCP servers remain.
pub fn uninstall() -> Result<()> {
    let root = cursor_home()?;
    let hooks = uninstall_hooks(&root);
    let mcp = uninstall_mcp(&root);
    match (hooks, mcp) {
        (Ok(()), Ok(())) => {
            println!("removed funes from Cursor - recall registration, hooks, and hook scripts.");
            Ok(())
        }
        (Err(hooks), Ok(())) => Err(hooks.context("Cursor MCP registration was removed")),
        (Ok(()), Err(mcp)) => Err(mcp.context("Cursor hooks and scripts were removed")),
        (Err(hooks), Err(mcp)) => Err(hooks.context(format!("Cursor MCP cleanup also failed: {mcp:#}"))),
    }
}

fn cursor_home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("resolving $HOME for the Cursor integration")?;
    Ok(PathBuf::from(home).join(".cursor"))
}

fn desired_hooks(root: &Path, memory: Option<&str>) -> Vec<hooks::Hook> {
    let hooks_dir = root.join("hooks");
    let index_script = hooks_dir.join("funes-index.sh").display().to_string();
    let mut desired = vec![hooks::Hook {
        event: "stop",
        command: hooks::command(&index_script, &["cursor"]),
        status: INDEX_STATUS,
    }];
    if let Some(memory) = memory {
        let push_script = hooks_dir.join("funes-push.sh").display().to_string();
        let command = hooks::command(&push_script, &[memory, "cursor"]);
        desired.push(hooks::Hook {
            event: "sessionStart",
            command: command.clone(),
            status: PUSH_STATUS,
        });
        desired.push(hooks::Hook {
            event: "sessionEnd",
            command,
            status: PUSH_STATUS,
        });
    }
    desired
}

fn install_hooks(root: &Path, memory: Option<&str>) -> Result<bool> {
    let hooks_dir = root.join("hooks");
    hooks::write_scripts(&hooks_dir)?;
    let desired = desired_hooks(root, memory);
    let config = root.join("hooks.json");

    let cfg = match std::fs::read_to_string(&config) {
        Ok(s) if !s.trim().is_empty() => match serde_json::from_str::<Value>(&s) {
            Ok(v) if v.is_object() && valid_hooks_shape(&v) => v,
            _ => {
                manual_hook_instructions(&config, &desired)?;
                return Ok(false);
            }
        },
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", config.display()))),
    };
    let out = hooks::apply_direct_hooks(cfg, &desired);
    atomic_write(&config, &format!("{}\n", serde_json::to_string_pretty(&out)?))?;
    Ok(true)
}

fn install_mcp(root: &Path, memory: Option<&str>) -> Result<bool> {
    let config = root.join("mcp.json");
    let cfg = match std::fs::read_to_string(&config) {
        Ok(s) if !s.trim().is_empty() => match serde_json::from_str::<Value>(&s) {
            Ok(v) if v.is_object() && valid_mcp_shape(&v) => v,
            _ => {
                manual_mcp_instructions(&config, memory)?;
                return Ok(false);
            }
        },
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", config.display()))),
    };
    let funes = std::env::var("FUNES_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "funes".to_string());
    let out = apply_mcp(cfg, &funes, memory);
    atomic_write(&config, &format!("{}\n", serde_json::to_string_pretty(&out)?))?;
    Ok(true)
}

fn valid_hooks_shape(cfg: &Value) -> bool {
    cfg.get("hooks")
        .map(|hooks| {
            hooks
                .as_object()
                .map(|map| map.values().all(Value::is_array))
                .unwrap_or(false)
        })
        .unwrap_or(true)
}

fn valid_mcp_shape(cfg: &Value) -> bool {
    cfg.get("mcpServers").map(Value::is_object).unwrap_or(true)
}

fn apply_mcp(mut cfg: Value, funes: &str, memory: Option<&str>) -> Value {
    let obj = cfg.as_object_mut().expect("cfg is a JSON object");
    if !obj.get("mcpServers").map(Value::is_object).unwrap_or(false) {
        obj.insert("mcpServers".to_string(), json!({}));
    }
    obj["mcpServers"]
        .as_object_mut()
        .expect("mcpServers is an object")
        .insert(MCP_NAME.to_string(), mcp_server(funes, memory));
    cfg
}

fn mcp_server(funes: &str, memory: Option<&str>) -> Value {
    let mut args = vec![json!("mcp")];
    if let Some(memory) = memory {
        args.push(json!(memory));
    }
    json!({
        "type": "stdio",
        "command": funes,
        "args": args,
    })
}

fn uninstall_hooks(root: &Path) -> Result<()> {
    let config = root.join("hooks.json");
    if let Some(current) = read_object(&config, "hooks")? {
        if !valid_hooks_shape(&current) {
            bail!(
                "{} has an invalid Cursor hooks shape - leaving it and hook scripts untouched; remove entries whose command contains funes-index.sh or funes-push.sh, then re-run funes remove cursor",
                config.display()
            );
        }
        let out = hooks::apply_direct_hooks(current.clone(), &[]);
        if out != current {
            atomic_write(&config, &format!("{}\n", serde_json::to_string_pretty(&out)?))?;
        }
    }

    let hooks_dir = root.join("hooks");
    for name in ["funes-index.sh", "funes-push.sh", "funes-sync.log"] {
        remove_file(&hooks_dir.join(name))?;
    }
    remove_empty_dir(&hooks_dir)?;
    Ok(())
}

fn uninstall_mcp(root: &Path) -> Result<()> {
    let config = root.join("mcp.json");
    let Some(mut current) = read_object(&config, "MCP")? else {
        return Ok(());
    };
    if !valid_mcp_shape(&current) {
        bail!(
            "{} has an invalid Cursor MCP shape - leaving it untouched; remove the funes entry from mcpServers, then re-run funes remove cursor",
            config.display()
        );
    }
    let obj = current.as_object_mut().expect("cfg is a JSON object");
    let removed = obj
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .map(|servers| servers.remove(MCP_NAME).is_some())
        .unwrap_or(false);
    if !removed {
        return Ok(());
    }
    if obj
        .get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(|servers| servers.is_empty())
    {
        obj.remove("mcpServers");
    }
    atomic_write(&config, &format!("{}\n", serde_json::to_string_pretty(&current)?))?;
    Ok(())
}

fn read_object(path: &Path, label: &str) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => {
            let value = serde_json::from_str::<Value>(&s)
                .with_context(|| format!("parsing {} to remove funes {label}", path.display()))?;
            if !value.is_object() {
                bail!("{} isn't a JSON object - leaving it untouched", path.display());
            }
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    }
}

fn manual_hook_instructions(path: &Path, desired: &[hooks::Hook]) -> Result<()> {
    let block = serde_json::to_string_pretty(&hooks::apply_direct_hooks(json!({}), desired))?;
    println!(
        "{} isn't plain Cursor hooks JSON - leaving it untouched. Merge this in to enable funes hooks:\n{block}",
        path.display()
    );
    Ok(())
}

fn manual_mcp_instructions(path: &Path, memory: Option<&str>) -> Result<()> {
    let funes = std::env::var("FUNES_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "funes".to_string());
    let block = serde_json::to_string_pretty(&json!({
        "mcpServers": { MCP_NAME: mcp_server(&funes, memory) }
    }))?;
    println!(
        "{} isn't plain Cursor MCP JSON - leaving it untouched. Merge this server in to enable funes recall:\n{block}",
        path.display()
    );
    Ok(())
}

/// Write a watched Cursor config through a same-directory temporary path, so Cursor never reads a
/// partially serialized document.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("resolving parent of {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = path.with_extension("funes-tmp");
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{apply_mcp, desired_hooks, mcp_server};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn cursor_hooks_use_stop_and_session_boundaries() {
        let local = desired_hooks(Path::new("/h/.cursor"), None);
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].event, "stop");
        assert!(local[0].command.contains("/.cursor/hooks/funes-index.sh"));

        let remote = desired_hooks(Path::new("/h/.cursor"), Some("acme/kb"));
        assert_eq!(remote.len(), 3);
        assert!(remote.iter().any(|hook| hook.event == "sessionStart"));
        assert!(remote.iter().any(|hook| hook.event == "sessionEnd"));
        assert!(remote
            .iter()
            .filter(|hook| hook.event != "stop")
            .all(|hook| hook.command.contains("acme/kb")));
    }

    #[test]
    fn mcp_server_bakes_the_memory_binding() {
        assert_eq!(
            mcp_server("funes", None),
            json!({"type":"stdio","command":"funes","args":["mcp"]})
        );
        assert_eq!(
            mcp_server("/bin/funes", Some("acme/kb")),
            json!({"type":"stdio","command":"/bin/funes","args":["mcp","acme/kb"]})
        );
    }

    #[test]
    fn mcp_merge_replaces_only_funes_and_preserves_other_servers() {
        let cfg = json!({
            "mcpServers": {
                "other": {"command":"other"},
                "funes": {"command":"/old/funes","args":["mcp"]}
            }
        });
        let out = apply_mcp(cfg, "/new/funes", Some("acme/kb"));
        assert_eq!(out["mcpServers"]["other"]["command"], "other");
        assert_eq!(out["mcpServers"]["funes"]["command"], "/new/funes");
        assert_eq!(out["mcpServers"]["funes"]["args"], json!(["mcp", "acme/kb"]));
    }
}
