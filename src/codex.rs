//! `funes add codex`: register recall and automation with Codex.
//!
//! Codex has a native MCP client, so funes is consumed as its stdio MCP server —
//! `codex mcp add funes -- funes mcp [memory]`. A non-local `memory` binds this agent's recall to it.
//! The command is `funes` from PATH (override with `FUNES_BIN`). Codex's `mcp add` always writes the
//! user config (`~/.codex/config.toml`); it has no project scope, and re-adding an existing server
//! overwrites it (idempotent).
//!
//! Automation lives in Codex's dedicated `~/.codex/hooks.json`: a `Stop` hook indexes each
//! completed turn and, with a bound memory, `SessionStart` publishes it. funes merges only its own
//! hook groups and installs the scripts under `~/.codex/hooks/`.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const INDEX_STATUS: &str = "Indexing turn into funes memory";
const PUSH_STATUS: &str = "Publishing funes memory";

/// The `codex mcp add` argument vector registering `funes mcp [memory]`. A non-local `memory` is
/// appended as `funes mcp <memory>`, pinning this agent's recall to it.
fn mcp_add_args(funes: &str, memory: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["mcp", "add", "funes", "--", funes, "mcp"]
        .into_iter()
        .map(String::from)
        .collect();
    if let Some(s) = memory {
        args.push(s.to_string());
    }
    args
}

pub fn install(memory: Option<String>) -> Result<()> {
    // Hooks are files + a hooks.json edit, so they land even when the MCP registration below can't
    // reach the Codex CLI.
    install_hooks(memory.as_deref())?;

    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let args = mcp_add_args(&funes, memory.as_deref());
    let manual = format!("codex {}", args.join(" "));
    let status = match Command::new("codex").args(&args).status() {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`codex` isn't on PATH — once it is, run:  {manual}");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `codex mcp add`")),
    };

    if status.success() {
        println!(
            "installed funes recall into Codex — `recall`/`get` are now available (restart Codex if it's running)."
        );
        Ok(())
    } else {
        anyhow::bail!(
            "`codex mcp add funes` failed (exit {:?}); run `{manual}` manually to see why.",
            status.code()
        );
    }
}

fn desired_hooks(hooks_dir: &Path, memory: Option<&str>) -> Vec<crate::hooks::Hook> {
    let mut hooks = vec![crate::hooks::Hook {
        event: "Stop",
        command: crate::hooks::command(&hooks_dir.join("funes-index.sh").display().to_string(), "codex"),
        status: INDEX_STATUS,
    }];
    if let Some(memory) = memory {
        hooks.push(crate::hooks::Hook {
            event: "SessionStart",
            command: crate::hooks::command(&hooks_dir.join("funes-push.sh").display().to_string(), memory),
            status: PUSH_STATUS,
        });
    }
    hooks
}

/// Write the scripts and merge funes's groups into Codex's dedicated hooks file, preserving every
/// hand-authored group.
fn install_hooks(memory: Option<&str>) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("resolving $HOME for the hooks dir")?);
    let base = home.join(".codex");
    let hooks_dir = base.join("hooks");
    crate::hooks::write_scripts(&hooks_dir)?;
    let desired = desired_hooks(&hooks_dir, memory);

    let config = base.join("hooks.json");
    let cfg = match std::fs::read_to_string(&config).ok().as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => match serde_json::from_str::<Value>(s) {
            Ok(v) if v.is_object() => v,
            _ => return manual_hook_instructions(&config, &desired),
        },
        _ => json!({}),
    };
    let out = crate::hooks::apply_funes_hooks(cfg, &desired);
    if let Some(dir) = config.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&config, format!("{}\n", serde_json::to_string_pretty(&out)?))
        .with_context(|| format!("writing {}", config.display()))?;

    let events: Vec<&str> = desired.iter().map(|hook| hook.event).collect();
    let what = if memory.is_some() {
        "indexes each turn and publishes at session boundaries"
    } else {
        "indexes each turn (local only — pass a memory to also publish)"
    };
    println!(
        "installed funes hooks into {} ({}) — {what}.",
        config.display(),
        events.join(", ")
    );
    Ok(())
}

fn manual_hook_instructions(path: &Path, desired: &[crate::hooks::Hook]) -> Result<()> {
    let block = serde_json::to_string_pretty(&crate::hooks::apply_funes_hooks(json!({}), desired))?;
    println!(
        "{} isn't plain JSON — leaving it untouched. Merge this in to enable funes hooks:\n{block}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{desired_hooks, mcp_add_args};
    use std::path::Path;

    #[test]
    fn bakes_the_memory_only_when_present() {
        assert_eq!(
            mcp_add_args("funes", None),
            ["mcp", "add", "funes", "--", "funes", "mcp"]
        );
        assert_eq!(
            mcp_add_args("funes", Some("acme/kb")),
            ["mcp", "add", "funes", "--", "funes", "mcp", "acme/kb"]
        );
    }

    #[test]
    fn hooks_use_codex_paths_and_available_events() {
        let local = desired_hooks(Path::new("/h/hooks"), None);
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].event, "Stop");
        assert!(local[0].command.contains("/h/hooks/funes-index.sh"));

        let remote = desired_hooks(Path::new("/h/hooks"), Some("acme/kb"));
        assert_eq!(remote.len(), 2);
        assert!(remote.iter().any(|hook| hook.event == "SessionStart"));
        assert!(!remote.iter().any(|hook| hook.event == "SessionEnd"));
        assert!(remote[1].command.contains("acme/kb"));
    }
}
