//! `funes add claude` / `funes remove claude`: manage recall and automation in Claude Code.
//!
//! Claude Code has a native MCP client, so funes is consumed as its stdio MCP server —
//! `claude mcp add funes -s user -- funes mcp [memory]`, registered at `user` scope so recall is
//! available across all your projects. `memory` (when not local) binds this agent's recall to that
//! memory. The command is `funes` from PATH (override with `FUNES_BIN`).
//!
//! Automation is a hooks-only Claude plugin extracted to
//! `~/.funes/integrations/claude-plugin` and registered through Claude's plugin CLI. Its own
//! `hooks/hooks.json` indexes each completed turn and, with a bound memory, publishes at session
//! start and end. funes never edits the user's `settings.json`.

use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MARKETPLACE_JSON: &str = include_str!("../integrations/claude-plugin/.claude-plugin/marketplace.json");
const PLUGIN_JSON: &str = include_str!("../integrations/claude-plugin/funes/.claude-plugin/plugin.json");
const PLUGIN_ID: &str = "funes@huggingface";
const INDEX_STATUS: &str = "Indexing turn into funes memory";
const PUSH_STATUS: &str = "Publishing funes memory";

/// The `claude mcp add` argument vector registering `funes mcp [memory]` at user scope. A non-local
/// `memory` is appended as `funes mcp <memory>`, pinning this agent's recall to it.
fn mcp_add_args(funes: &str, memory: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["mcp", "add", "funes", "-s", "user", "--", funes, "mcp"]
        .into_iter()
        .map(String::from)
        .collect();
    if let Some(s) = memory {
        args.push(s.to_string());
    }
    args
}

pub fn install(memory: Option<String>) -> Result<()> {
    // Extract/register automation first. The files land even when `claude` isn't on PATH, and the
    // registration helper prints the exact manual command in that case.
    install_hooks(memory.as_deref())?;

    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let args = mcp_add_args(&funes, memory.as_deref());
    let manual = format!("claude {}", args.join(" "));

    // `claude mcp add` errors if `funes` is already registered, so a re-run — e.g. to change the
    // memory — would fail. Remove any existing registration first (silenced and ignored: it errors
    // when absent), so add always succeeds and picks up the current memory. Skipped when `claude`
    // isn't on PATH — the add below handles that with a manual hint.
    let _ = Command::new("claude")
        .args(["mcp", "remove", "funes"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let status = match Command::new("claude").args(&args).status() {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`claude` isn't on PATH — once it is, run:  {manual}");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `claude mcp add`")),
    };

    if status.success() {
        println!("installed funes recall into Claude Code (user scope) — `recall`/`get` are now available (restart Claude Code if it's running).");
        Ok(())
    } else {
        anyhow::bail!(
            "`claude mcp add funes` failed (exit {:?}) — run `{manual}` manually to see why.",
            status.code()
        );
    }
}

/// Reverse [`install`] without touching the memory: remove both user-scoped registrations and the
/// extracted hooks plugin source.
pub fn uninstall() -> Result<()> {
    use crate::integration::RemoveCommand;

    let registrations = uninstall_registrations();
    let files = uninstall_hooks();
    let outcome = match (registrations, files) {
        (Ok(outcome), Ok(())) => outcome,
        (Err(registration), Ok(())) => {
            return Err(registration.context("local Claude integration files were removed"));
        }
        (Ok(_), Err(files)) => return Err(files),
        (Err(registration), Err(files)) => {
            return Err(registration.context(format!("local Claude cleanup also failed: {files:#}")));
        }
    };

    if outcome == RemoveCommand::MissingCli {
        println!(
            "`claude` isn't on PATH — extracted integration files were removed. Once it is, remove the registrations manually:\n{}",
            remove_instructions()
        );
    } else {
        println!(
            "removed funes from Claude Code — recall registration, hooks plugin, and extracted integration files."
        );
    }
    Ok(())
}

fn remove_instructions() -> &'static str {
    "  claude mcp remove funes -s user\n  \
     claude plugin uninstall funes@huggingface -s user\n  \
     claude plugin marketplace remove huggingface --scope user"
}

fn uninstall_registrations() -> Result<crate::integration::RemoveCommand> {
    use crate::integration::{run_remove, RemoveCommand};

    let outcome = run_remove(
        "claude",
        &["mcp", "remove", "funes", "-s", "user"],
        &["No MCP server named \"funes\""],
    )?;
    if outcome == RemoveCommand::MissingCli {
        return Ok(outcome);
    }
    run_remove(
        "claude",
        &["plugin", "uninstall", PLUGIN_ID, "-s", "user"],
        &["Plugin \"funes@huggingface\" not found"],
    )?;
    run_remove(
        "claude",
        &["plugin", "marketplace", "remove", "huggingface", "--scope", "user"],
        &["Marketplace 'huggingface' not found"],
    )?;
    Ok(outcome)
}

fn uninstall_hooks() -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("resolving $HOME for the plugin dir")?);
    let root = home.join(".funes/integrations/claude-plugin");
    crate::integration::remove_tree(&root)?;
    if let Some(parent) = root.parent() {
        crate::integration::remove_empty_dir(parent)?;
    }
    Ok(())
}

fn desired_hooks(memory: Option<&str>) -> Vec<crate::hooks::Hook> {
    let mut hooks = vec![crate::hooks::Hook {
        event: "Stop",
        command: crate::hooks::command("${CLAUDE_PLUGIN_ROOT}/scripts/funes-index.sh", "claude"),
        status: INDEX_STATUS,
    }];
    if let Some(memory) = memory {
        let command = crate::hooks::command("${CLAUDE_PLUGIN_ROOT}/scripts/funes-push.sh", memory);
        hooks.push(crate::hooks::Hook {
            event: "SessionStart",
            command: command.clone(),
            status: PUSH_STATUS,
        });
        hooks.push(crate::hooks::Hook {
            event: "SessionEnd",
            command,
            status: PUSH_STATUS,
        });
    }
    hooks
}

/// Extract the hooks-only plugin and register it with Claude. The fixed source path must outlive a
/// session because Claude records the marketplace by reference.
fn install_hooks(memory: Option<&str>) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("resolving $HOME for the plugin dir")?);
    let root = home.join(".funes/integrations/claude-plugin");
    let plugin = root.join("funes");
    let hooks_json = format!(
        "{}\n",
        serde_json::to_string_pretty(&crate::hooks::apply_funes_hooks(json!({}), &desired_hooks(memory),))?
    );

    let mut dirty = false;
    dirty |= crate::hooks::write_if_changed(&root.join(".claude-plugin/marketplace.json"), MARKETPLACE_JSON)?;
    dirty |= crate::hooks::write_if_changed(&plugin.join(".claude-plugin/plugin.json"), PLUGIN_JSON)?;
    dirty |= crate::hooks::write_if_changed(&plugin.join("hooks/hooks.json"), &hooks_json)?;
    dirty |= crate::hooks::write_scripts(&plugin.join("scripts"))?;

    register_hooks(&root, memory.is_some(), dirty)
}

/// Register or refresh the extracted plugin. Claude's plain install is a no-op for an installed
/// plugin and update is version-gated, so changed content is refreshed by uninstalling first.
fn register_hooks(root: &Path, has_memory: bool, dirty: bool) -> Result<()> {
    let root_str = root.display().to_string();
    let manual = format!("  claude plugin marketplace add \"{root_str}\"\n  claude plugin install {PLUGIN_ID}");
    match Command::new("claude").args(["plugin", "marketplace", "add", &root_str]).status() {
        Ok(s) if s.success() => {}
        Ok(s) => bail!(
            "`claude plugin marketplace add` failed (exit {:?}) — the plugin is at {root_str}; register it manually:\n{manual}",
            s.code()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("extracted the funes hooks plugin to {root_str}");
            println!("`claude` isn't on PATH — once it is, run:\n{manual}");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `claude plugin marketplace add`")),
    }
    if dirty {
        let _ = Command::new("claude")
            .args(["plugin", "uninstall", PLUGIN_ID])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    match Command::new("claude").args(["plugin", "install", PLUGIN_ID]).status() {
        Ok(s) if s.success() => {
            let what = if has_memory {
                "indexes each turn and publishes at session boundaries"
            } else {
                "indexes each turn (local only — pass a memory to also publish)"
            };
            println!("installed the funes hooks plugin into Claude Code — {what} (restart Claude Code if it's running).");
            Ok(())
        }
        Ok(s) => bail!(
            "`claude plugin install {PLUGIN_ID}` failed (exit {:?}); the plugin is at {root_str} — run `claude plugin install {PLUGIN_ID}` manually.",
            s.code()
        ),
        Err(e) => Err(anyhow::Error::new(e).context("running `claude plugin install`")),
    }
}

#[cfg(test)]
mod tests {
    use super::{desired_hooks, mcp_add_args};

    #[test]
    fn bakes_the_memory_only_when_present() {
        assert_eq!(
            mcp_add_args("funes", None),
            ["mcp", "add", "funes", "-s", "user", "--", "funes", "mcp"]
        );
        assert_eq!(
            mcp_add_args("funes", Some("acme/kb")),
            ["mcp", "add", "funes", "-s", "user", "--", "funes", "mcp", "acme/kb"]
        );
    }

    #[test]
    fn hooks_use_claudes_plugin_paths_and_session_events() {
        let local = desired_hooks(None);
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].event, "Stop");
        assert!(local[0]
            .command
            .contains("${CLAUDE_PLUGIN_ROOT}/scripts/funes-index.sh"));

        let remote = desired_hooks(Some("acme/kb"));
        assert_eq!(remote.len(), 3);
        assert!(remote.iter().any(|hook| hook.event == "SessionStart"));
        assert!(remote.iter().any(|hook| hook.event == "SessionEnd"));
        assert!(remote
            .iter()
            .filter(|hook| hook.event != "Stop")
            .all(|hook| hook.command.contains("acme/kb")));
    }
}
