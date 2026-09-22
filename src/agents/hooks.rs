//! Agent-agnostic building blocks for the index/push automation hooks.
//!
//! Two embedded scripts drive the automation: `funes-index.sh` (per-turn local index) and
//! `funes-push.sh` (publish at session boundaries). Agent modules choose lifecycle events, paths,
//! registration mechanisms, and memory bindings; this module only provides the shared scripts,
//! shell command construction, and the writing of them.

use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const INDEX_SH: &str = include_str!("../../scripts/automation/funes-index.sh");
const PUSH_SH: &str = include_str!("../../scripts/automation/funes-push.sh");

/// `bash "<script>" "<arg>"…` — the hook command line. `script` may be a path or an environment
/// expression expanded by the hook runner; double-quoted so spaces survive. `"`/`\` in every field
/// are escaped so a value with a quote can't break out (`$` remains available to the runner).
pub fn command(script: &str, args: &[&str]) -> String {
    let mut out = format!("bash \"{}\"", dquote_escape(script));
    for arg in args {
        out.push_str(&format!(" \"{}\"", dquote_escape(arg)));
    }
    out
}

/// Escape a value for embedding inside a double-quoted shell string: backslash then double-quote.
/// A no-op for ordinary paths and harness/memory names.
fn dquote_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Write the embedded scripts into an agent-chosen `dir`, executable. Returns whether anything
/// changed (a drifted or absent copy is rewritten); the executable bit is (re)set every time.
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
}
