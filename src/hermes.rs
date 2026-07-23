//! `funes add hermes` / `funes remove hermes`: manage recall and automation in hermes.
//!
//! hermes has two user-scoped integration surfaces under `~/.hermes`:
//! - **Recall.** hermes has a native MCP client, so funes registers as its stdio MCP server —
//!   `hermes mcp add funes --command funes --args mcp [memory]`. hermes' `--args` is variadic, so a
//!   non-local `memory` rides along as an extra token, binding this agent's recall to it.
//! - **Automation.** hermes fires shell hooks declared in `config.yaml`. funes installs a per-turn
//!   index hook (`post_llm_call`, fired once per completed turn) and, with a bound memory, publish
//!   hooks (`on_session_start` to catch up + `on_session_finalize` at the true session boundary),
//!   driving the same detached scripts Claude/Codex use. hermes gates shell hooks behind a consent
//!   allowlist; funes pre-writes its own `(event, command)` approvals so they run from the first
//!   turn.
//!
//! Unlike Claude's plugin / Codex's `hooks.json` (both JSON), hermes' hooks live in `config.yaml`,
//! so the merge round-trips YAML. Comments aren't preserved — hermes' own `memory setup` rewrites
//! the file the same way. Both files are written atomically (temp + rename) since hermes may be
//! running.

use anyhow::{Context, Result};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Per-hook timeout (seconds). The scripts detach immediately and return in well under a second, so
/// this only bounds the fast foreground handoff.
const TIMEOUT: u64 = 30;

pub fn install(memory: Option<String>) -> Result<()> {
    // Automation first: writing scripts + config + allowlist needs no `hermes` binary, so it lands
    // even if the MCP registration below can't reach the CLI (mirrors codex).
    install_hooks(memory.as_deref())?;
    register_recall(memory.as_deref())
}

/// Reverse [`install`]: remove only funes's hooks and approvals, delete its hook scripts/log, and
/// unregister the MCP server. The memory and Hermes session database are deliberately untouched.
pub fn uninstall() -> Result<()> {
    // These surfaces are independent: attempt MCP removal before propagating any malformed hook
    // config error, and still attempt hook cleanup if the CLI removal itself fails.
    let registration = crate::integration::run_remove(
        "hermes",
        &["mcp", "remove", "funes"],
        &["Server 'funes' not found in config"],
    );
    let hooks = uninstall_hooks();
    let outcome = match (registration, hooks) {
        (Ok(outcome), Ok(())) => outcome,
        (Err(registration), Ok(())) => {
            return Err(registration.context("local Hermes hooks were removed"));
        }
        (Ok(_), Err(hooks)) => return Err(hooks),
        (Err(registration), Err(hooks)) => {
            return Err(registration.context(format!("Hermes hook cleanup also failed: {hooks:#}")));
        }
    };

    if outcome == crate::integration::RemoveCommand::MissingCli {
        println!("`hermes` isn't on PATH — hooks were removed; once it is, run:  hermes mcp remove funes");
    } else {
        println!("removed funes from hermes — recall registration, hook entries, approvals, and hook scripts.");
    }
    Ok(())
}

// ---- automation: config.yaml hooks + the consent allowlist ----

/// The funes-owned hooks: a per-turn index (always) and, with a bound `memory`, a publish on session
/// start (catch-up) and finalize (the true boundary). Each is an `(event, command)` pair; the
/// command drives a detached script and is the exact string the allowlist must match.
fn desired(hooks_dir: &Path, memory: Option<&str>) -> Vec<(&'static str, String)> {
    let index_script = hooks_dir.join("funes-index.sh").display().to_string();
    let mut out = vec![("post_llm_call", crate::hooks::command(&index_script, "hermes"))];
    if let Some(s) = memory {
        let push = crate::hooks::command(&hooks_dir.join("funes-push.sh").display().to_string(), s);
        out.push(("on_session_start", push.clone()));
        out.push(("on_session_finalize", push));
    }
    out
}

fn install_hooks(memory: Option<&str>) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("resolving $HOME for the hermes hooks dir")?);
    let hermes = home.join(".hermes");
    let hooks_dir = hermes.join("hooks");
    crate::hooks::write_scripts(&hooks_dir)?;

    let entries = desired(&hooks_dir, memory);
    let config = hermes.join("config.yaml");
    let allowlist = hermes.join("shell-hooks-allowlist.json");
    let wrote_config = write_config_hooks(&config, &entries)?;
    let wrote_allowlist = write_allowlist(&allowlist, &entries)?;

    if wrote_config && wrote_allowlist {
        let what = if memory.is_some() {
            "indexes each turn and publishes at session boundaries"
        } else {
            "indexes each turn (local only — pass a memory to also publish)"
        };
        let events: Vec<&str> = entries.iter().map(|(e, _)| *e).collect();
        println!(
            "installed funes hooks into {} ({}) — {what}. Hermes indexing is beta.",
            config.display(),
            events.join(", ")
        );
    } else {
        manual_hook_instructions(&config, &allowlist, &entries, wrote_config, wrote_allowlist);
    }
    Ok(())
}

/// Parse both shared files before changing either, remove only funes-owned entries, then delete the
/// scripts/log. This avoids a half-edited local integration when one file is malformed.
fn uninstall_hooks() -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("resolving $HOME for the hermes hooks dir")?);
    let hermes = home.join(".hermes");
    let config = hermes.join("config.yaml");
    let allowlist = hermes.join("shell-hooks-allowlist.json");

    let config_doc = match std::fs::read_to_string(&config) {
        Ok(s) if !s.trim().is_empty() => {
            let doc = serde_yaml::from_str::<serde_yaml::Value>(&s)
                .with_context(|| format!("parsing {} to remove funes hooks", config.display()))?;
            if !doc.is_mapping() {
                anyhow::bail!(
                    "{} isn't a YAML mapping — leaving the Hermes integration untouched; remove hooks whose command contains `funes-index.sh` or `funes-push.sh`, then re-run `funes remove hermes`",
                    config.display()
                );
            }
            Some(doc)
        }
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", config.display()))),
    };
    let allowlist_doc = match std::fs::read_to_string(&allowlist) {
        Ok(s) if !s.trim().is_empty() => {
            let doc = serde_json::from_str::<serde_json::Value>(&s)
                .with_context(|| format!("parsing {} to remove funes approvals", allowlist.display()))?;
            if !doc.is_object() {
                anyhow::bail!(
                    "{} isn't a JSON object — leaving the Hermes integration untouched; remove approvals whose command contains `funes-index.sh` or `funes-push.sh`, then re-run `funes remove hermes`",
                    allowlist.display()
                );
            }
            Some(doc)
        }
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", allowlist.display()))),
    };

    if let Some(current) = config_doc {
        let out = apply_config_hooks(current.clone(), &[]);
        if out != current {
            atomic_write(
                &config,
                &serde_yaml::to_string(&out).context("serializing ~/.hermes/config.yaml")?,
            )?;
        }
    }
    if let Some(current) = allowlist_doc {
        let out = apply_allowlist(current.clone(), &[]);
        if out != current {
            atomic_write(&allowlist, &format!("{}\n", serde_json::to_string_pretty(&out)?))?;
        }
    }

    let hooks_dir = hermes.join("hooks");
    for name in ["funes-index.sh", "funes-push.sh", "funes-sync.log"] {
        crate::integration::remove_file(&hooks_dir.join(name))?;
    }
    crate::integration::remove_empty_dir(&hooks_dir)?;
    Ok(())
}

/// Merge the funes hooks into `~/.hermes/config.yaml`'s `hooks:` block. Returns `false` (writing
/// nothing) when an existing file isn't a plain YAML mapping — funes won't clobber a config it can't
/// safely round-trip; the caller then prints the block to add by hand.
fn write_config_hooks(config: &Path, entries: &[(&'static str, String)]) -> Result<bool> {
    let doc = match std::fs::read_to_string(config).ok().as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => match serde_yaml::from_str::<serde_yaml::Value>(s) {
            Ok(v) if v.is_mapping() => v,
            _ => return Ok(false),
        },
        _ => serde_yaml::Value::Mapping(Default::default()),
    };
    let out = apply_config_hooks(doc, entries);
    let yaml = serde_yaml::to_string(&out).context("serializing ~/.hermes/config.yaml")?;
    atomic_write(config, &yaml)?;
    Ok(true)
}

/// Merge the funes approvals into `~/.hermes/shell-hooks-allowlist.json`. Returns `false` when an
/// existing file isn't a JSON object (leaving it untouched).
fn write_allowlist(path: &Path, entries: &[(&'static str, String)]) -> Result<bool> {
    let doc = match std::fs::read_to_string(path).ok().as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) if v.is_object() => v,
            _ => return Ok(false),
        },
        _ => json!({}),
    };
    let out = apply_allowlist(doc, entries);
    atomic_write(path, &format!("{}\n", serde_json::to_string_pretty(&out)?))?;
    Ok(true)
}

/// Remove every funes hook from `doc`'s `hooks:` map (across all events), then add `entries` —
/// re-running replaces funes's hooks, never duplicating them, and leaves every non-funes hook and
/// every other config key untouched. Empty event lists are pruned (so a local re-run after a remote
/// one drops the publish events).
fn apply_config_hooks(mut doc: serde_yaml::Value, entries: &[(&'static str, String)]) -> serde_yaml::Value {
    let map = doc.as_mapping_mut().expect("config is a mapping");
    let hooks_key = serde_yaml::Value::from("hooks");
    if !map.get(&hooks_key).map(serde_yaml::Value::is_mapping).unwrap_or(false) {
        if entries.is_empty() {
            return doc;
        }
        map.insert(hooks_key.clone(), serde_yaml::Value::Mapping(Default::default()));
    }
    let hooks = map.get_mut(&hooks_key).unwrap().as_mapping_mut().unwrap();

    for (_event, list) in hooks.iter_mut() {
        if let Some(seq) = list.as_sequence_mut() {
            seq.retain(|e| !is_funes_entry(e));
        }
    }
    for (event, command) in entries {
        let key = serde_yaml::Value::from(*event);
        if !hooks.get(&key).map(serde_yaml::Value::is_sequence).unwrap_or(false) {
            hooks.insert(key.clone(), serde_yaml::Value::Sequence(Vec::new()));
        }
        hooks
            .get_mut(&key)
            .unwrap()
            .as_sequence_mut()
            .unwrap()
            .push(hook_entry(command));
    }
    let empty: Vec<serde_yaml::Value> = hooks
        .iter()
        .filter(|(_, v)| v.as_sequence().map(|s| s.is_empty()).unwrap_or(false))
        .map(|(k, _)| k.clone())
        .collect();
    for k in empty {
        hooks.remove(&k);
    }
    doc
}

/// A `{command, timeout}` hook entry — hermes' per-event list element.
fn hook_entry(command: &str) -> serde_yaml::Value {
    let mut m = serde_yaml::Mapping::new();
    m.insert("command".into(), command.into());
    m.insert("timeout".into(), TIMEOUT.into());
    serde_yaml::Value::Mapping(m)
}

/// Remove funes's approvals from `doc`'s `approvals` list, then add one per entry. The allowlist
/// matches on exact `(event, command)`, so these must carry the same command strings as the config.
fn apply_allowlist(mut doc: serde_json::Value, entries: &[(&'static str, String)]) -> serde_json::Value {
    let obj = doc.as_object_mut().expect("allowlist is an object");
    if !obj.get("approvals").map(serde_json::Value::is_array).unwrap_or(false) {
        if entries.is_empty() {
            return doc;
        }
        obj.insert("approvals".to_string(), json!([]));
    }
    let list = obj.get_mut("approvals").unwrap().as_array_mut().unwrap();
    list.retain(|a| !is_funes_approval(a));
    for (event, command) in entries {
        list.push(json!({ "event": event, "command": command }));
    }
    doc
}

/// A hook/approval is funes's if its command invokes a funes script.
fn is_funes_command(command: Option<&str>) -> bool {
    command
        .map(|c| c.contains("funes-index.sh") || c.contains("funes-push.sh"))
        .unwrap_or(false)
}

fn is_funes_entry(entry: &serde_yaml::Value) -> bool {
    is_funes_command(entry.get("command").and_then(serde_yaml::Value::as_str))
}

fn is_funes_approval(approval: &serde_json::Value) -> bool {
    is_funes_command(approval.get("command").and_then(serde_json::Value::as_str))
}

/// Write `content` to `path` via a temp file + rename, so a reader (a running hermes) never sees a
/// torn file. Creates parent dirs.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("funes-tmp");
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// When a file can't be safely round-tripped, print exactly what to add rather than clobber it.
fn manual_hook_instructions(
    config: &Path,
    allowlist: &Path,
    entries: &[(&'static str, String)],
    wrote_config: bool,
    wrote_allowlist: bool,
) {
    if !wrote_config {
        let block = serde_yaml::to_string(&apply_config_hooks(
            serde_yaml::Value::Mapping(Default::default()),
            entries,
        ))
        .unwrap_or_default();
        println!(
            "{} isn't a plain YAML mapping — leaving it untouched. Merge this in to enable funes hooks:\n{block}",
            config.display()
        );
    }
    if !wrote_allowlist {
        let block = serde_json::to_string_pretty(&apply_allowlist(json!({}), entries)).unwrap_or_default();
        println!(
            "{} isn't a JSON object — leaving it untouched. Merge this in to approve funes' hooks:\n{block}",
            allowlist.display()
        );
    }
}

// ---- recall: register funes as an MCP server ----

/// The `hermes mcp add` argument vector registering `funes mcp [memory]`. hermes' `--args` is
/// variadic, so a non-local `memory` is appended after `mcp` as another `--args` value.
fn mcp_add_args(funes: &str, memory: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["mcp", "add", "funes", "--command", funes, "--args", "mcp"]
        .into_iter()
        .map(String::from)
        .collect();
    if let Some(s) = memory {
        args.push(s.to_string());
    }
    args
}

fn register_recall(memory: Option<&str>) -> Result<()> {
    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let args = mcp_add_args(&funes, memory);
    let manual = crate::integration::shell_command("hermes", &args);
    let mut child = match Command::new("hermes").args(&args).stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`hermes` isn't on PATH — once it is, run:  {manual}");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `hermes mcp add`")),
    };

    // After probing the server, `mcp add` prompts "Enable all N tools?" on stdin and
    // cancels on EOF — feed it "y" so funes' tools are enabled non-interactively.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"y\ny\n");
    }

    let status = child.wait().context("waiting for `hermes mcp add`")?;
    if status.success() {
        println!("installed funes recall into hermes — `mcp_funes_recall`/`_get` are now available.");
        Ok(())
    } else {
        anyhow::bail!(
            "`hermes mcp add funes` failed (exit {:?}); run `{manual}` manually to see why.",
            status.code()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> serde_yaml::Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// The command carried by event `ev`'s first funes hook entry, if any.
    fn funes_cmd(doc: &serde_yaml::Value, ev: &str) -> Option<String> {
        doc.get("hooks")?
            .get(ev)?
            .as_sequence()?
            .iter()
            .find(|e| is_funes_entry(e))?
            .get("command")?
            .as_str()
            .map(str::to_string)
    }

    #[test]
    fn bakes_the_memory_only_when_present() {
        assert_eq!(
            mcp_add_args("funes", None),
            ["mcp", "add", "funes", "--command", "funes", "--args", "mcp"]
        );
        assert_eq!(
            mcp_add_args("funes", Some("acme/kb")),
            ["mcp", "add", "funes", "--command", "funes", "--args", "mcp", "acme/kb"]
        );
    }

    #[test]
    fn adds_hooks_to_a_fresh_config() {
        let entries = desired(Path::new("/h/hooks"), Some("acme/kb"));
        let out = apply_config_hooks(serde_yaml::Value::Mapping(Default::default()), &entries);
        assert_eq!(
            funes_cmd(&out, "post_llm_call").as_deref(),
            Some("bash \"/h/hooks/funes-index.sh\" \"hermes\"")
        );
        assert_eq!(
            funes_cmd(&out, "on_session_finalize").as_deref(),
            Some("bash \"/h/hooks/funes-push.sh\" \"acme/kb\"")
        );
        assert!(funes_cmd(&out, "on_session_start").is_some());
    }

    #[test]
    fn local_has_no_publish_hooks() {
        let entries = desired(Path::new("/h/hooks"), None);
        let out = apply_config_hooks(serde_yaml::Value::Mapping(Default::default()), &entries);
        assert!(funes_cmd(&out, "post_llm_call").is_some());
        assert!(out.get("hooks").unwrap().get("on_session_finalize").is_none());
        assert!(out.get("hooks").unwrap().get("on_session_start").is_none());
    }

    #[test]
    fn replaces_funes_hooks_and_preserves_user_config() {
        // A config with an unrelated key, a user hook on the same event, and a stale funes hook.
        let doc = cfg("model: hermes-4\n\
             hooks:\n  \
               post_llm_call:\n    \
                 - command: make lint\n      \
                   timeout: 10\n    \
                 - command: bash \"/old/funes-index.sh\" \"hermes\"\n");
        let entries = desired(Path::new("/h/hooks"), None);
        let out = apply_config_hooks(doc, &entries);

        // Unrelated key survives.
        assert_eq!(out.get("model").unwrap().as_str(), Some("hermes-4"));
        let list = out
            .get("hooks")
            .unwrap()
            .get("post_llm_call")
            .unwrap()
            .as_sequence()
            .unwrap();
        // User hook kept, exactly one (refreshed) funes hook, no duplicate.
        assert!(list
            .iter()
            .any(|e| e.get("command").unwrap().as_str() == Some("make lint")));
        assert_eq!(list.iter().filter(|e| is_funes_entry(e)).count(), 1);
        assert_eq!(
            funes_cmd(&out, "post_llm_call").as_deref(),
            Some("bash \"/h/hooks/funes-index.sh\" \"hermes\"")
        );
    }

    #[test]
    fn local_rerun_after_remote_drops_publish_events() {
        let remote = apply_config_hooks(
            serde_yaml::Value::Mapping(Default::default()),
            &desired(Path::new("/h/hooks"), Some("acme/kb")),
        );
        let local = apply_config_hooks(remote, &desired(Path::new("/h/hooks"), None));
        assert!(local.get("hooks").unwrap().get("on_session_start").is_none());
        assert!(local.get("hooks").unwrap().get("on_session_finalize").is_none());
        assert!(funes_cmd(&local, "post_llm_call").is_some());
    }

    #[test]
    fn allowlist_pre_approves_each_hook_and_is_idempotent() {
        let entries = desired(Path::new("/h/hooks"), Some("acme/kb"));
        // Start from an allowlist holding a user approval and a stale funes one.
        let doc = json!({ "approvals": [
            { "event": "pre_tool_call", "command": "guard.sh" },
            { "event": "post_llm_call", "command": "bash \"/old/funes-index.sh\" \"hermes\"" }
        ]});
        let out = apply_allowlist(doc, &entries);
        let approvals = out["approvals"].as_array().unwrap();

        // User approval kept; exactly one funes approval per desired entry (stale one replaced).
        assert!(approvals.iter().any(|a| a["command"] == "guard.sh"));
        assert_eq!(approvals.iter().filter(|a| is_funes_approval(a)).count(), entries.len());
        // Each approval matches its config command exactly (what hermes keys on).
        for (event, command) in &entries {
            assert!(
                approvals
                    .iter()
                    .any(|a| a["event"] == *event && a["command"].as_str() == Some(command)),
                "missing approval for {event}"
            );
        }
    }

    #[test]
    fn empty_entries_remove_only_funes_hooks_and_approvals() {
        let doc = cfg("model: hermes-4\n\
             hooks:\n  \
               post_llm_call:\n    \
                 - command: make lint\n      \
                   timeout: 10\n    \
                 - command: bash \"/h/funes-index.sh\" \"hermes\"\n  \
               on_session_start:\n    \
                 - command: bash \"/h/funes-push.sh\" \"acme/kb\"\n");
        let out = apply_config_hooks(doc, &[]);
        assert_eq!(out.get("model").unwrap().as_str(), Some("hermes-4"));
        assert_eq!(
            out.get("hooks")
                .unwrap()
                .get("post_llm_call")
                .unwrap()
                .as_sequence()
                .unwrap()
                .len(),
            1
        );
        assert!(out.get("hooks").unwrap().get("on_session_start").is_none());

        let approvals = json!({ "approved": true, "approvals": [
            { "event": "pre_tool_call", "command": "guard.sh" },
            { "event": "post_llm_call", "command": "bash \"/h/funes-index.sh\" \"hermes\"" }
        ]});
        let out = apply_allowlist(approvals, &[]);
        assert_eq!(out["approved"], true);
        assert_eq!(out["approvals"].as_array().unwrap().len(), 1);
        assert_eq!(out["approvals"][0]["command"], "guard.sh");

        let no_hooks = cfg("model: hermes-4\n");
        assert_eq!(apply_config_hooks(no_hooks.clone(), &[]), no_hooks);
        let no_approvals = json!({ "approved": true });
        assert_eq!(apply_allowlist(no_approvals.clone(), &[]), no_approvals);
    }
}
