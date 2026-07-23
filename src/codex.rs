//! `funes add codex` / `funes remove codex`: manage recall and automation in Codex.
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

use anyhow::{bail, Context, Result};
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
    let manual = crate::integration::shell_command("codex", &args);
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

/// Reverse [`install`] without touching the memory. MCP unregistering and hook cleanup are both
/// attempted, so a malformed hooks file cannot leave recall registered.
pub fn uninstall() -> Result<()> {
    let registration = crate::integration::run_remove(
        "codex",
        &["mcp", "remove", "funes"],
        &["No MCP server named 'funes' found"],
    );
    let hooks = uninstall_hooks();
    let outcome = match (registration, hooks) {
        (Ok(outcome), Ok(())) => outcome,
        (Err(registration), Ok(())) => {
            return Err(registration.context("local Codex hooks were removed"));
        }
        (Ok(_), Err(hooks)) => return Err(hooks),
        (Err(registration), Err(hooks)) => {
            return Err(registration.context(format!("Codex hook cleanup also failed: {hooks:#}")));
        }
    };

    if outcome == crate::integration::RemoveCommand::MissingCli {
        println!("`codex` isn't on PATH — hooks were removed; once it is, run:  codex mcp remove funes");
    } else {
        println!("removed funes from Codex — recall registration, hook entries, and hook scripts.");
    }
    Ok(())
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

/// Remove only funes's groups from Codex's shared hooks file, then delete its scripts and log. An
/// absent setup is already removed; a malformed hooks file is left wholly untouched.
fn uninstall_hooks() -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("resolving $HOME for the hooks dir")?);
    let base = home.join(".codex");
    let config = base.join("hooks.json");

    let current = match std::fs::read_to_string(&config) {
        Ok(s) if !s.trim().is_empty() => {
            let value = serde_json::from_str::<Value>(&s)
                .with_context(|| format!("parsing {} to remove funes hooks", config.display()))?;
            if !value.is_object() {
                bail!(
                    "{} isn't a JSON object — leaving it and the hook scripts untouched; remove hook groups whose command contains `funes-index.sh` or `funes-push.sh`, then re-run `funes remove codex`",
                    config.display()
                );
            }
            Some(value)
        }
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", config.display()))),
    };
    if let Some(current) = current {
        let out = crate::hooks::apply_funes_hooks(current.clone(), &[]);
        if out != current {
            std::fs::write(&config, format!("{}\n", serde_json::to_string_pretty(&out)?))
                .with_context(|| format!("writing {}", config.display()))?;
        }
    }

    let hooks_dir = base.join("hooks");
    for name in ["funes-index.sh", "funes-push.sh", "funes-sync.log"] {
        crate::integration::remove_file(&hooks_dir.join(name))?;
    }
    crate::integration::remove_empty_dir(&hooks_dir)?;
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
