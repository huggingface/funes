//! `funes add <agent>` — installing the index/push automation hooks (issue #72).
//!
//! Two embedded scripts drive the automation: `funes-index.sh` (per-turn local index) and
//! `funes-push.sh` (publish at session boundaries, when a remote store is bound). How they're
//! registered differs by agent:
//!
//! - **Claude** has a plugin system, so funes ships a hooks-only plugin. `funes add` extracts it to
//!   `~/.funes/integrations/claude-plugin` and registers it with `claude plugin marketplace add`
//!   plus `claude plugin install`. Claude's loader activates the plugin's own `hooks/hooks.json` —
//!   funes never parses or rewrites the user's `settings.json`. Removal is `claude plugin uninstall
//!   funes@huggingface`.
//! - **Codex** has no plugin system, so funes writes its hooks into `~/.codex/hooks.json` — a file
//!   dedicated to hooks, not Codex's main `config.toml`. The merge is append-or-replace keyed by
//!   funes's own scripts (re-running replaces funes's groups, leaves any others untouched).
//!
//! The push hook is registered only when a remote store is bound (`funes add <agent> <store>`); a
//! local-only install indexes each turn but has nothing to publish.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const INDEX_SH: &str = include_str!("../scripts/automation/funes-index.sh");
const PUSH_SH: &str = include_str!("../scripts/automation/funes-push.sh");
const MARKETPLACE_JSON: &str = include_str!("../integrations/claude-plugin/.claude-plugin/marketplace.json");
const PLUGIN_JSON: &str = include_str!("../integrations/claude-plugin/funes/.claude-plugin/plugin.json");

/// The hook's per-run timeout (seconds). Short because both scripts hand off to a detached worker
/// and return in well under a second — the index/push happen off the hook's critical path.
const TIMEOUT: u32 = 15;
const INDEX_STATUS: &str = "Indexing turn into funes memory";
const PUSH_STATUS: &str = "Publishing funes memory";

/// The Claude plugin id funes registers (`<plugin>@<marketplace>`): the `funes` plugin in the
/// `huggingface` marketplace (names from the bundled plugin.json / marketplace.json).
const PLUGIN_ID: &str = "funes@huggingface";

/// An agent whose hooks funes manages.
#[derive(Clone, Copy)]
pub enum Agent {
    Claude,
    Codex,
}

/// Install the index (and, with a bound `store`, push) hooks for `agent`.
pub fn install(agent: Agent, store: Option<&str>) -> Result<()> {
    match agent {
        Agent::Claude => install_claude(store),
        Agent::Codex => install_codex(store),
    }
}

/// A funes-owned hook: which lifecycle event fires it, the shell command, and the status line.
struct Desired {
    event: &'static str,
    command: String,
    status: &'static str,
}

/// `bash "<script>" "<arg>"` — the hook command line. `script` is a path or a `${CLAUDE_PLUGIN_ROOT}`
/// expression (Claude expands the latter before the shell runs it); double-quoted so a space
/// survives. Also used by hermes' YAML hook install. `"`/`\` in either field are escaped so a value
/// with a quote can't break out of the double-quotes (`$` is left intact so `${CLAUDE_PLUGIN_ROOT}`
/// still expands).
pub fn command(script: &str, arg: &str) -> String {
    format!("bash \"{}\" \"{}\"", dquote_escape(script), dquote_escape(arg))
}

/// Escape a value for embedding inside a double-quoted shell string: backslash then double-quote.
/// A no-op for ordinary paths and harness/store names.
fn dquote_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The funes hook set: a per-turn index (always) and, with a bound `store`, a publish on each
/// session boundary. `index_script`/`push_script` are how the scripts are referenced from the
/// command line (absolute path for Codex, `${CLAUDE_PLUGIN_ROOT}/scripts/…` for the Claude plugin);
/// `session_end` adds the SessionEnd publish (Claude has one, Codex doesn't).
fn desired(
    index_script: &str,
    push_script: &str,
    harness: &str,
    store: Option<&str>,
    session_end: bool,
) -> Vec<Desired> {
    let mut out = vec![Desired {
        event: "Stop",
        command: command(index_script, harness),
        status: INDEX_STATUS,
    }];
    if let Some(s) = store {
        out.push(Desired {
            event: "SessionStart",
            command: command(push_script, s),
            status: PUSH_STATUS,
        });
        if session_end {
            out.push(Desired {
                event: "SessionEnd",
                command: command(push_script, s),
                status: PUSH_STATUS,
            });
        }
    }
    out
}

// ---- Claude: a hooks-only plugin ----

/// Ship the funes hooks plugin: extract it to `~/.funes/integrations/claude-plugin` (fixed, like the
/// pi extension — Claude records the marketplace source path, so it must outlive the session) and
/// register it with `claude`. The plugin's `hooks/hooks.json` is regenerated with the current store.
fn install_claude(store: Option<&str>) -> Result<()> {
    // Fixed at `~/.funes`, deliberately not `$FUNES_HOME`: Claude stores the marketplace source
    // path by reference, so it can't follow a per-session home.
    let home = PathBuf::from(std::env::var_os("HOME").context("resolving $HOME for the plugin dir")?);
    let root = home.join(".funes/integrations/claude-plugin");
    let plugin = root.join("funes");

    let hooks = desired(
        "${CLAUDE_PLUGIN_ROOT}/scripts/funes-index.sh",
        "${CLAUDE_PLUGIN_ROOT}/scripts/funes-push.sh",
        "claude",
        store,
        true,
    );
    let hooks_json = format!(
        "{}\n",
        serde_json::to_string_pretty(&apply_funes_hooks(json!({}), &hooks))?
    );

    // The plugin's hooks.json is funes's own file (not the user's), so it's generated whole — no
    // merge, no parsing anyone else's config. `dirty` tracks whether anything changed, so an
    // unchanged re-run skips the uninstall/reinstall refresh below.
    let mut dirty = false;
    dirty |= write_if_changed(&root.join(".claude-plugin/marketplace.json"), MARKETPLACE_JSON)?;
    dirty |= write_if_changed(&plugin.join(".claude-plugin/plugin.json"), PLUGIN_JSON)?;
    dirty |= write_if_changed(&plugin.join("hooks/hooks.json"), &hooks_json)?;
    dirty |= write_scripts(&plugin.join("scripts"))?;

    register_claude(&root, store.is_some(), dirty)
}

/// Register (or refresh) the extracted plugin with `claude`. Idempotent: the marketplace add is a
/// no-op when already present, and the plugin is (re)installed to pick up the current source — but
/// since `claude plugin install` is a no-op when already installed and `update` is version-gated,
/// a refresh (`dirty`) uninstalls first to force a fresh copy.
fn register_claude(root: &Path, has_store: bool, dirty: bool) -> Result<()> {
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
    // A content change won't be picked up by a plain re-install (no-op) or `update` (version-gated),
    // so force it by uninstalling first. On a first install the plugin isn't there yet and this
    // errors — expected, so its output is silenced and its status ignored.
    if dirty {
        let _ = Command::new("claude")
            .args(["plugin", "uninstall", PLUGIN_ID])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    match Command::new("claude").args(["plugin", "install", PLUGIN_ID]).status() {
        Ok(s) if s.success() => {
            let what = if has_store {
                "indexes each turn and publishes at session boundaries"
            } else {
                "indexes each turn (local only — pass a store to also publish)"
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

// ---- Codex: a dedicated hooks.json funes owns ----

/// Write funes's hooks into `~/.codex/hooks.json`. Codex has no plugin system, but that file is
/// dedicated to hooks (not `config.toml`), so the merge is low-stakes: append-or-replace keyed by
/// funes's scripts, leaving any hand-authored hooks alone.
fn install_codex(store: Option<&str>) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("resolving $HOME for the hooks dir")?);
    let base = home.join(".codex");
    let hooks_dir = base.join("hooks");
    write_scripts(&hooks_dir)?;

    let index = hooks_dir.join("funes-index.sh").display().to_string();
    let push = hooks_dir.join("funes-push.sh").display().to_string();
    let want = desired(&index, &push, "codex", store, false);

    let config = base.join("hooks.json");
    let cfg = match std::fs::read_to_string(&config).ok().as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => match serde_json::from_str::<Value>(s) {
            Ok(v) if v.is_object() => v,
            // A config that isn't a plain JSON object (rare for this file) isn't ours to rewrite.
            _ => return manual_instructions(&config, &want),
        },
        _ => json!({}),
    };
    let out = apply_funes_hooks(cfg, &want);
    if let Some(dir) = config.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&config, format!("{}\n", serde_json::to_string_pretty(&out)?))
        .with_context(|| format!("writing {}", config.display()))?;

    let events: Vec<&str> = want.iter().map(|d| d.event).collect();
    let what = if store.is_some() {
        "indexes each turn and publishes at session boundaries"
    } else {
        "indexes each turn (local only — pass a store to also publish)"
    };
    println!(
        "installed funes hooks into {} ({}) — {what}.",
        config.display(),
        events.join(", ")
    );
    Ok(())
}

// ---- shared ----

/// Remove every funes hook group from `cfg` (across all events), then add `desired`. The
/// remove-then-add is what makes re-running idempotent — funes's groups are replaced, never
/// duplicated — while leaving every non-funes hook untouched. Empty event arrays are pruned.
fn apply_funes_hooks(mut cfg: Value, desired: &[Desired]) -> Value {
    let obj = cfg.as_object_mut().expect("cfg is a JSON object");
    if !obj.get("hooks").map(Value::is_object).unwrap_or(false) {
        obj.insert("hooks".to_string(), json!({}));
    }
    let hooks = obj["hooks"].as_object_mut().expect("hooks is an object");

    for group_list in hooks.values_mut() {
        if let Some(list) = group_list.as_array_mut() {
            list.retain(|g| !is_funes_group(g));
        }
    }
    for d in desired {
        let group = json!({
            "hooks": [ { "type": "command", "command": d.command, "timeout": TIMEOUT, "statusMessage": d.status } ]
        });
        hooks
            .entry(d.event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("event maps to a hook-group array")
            .push(group);
    }
    hooks.retain(|_event, list| !list.as_array().map(|a| a.is_empty()).unwrap_or(false));
    cfg
}

/// A hook group is funes's if any of its commands invokes a funes script.
fn is_funes_group(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hs| hs.iter().any(is_funes_hook))
        .unwrap_or(false)
}

fn is_funes_hook(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .map(|c| c.contains("funes-index.sh") || c.contains("funes-push.sh"))
        .unwrap_or(false)
}

/// Write the embedded scripts into `dir`, executable. Returns whether anything changed (a drifted
/// or absent copy is rewritten); the executable bit is (re)set every time regardless. Shared with
/// hermes, which drops the same scripts into `~/.hermes/hooks`.
pub fn write_scripts(dir: &Path) -> Result<bool> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut changed = false;
    for (name, content) in [("funes-index.sh", INDEX_SH), ("funes-push.sh", PUSH_SH)] {
        let path = dir.join(name);
        changed |= write_if_changed(&path, content)?;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).with_context(|| format!("chmod +x {}", path.display()))?;
    }
    Ok(changed)
}

/// Write `content` to `path` (creating parents) only if it differs from what's there. Returns
/// whether it wrote — the caller uses this to skip an unnecessary plugin reinstall.
fn write_if_changed(path: &Path, content: &str) -> Result<bool> {
    if file_matches(path, content) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// True if `path` exists and already holds exactly `want`.
fn file_matches(path: &Path, want: &str) -> bool {
    std::fs::read_to_string(path).map(|got| got == want).unwrap_or(false)
}

/// When a config can't be parsed as plain JSON, don't clobber it — print the `hooks` block to merge
/// by hand. (Only reachable for Codex's `hooks.json`; Claude never touches user config.)
fn manual_instructions(path: &Path, desired: &[Desired]) -> Result<()> {
    let block = serde_json::to_string_pretty(&apply_funes_hooks(json!({}), desired))?;
    println!(
        "{} isn't plain JSON — leaving it untouched. Merge this in to enable funes hooks:\n{block}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(harness: &str) -> Desired {
        Desired {
            event: "Stop",
            command: command("/h/funes-index.sh", harness),
            status: "idx",
        }
    }

    #[test]
    fn command_escapes_quotes_but_keeps_dollar() {
        // Ordinary paths/args are untouched.
        assert_eq!(command("/h/x.sh", "claude"), "bash \"/h/x.sh\" \"claude\"");
        // A quote in a value is escaped, so it can't break out of the double-quotes.
        assert_eq!(command("/h/a\"b.sh", "s"), "bash \"/h/a\\\"b.sh\" \"s\"");
        // `$` is left intact so Claude still expands ${CLAUDE_PLUGIN_ROOT}.
        assert_eq!(
            command("${CLAUDE_PLUGIN_ROOT}/x.sh", "claude"),
            "bash \"${CLAUDE_PLUGIN_ROOT}/x.sh\" \"claude\""
        );
    }

    fn push(event: &'static str, store: &str) -> Desired {
        Desired {
            event,
            command: command("/h/funes-push.sh", store),
            status: "push",
        }
    }

    /// The command carried by the single hook in event `ev`'s first funes group.
    fn funes_command<'a>(cfg: &'a Value, ev: &str) -> Option<&'a str> {
        cfg["hooks"][ev]
            .as_array()?
            .iter()
            .find(|g| is_funes_group(g))?
            .get("hooks")?
            .as_array()?
            .first()?
            .get("command")?
            .as_str()
    }

    #[test]
    fn appends_when_absent() {
        let out = apply_funes_hooks(json!({}), &[idx("claude")]);
        assert_eq!(
            funes_command(&out, "Stop"),
            Some("bash \"/h/funes-index.sh\" \"claude\"")
        );
    }

    #[test]
    fn replaces_the_funes_group_and_preserves_others() {
        // A config with the user's own Stop hook, a *stale* funes Stop hook, and an unrelated event.
        let cfg = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [ { "type": "command", "command": "make lint" } ] },
                    { "hooks": [ { "type": "command", "command": "bash \"/old/funes-index.sh\" \"claude\"" } ] }
                ],
                "PreToolUse": [ { "hooks": [ { "type": "command", "command": "guard.sh" } ] } ]
            }
        });
        let out = apply_funes_hooks(cfg, &[idx("claude")]);

        let stop = out["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "user group + one refreshed funes group");
        assert!(stop.iter().any(|g| g["hooks"][0]["command"] == "make lint"));
        assert_eq!(
            funes_command(&out, "Stop"),
            Some("bash \"/h/funes-index.sh\" \"claude\"")
        );
        assert_eq!(
            stop.iter().filter(|g| is_funes_group(g)).count(),
            1,
            "no duplicate funes group"
        );
        assert_eq!(out["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "guard.sh");
    }

    #[test]
    fn push_events_only_with_a_store() {
        let local = apply_funes_hooks(json!({}), &[idx("claude")]);
        assert!(local["hooks"].get("SessionStart").is_none());

        let remote = apply_funes_hooks(
            json!({}),
            &[
                idx("claude"),
                push("SessionStart", "acme/kb"),
                push("SessionEnd", "acme/kb"),
            ],
        );
        assert_eq!(
            funes_command(&remote, "SessionStart"),
            Some("bash \"/h/funes-push.sh\" \"acme/kb\"")
        );
        assert_eq!(
            funes_command(&remote, "SessionEnd"),
            Some("bash \"/h/funes-push.sh\" \"acme/kb\"")
        );
    }

    #[test]
    fn re_running_local_after_remote_drops_the_push_hooks() {
        let remote = apply_funes_hooks(
            json!({}),
            &[
                idx("claude"),
                push("SessionStart", "acme/kb"),
                push("SessionEnd", "acme/kb"),
            ],
        );
        let local = apply_funes_hooks(remote, &[idx("claude")]);
        assert!(local["hooks"].get("SessionStart").is_none(), "stale push event pruned");
        assert!(local["hooks"].get("SessionEnd").is_none(), "stale push event pruned");
        assert_eq!(
            funes_command(&local, "Stop"),
            Some("bash \"/h/funes-index.sh\" \"claude\"")
        );
    }

    #[test]
    fn desired_bakes_scripts_store_and_session_end() {
        // Claude plugin shape: plugin-root script refs, SessionStart + SessionEnd with the store.
        let claude = desired(
            "${CLAUDE_PLUGIN_ROOT}/scripts/funes-index.sh",
            "${CLAUDE_PLUGIN_ROOT}/scripts/funes-push.sh",
            "claude",
            Some("acme/kb"),
            true,
        );
        let out = apply_funes_hooks(json!({}), &claude);
        assert_eq!(
            funes_command(&out, "Stop"),
            Some("bash \"${CLAUDE_PLUGIN_ROOT}/scripts/funes-index.sh\" \"claude\"")
        );
        assert_eq!(
            funes_command(&out, "SessionEnd"),
            Some("bash \"${CLAUDE_PLUGIN_ROOT}/scripts/funes-push.sh\" \"acme/kb\"")
        );

        // Codex shape: absolute path, no SessionEnd, and local (no push at all).
        let codex = desired("/h/funes-index.sh", "/h/funes-push.sh", "codex", None, false);
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].event, "Stop");
    }
}
