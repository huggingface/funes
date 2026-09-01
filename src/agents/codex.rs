//! `funes add codex` / `funes remove codex`: manage recall and automation in Codex.
//!
//! Codex has a native MCP client, so funes is consumed as its stdio MCP server —
//! `codex mcp add funes -- funes mcp [memory]`. A non-local `memory` binds this agent's recall to it.
//! The command is `funes` from PATH (override with `FUNES_BIN`). Codex's `mcp add` always writes the
//! user config (`~/.codex/config.toml`); it has no project scope, and re-adding an existing server
//! overwrites it (idempotent).
//!
//! Codex lists a skill's name and description before any MCP tool is loaded, so funes installs one
//! under Codex's own `skills/` to be recognizable as memory while its tools are still deferred.
//!
//! Automation lives in Codex's dedicated `hooks.json`: a `Stop` hook indexes each
//! completed turn and, with a bound memory, `SessionEnd` publishes it — with `SessionStart`
//! catching up whatever a missed end left behind. funes merges only its own hook groups and
//! installs the scripts under Codex's `hooks/`. Every one of those paths hangs off the home
//! `codex doctor` reports, so a relocated `CODEX_HOME` is written to and read from alike.

use super::hooks;
use super::{remove_empty_dir, remove_file, run_remove, shell_command, RemoveCommand};
use crate::commands::update::parse_semver;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const SKILL_MD: &str = include_str!("../../integrations/codex/SKILL.md");

const INDEX_STATUS: &str = "Indexing turn into funes memory";
const PUSH_STATUS: &str = "Publishing funes memory";

/// The gate for publishing: below this the install is refused. `SessionEnd` exists from 0.145.0,
/// but this is the release the hook was verified to fire on, so it is the one enforced.
const MIN_CODEX: (u32, u32, u32) = (0, 151, 0);

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
    // Only publishing needs `SessionEnd`, and the check precedes every write.
    if memory.is_some() {
        if let Some(version) = codex_version()? {
            if matches!(parse_version_line(&version), Some(v) if v < MIN_CODEX) {
                let (major, minor, patch) = MIN_CODEX;
                bail!(
                    "codex {version} has no SessionEnd hook, so a bound memory would never publish — upgrade to {major}.{minor}.{patch} or later, or run `funes add codex` with no memory to index locally."
                );
            }
        }
    }

    // The skill and the hooks are plain files, so they land even when the MCP registration below
    // can't reach the Codex CLI.
    let codex_home = codex_home()?;
    install_skill(&codex_home)?;
    install_hooks(&codex_home, memory.as_deref())?;

    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let args = mcp_add_args(&funes, memory.as_deref());
    let manual = shell_command("codex", &args);
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
    let registration = run_remove(
        "codex",
        &["mcp", "remove", "funes"],
        &["No MCP server named 'funes' found"],
    );
    // Independent cleanups: a malformed hooks file must not strand the skill.
    let codex_home = codex_home()?;
    let files = match (uninstall_hooks(&codex_home), uninstall_skill(&codex_home)) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
        (Err(hooks), Err(skill)) => Err(hooks.context(format!("skill cleanup also failed: {skill:#}"))),
    };
    let outcome = match (registration, files) {
        (Ok(outcome), Ok(())) => outcome,
        (Err(registration), Ok(())) => {
            return Err(registration.context("the local Codex skill and hooks were removed"));
        }
        (Ok(_), Err(files)) => return Err(files),
        (Err(registration), Err(files)) => {
            return Err(registration.context(format!("Codex file cleanup also failed: {files:#}")));
        }
    };

    if outcome == RemoveCommand::MissingCli {
        println!("`codex` isn't on PATH — the skill and hooks were removed; once it is, run:  codex mcp remove funes");
    } else {
        println!("removed funes from Codex — recall registration, skill, hook entries, and hook scripts.");
    }
    Ok(())
}

/// Codex's own home — where its config, hooks, and skills live. Asked of Codex rather than assumed,
/// because Codex honors `CODEX_HOME` and resolves the user's home itself; `$HOME/.codex` is only
/// where a default install happens to land, and is the fallback when Codex can't be asked.
fn codex_home() -> Result<PathBuf> {
    if let Some(path) = doctor_codex_home() {
        return Ok(path);
    }
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("resolving $HOME for the Codex home")?;
    Ok(PathBuf::from(home).join(".codex"))
}

/// The home `codex doctor --json` reports. Its exit status is the health verdict, not whether it
/// answered, so the report is read whatever it says; anything unreadable yields `None` and leaves
/// the caller its fallback.
fn doctor_codex_home() -> Option<PathBuf> {
    let report = Command::new("codex").args(["doctor", "--json"]).output().ok()?;
    codex_home_from_report(&report.stdout)
}

/// The `CODEX_HOME` a doctor report carries, from the check that states it as a plain absolute path.
fn codex_home_from_report(report: &[u8]) -> Option<PathBuf> {
    let report: Value = serde_json::from_slice(report).ok()?;
    let home = report
        .get("checks")?
        .get("config.load")?
        .get("details")?
        .get("CODEX_HOME")?
        .as_str()?;
    (!home.is_empty()).then(|| PathBuf::from(home))
}

/// The funes-owned skill directory under Codex's home. Codex reads user skills from both
/// `<codex home>/skills` and `~/.agents/skills`; the funes skill goes in the Codex-private one,
/// because every harness implementing the skills standard reads the shared tree — a skill funes
/// installs for one agent and removes with it has no business being another agent's too.
fn skill_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("skills/funes")
}

/// The shared-tree copy an earlier install left behind, cleared on both install and uninstall so it
/// stops reaching the agents that read that tree. Best-effort: it is gone on every host but the ones
/// that ran that install.
fn remove_shared_skill() -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("resolving $HOME for the skills dir")?);
    let dir = home.join(".agents/skills/funes");
    remove_file(&dir.join("SKILL.md"))?;
    remove_empty_dir(&dir)?;
    for parent in dir.ancestors().skip(1).take(2) {
        remove_empty_dir(parent)?;
    }
    Ok(())
}

fn install_skill(codex_home: &Path) -> Result<()> {
    remove_shared_skill()?;
    let dir = skill_dir(codex_home);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, SKILL_MD).with_context(|| format!("writing {}", path.display()))?;
    println!("installed the funes skill into {}.", path.display());
    Ok(())
}

fn uninstall_skill(codex_home: &Path) -> Result<()> {
    remove_shared_skill()?;
    let dir = skill_dir(codex_home);
    remove_file(&dir.join("SKILL.md"))?;
    // Only `funes/` is funes's: `skills` is Codex's own root, and its parent Codex's home.
    remove_empty_dir(&dir)?;
    Ok(())
}

/// What Codex prints for `--version`. `None` when Codex is absent, which leaves the install to
/// proceed as the registration does.
fn codex_version() -> Result<Option<String>> {
    let out = match Command::new("codex").arg("--version").output() {
        Ok(o) if o.status.success() => o,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context("running `codex --version`")),
    };
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// Codex prints `codex-cli 0.151.0`, so the version is a later token, not the first.
fn parse_version_line(printed: &str) -> Option<(u32, u32, u32)> {
    printed.split_whitespace().find_map(parse_semver)
}

fn desired_hooks(hooks_dir: &Path, memory: Option<&str>) -> Vec<hooks::Hook> {
    let mut hooks = vec![hooks::Hook {
        event: "Stop",
        command: hooks::command(&hooks_dir.join("funes-index.sh").display().to_string(), &["codex"]),
        status: INDEX_STATUS,
    }];
    if let Some(memory) = memory {
        let command = hooks::command(
            &hooks_dir.join("funes-push.sh").display().to_string(),
            &[memory, "codex"],
        );
        hooks.push(hooks::Hook {
            event: "SessionEnd",
            command: command.clone(),
            status: PUSH_STATUS,
        });
        hooks.push(hooks::Hook {
            event: "SessionStart",
            command,
            status: PUSH_STATUS,
        });
    }
    hooks
}

/// Write the scripts and merge funes's groups into Codex's dedicated hooks file, preserving every
/// hand-authored group.
fn install_hooks(codex_home: &Path, memory: Option<&str>) -> Result<()> {
    let base = codex_home.to_path_buf();
    let hooks_dir = base.join("hooks");
    hooks::write_scripts(&hooks_dir)?;
    let desired = desired_hooks(&hooks_dir, memory);

    let config = base.join("hooks.json");
    let cfg = match std::fs::read_to_string(&config).ok().as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => match serde_json::from_str::<Value>(s) {
            Ok(v) if v.is_object() => v,
            _ => return manual_hook_instructions(&config, &desired),
        },
        _ => json!({}),
    };
    let out = hooks::apply_funes_hooks(cfg, &desired);
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
    // Codex skips a hook until its exact command is trusted, so an install that says nothing here
    // reads as automation that silently never runs.
    println!("run `/hooks` in Codex to review and trust them — until then Codex skips them.");
    Ok(())
}

fn manual_hook_instructions(path: &Path, desired: &[hooks::Hook]) -> Result<()> {
    let block = serde_json::to_string_pretty(&hooks::apply_funes_hooks(json!({}), desired))?;
    println!(
        "{} isn't plain JSON — leaving it untouched. Merge this in to enable funes hooks:\n{block}",
        path.display()
    );
    Ok(())
}

/// Remove only funes's groups from Codex's shared hooks file, then delete its scripts and log. An
/// absent setup is already removed; a malformed hooks file is left wholly untouched.
fn uninstall_hooks(codex_home: &Path) -> Result<()> {
    let base = codex_home.to_path_buf();
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
        let out = hooks::apply_funes_hooks(current.clone(), &[]);
        if out != current {
            std::fs::write(&config, format!("{}\n", serde_json::to_string_pretty(&out)?))
                .with_context(|| format!("writing {}", config.display()))?;
        }
    }

    let hooks_dir = base.join("hooks");
    for name in ["funes-index.sh", "funes-push.sh", "funes-sync.log"] {
        remove_file(&hooks_dir.join(name))?;
    }
    remove_empty_dir(&hooks_dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{codex_home_from_report, desired_hooks, mcp_add_args, parse_version_line, MIN_CODEX, SKILL_MD};
    use std::path::Path;
    use std::path::PathBuf;

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
    fn codex_home_comes_from_the_doctor_report() {
        let report = br#"{"schemaVersion":1,"overallStatus":"ok","checks":{
            "config.load":{"details":{"CODEX_HOME":"/elsewhere/codex","config.toml":"x"}},
            "state.paths":{"details":{"CODEX_HOME":"~/.codex (dir)"}}}}"#;
        assert_eq!(codex_home_from_report(report), Some(PathBuf::from("/elsewhere/codex")));

        // A report that carries no home — a failed run, an older schema — leaves the caller its
        // fallback rather than a guess.
        assert_eq!(
            codex_home_from_report(br#"{"checks":{"config.load":{"details":{}}}}"#),
            None
        );
        assert_eq!(codex_home_from_report(br#"{"checks":{}}"#), None);
        assert_eq!(codex_home_from_report(b"not json"), None);
        assert_eq!(codex_home_from_report(b""), None);
    }

    #[test]
    fn the_embedded_skill_declares_itself_to_codex() {
        let front = SKILL_MD.split("---").nth(1).expect("frontmatter");
        assert!(front.contains("name: funes"), "{front}");
        assert!(front.contains("description:"), "{front}");
    }

    #[test]
    fn reads_the_version_out_of_codexs_own_line() {
        assert_eq!(parse_version_line("codex-cli 0.151.0"), Some((0, 151, 0)));
        assert!(parse_version_line("codex-cli 0.150.9").unwrap() < MIN_CODEX);
        assert_eq!(parse_version_line("codex-cli"), None);
    }

    #[test]
    fn hooks_use_codex_paths_and_available_events() {
        let local = desired_hooks(Path::new("/h/hooks"), None);
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].event, "Stop");
        assert!(local[0].command.contains("/h/hooks/funes-index.sh"));

        let remote = desired_hooks(Path::new("/h/hooks"), Some("acme/kb"));
        assert_eq!(remote.len(), 3);
        assert!(remote.iter().any(|hook| hook.event == "SessionEnd"));
        assert!(remote.iter().any(|hook| hook.event == "SessionStart"));
        assert!(remote
            .iter()
            .filter(|hook| hook.event != "Stop")
            .all(|hook| hook.command.contains("acme/kb")));
    }
}
