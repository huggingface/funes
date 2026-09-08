//! Agent-agnostic building blocks for the index/push automation hooks.
//!
//! Two embedded scripts drive the automation: `funes-index.sh` (per-turn local index) and
//! `funes-push.sh` (publish at session boundaries). Agent modules choose lifecycle events, paths,
//! registration mechanisms, and memory bindings; this module only provides the shared scripts,
//! shell command construction, and JSON hook-group merge used by compatible agents.

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const INDEX_SH: &str = include_str!("../../scripts/automation/funes-index.sh");
const PUSH_SH: &str = include_str!("../../scripts/automation/funes-push.sh");
const INDEX_PS: &str = include_str!("../../scripts/automation/funes-index.ps1");
const PUSH_PS: &str = include_str!("../../scripts/automation/funes-push.ps1");

/// The installed index script for this host.
pub const INDEX_NAME: &str = if cfg!(windows) {
    "funes-index.ps1"
} else {
    "funes-index.sh"
};
/// The installed publish script for this host.
pub const PUSH_NAME: &str = if cfg!(windows) {
    "funes-push.ps1"
} else {
    "funes-push.sh"
};

/// The hook's per-run timeout (seconds). Short because both scripts hand off to a detached worker
/// and return in well under a second — the index/push happen off the hook's critical path.
const TIMEOUT: u32 = 15;

/// One funes-owned JSON hook group. The agent module supplies its lifecycle event and command.
pub(crate) struct Hook {
    pub(crate) event: &'static str,
    pub(crate) command: String,
    pub(crate) status: &'static str,
}

/// `bash "<script>" "<arg>"…` — the hook command line. `script` may be a path or an environment
/// expression expanded by the hook runner; double-quoted so spaces survive. `"`/`\` in every field
/// are escaped so a value with a quote can't break out (`$` remains available to the runner).
pub fn command(script: &str, args: &[&str]) -> String {
    if cfg!(windows) {
        return powershell_command(script, args);
    }
    posix_command(script, args)
}

/// Unix-only agent integrations retain their existing Bash templates on every build host.
pub(crate) fn posix_command(script: &str, args: &[&str]) -> String {
    let mut out = format!("bash \"{}\"", dquote_escape(script));
    for arg in args {
        out.push_str(&format!(" \"{}\"", dquote_escape(arg)));
    }
    out
}

// Encoding the whole invocation keeps CMD's %, &, parentheses and quoting rules out of paths
// and memory arguments. PowerShell single-quoted literals only escape an apostrophe by doubling it.
fn powershell_command(script: &str, args: &[&str]) -> String {
    let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let mut invocation = format!("& {}", literal(script));
    for arg in args {
        invocation.push(' ');
        invocation.push_str(&literal(arg));
    }
    let bytes: Vec<u8> = invocation.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {encoded}")
}

fn decoded_command(command: &str) -> Option<String> {
    let (_, encoded) = command.split_once(" -EncodedCommand ")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let words: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    String::from_utf16(&words).ok()
}

/// Escape a value for embedding inside a double-quoted shell string: backslash then double-quote.
/// A no-op for ordinary paths and harness/memory names.
fn dquote_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Remove every funes hook group from `cfg` (across all events), then add `desired`. The
/// remove-then-add is what makes re-running idempotent — funes's groups are replaced, never
/// duplicated — while leaving every non-funes hook untouched. Empty event arrays are pruned.
pub(crate) fn apply_funes_hooks(mut cfg: Value, desired: &[Hook]) -> Value {
    let obj = cfg.as_object_mut().expect("cfg is a JSON object");
    if !obj.get("hooks").map(Value::is_object).unwrap_or(false) {
        if desired.is_empty() {
            return cfg;
        }
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
        .map(|c| {
            let decoded = decoded_command(c);
            let c = decoded.as_deref().unwrap_or(c);
            ["funes-index.sh", "funes-push.sh", "funes-index.ps1", "funes-push.ps1"]
                .iter()
                .any(|name| c.contains(name))
        })
        .unwrap_or(false)
}

/// Write the embedded scripts into an agent-chosen `dir`, executable. Returns whether anything
/// changed (a drifted or absent copy is rewritten); the executable bit is (re)set every time.
pub fn write_scripts(dir: &Path) -> Result<bool> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut changed = false;
    let scripts = if cfg!(windows) {
        [(INDEX_NAME, INDEX_PS), (PUSH_NAME, PUSH_PS)]
    } else {
        [(INDEX_NAME, INDEX_SH), (PUSH_NAME, PUSH_SH)]
    };
    for (name, content) in scripts {
        let path = dir.join(name);
        changed |= write_if_changed(&path, content)?;
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).with_context(|| format!("chmod +x {}", path.display()))?;
        }
    }
    Ok(changed)
}

/// Write `content` to `path` (creating parents) only if it differs from what's there. Returns
/// whether it wrote — the caller uses this to skip an unnecessary plugin reinstall.
pub(crate) fn write_if_changed(path: &Path, content: &str) -> Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_preserves_literals_and_owned_hook_cleanup() {
        let script = r"C:\用户 & (data)\100%\it's\funes-index.ps1";
        let cmd = powershell_command(script, &["acme/a'b", "codex"]);
        assert_eq!(
            decoded_command(&cmd).unwrap(),
            "& 'C:\\用户 & (data)\\100%\\it''s\\funes-index.ps1' 'acme/a''b' 'codex'"
        );
        assert!(cmd
            .split(" -EncodedCommand ")
            .nth(1)
            .unwrap()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)));
        let config = json!({"hooks":{"Stop":[{"hooks":[{"command":cmd}]}]}});
        assert_eq!(apply_funes_hooks(config, &[]), json!({"hooks":{}}));
    }

    fn idx(arg: &str) -> Hook {
        Hook {
            event: "TurnComplete",
            command: command("/h/funes-index.sh", &[arg]),
            status: "idx",
        }
    }

    #[test]
    #[cfg(unix)]
    fn command_escapes_quotes_but_keeps_dollar() {
        // Ordinary paths/args are untouched, and every argument is carried.
        assert_eq!(command("/h/x.sh", &["agent"]), "bash \"/h/x.sh\" \"agent\"");
        assert_eq!(
            command("/h/x.sh", &["acme/kb", "codex"]),
            "bash \"/h/x.sh\" \"acme/kb\" \"codex\""
        );
        // A quote in a value is escaped, so it can't break out of the double-quotes.
        assert_eq!(command("/h/a\"b.sh", &["s"]), "bash \"/h/a\\\"b.sh\" \"s\"");
        // `$` is left intact so the hook runner can expand an environment-provided root.
        assert_eq!(
            command("${HOOK_ROOT}/x.sh", &["agent"]),
            "bash \"${HOOK_ROOT}/x.sh\" \"agent\""
        );
    }

    fn push(event: &'static str, memory: &str) -> Hook {
        Hook {
            event,
            command: command("/h/funes-push.sh", &[memory]),
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
        let out = apply_funes_hooks(json!({}), &[idx("agent")]);
        assert_eq!(
            funes_command(&out, "TurnComplete"),
            Some(command("/h/funes-index.sh", &["agent"]).as_str())
        );
    }

    #[test]
    fn replaces_the_funes_group_and_preserves_others() {
        // A config with the user's own hook, a stale funes hook, and an unrelated event.
        let cfg = json!({
            "hooks": {
                "TurnComplete": [
                    { "hooks": [ { "type": "command", "command": "make lint" } ] },
                    { "hooks": [ { "type": "command", "command": "bash \"/old/funes-index.sh\" \"agent\"" } ] }
                ],
                "BeforeTool": [ { "hooks": [ { "type": "command", "command": "guard.sh" } ] } ]
            }
        });
        let out = apply_funes_hooks(cfg, &[idx("agent")]);

        let completed = out["hooks"]["TurnComplete"].as_array().unwrap();
        assert_eq!(completed.len(), 2, "user group + one refreshed funes group");
        assert!(completed.iter().any(|g| g["hooks"][0]["command"] == "make lint"));
        assert_eq!(
            funes_command(&out, "TurnComplete"),
            Some(command("/h/funes-index.sh", &["agent"]).as_str())
        );
        assert_eq!(
            completed.iter().filter(|g| is_funes_group(g)).count(),
            1,
            "no duplicate funes group"
        );
        assert_eq!(out["hooks"]["BeforeTool"][0]["hooks"][0]["command"], "guard.sh");
    }

    #[test]
    fn push_events_only_with_a_memory() {
        let local = apply_funes_hooks(json!({}), &[idx("agent")]);
        assert!(local["hooks"].get("Start").is_none());

        let remote = apply_funes_hooks(
            json!({}),
            &[idx("agent"), push("Start", "acme/kb"), push("End", "acme/kb")],
        );
        assert_eq!(
            funes_command(&remote, "Start"),
            Some(command("/h/funes-push.sh", &["acme/kb"]).as_str())
        );
        assert_eq!(
            funes_command(&remote, "End"),
            Some(command("/h/funes-push.sh", &["acme/kb"]).as_str())
        );
    }

    #[test]
    fn re_running_local_after_remote_drops_the_push_hooks() {
        let remote = apply_funes_hooks(
            json!({}),
            &[idx("agent"), push("Start", "acme/kb"), push("End", "acme/kb")],
        );
        let local = apply_funes_hooks(remote, &[idx("agent")]);
        assert!(local["hooks"].get("Start").is_none(), "stale push event pruned");
        assert!(local["hooks"].get("End").is_none(), "stale push event pruned");
        assert_eq!(
            funes_command(&local, "TurnComplete"),
            Some(command("/h/funes-index.sh", &["agent"]).as_str())
        );
    }

    #[test]
    fn empty_desired_removes_only_funes_groups() {
        let cfg = json!({
            "theme": "dark",
            "hooks": {
                "TurnComplete": [
                    { "hooks": [ { "type": "command", "command": "make lint" } ] },
                    { "hooks": [ { "type": "command", "command": "bash \"/h/funes-index.sh\" \"agent\"" } ] }
                ],
                "Start": [
                    { "hooks": [ { "type": "command", "command": "bash \"/h/funes-push.sh\" \"memory\"" } ] }
                ]
            }
        });
        let out = apply_funes_hooks(cfg, &[]);
        assert_eq!(out["theme"], "dark");
        assert_eq!(out["hooks"]["TurnComplete"].as_array().unwrap().len(), 1);
        assert_eq!(out["hooks"]["TurnComplete"][0]["hooks"][0]["command"], "make lint");
        assert!(out["hooks"].get("Start").is_none());

        let no_hooks = json!({ "theme": "dark" });
        assert_eq!(apply_funes_hooks(no_hooks.clone(), &[]), no_hooks);
    }
}
