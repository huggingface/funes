//! Pre-publish secret scan. Before any chunk text leaves the machine for the Hub, it is run
//! through trufflehog; a finding blocks the publish. This keeps funes from handing the Hub's
//! receiving-end scanner a credential — which it would auto-invalidate on receipt.
//!
//! Fail-closed: if trufflehog can't be found or run, scanning errors rather than letting an
//! unscanned payload through. The binary is discovered via `$FUNES_TRUFFLEHOG`, then `$PATH`,
//! then common install dirs — funes runs as an IDE-spawned MCP server, whose `$PATH` is often
//! stripped of `/opt/homebrew/bin` and the like, so PATH alone isn't enough.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// trufflehog's exit code (under `--fail`) when at least one result was found.
const FOUND: i32 = 183;

/// One potential secret trufflehog flagged; `redacted` is its safe-to-log form.
#[derive(Debug, serde::Deserialize)]
pub struct Finding {
    #[serde(rename = "DetectorName")]
    pub detector: String,
    #[serde(rename = "Redacted")]
    pub redacted: String,
}

/// Locate the trufflehog binary: `$FUNES_TRUFFLEHOG` override → `$PATH` → common install dirs
/// (Homebrew, `/usr/local/bin`, `/usr/bin`, `~/go/bin`, `~/.local/bin`). Errors if none exists —
/// the gate is mandatory, so a missing scanner is fail-closed, never a silent pass.
fn find_trufflehog() -> Result<PathBuf> {
    find_in(|k| std::env::var_os(k), |p| p.is_file())
}

/// Pure core of [`find_trufflehog`]: candidate order is override → PATH entries → common dirs;
/// the first for which `exists` holds wins. Split out so discovery is testable without touching
/// the real environment or filesystem.
fn find_in(env: impl Fn(&str) -> Option<OsString>, exists: impl Fn(&Path) -> bool) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(over) = env("FUNES_TRUFFLEHOG") {
        candidates.push(PathBuf::from(over));
    }
    if let Some(path) = env("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join("trufflehog")));
    }
    if let Some(home) = env("HOME").map(PathBuf::from) {
        candidates.push(home.join("go/bin/trufflehog"));
        candidates.push(home.join(".local/bin/trufflehog"));
    }
    candidates.extend(
        [
            "/opt/homebrew/bin/trufflehog",
            "/usr/local/bin/trufflehog",
            "/usr/bin/trufflehog",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    candidates.into_iter().find(|p| exists(p)).ok_or_else(|| {
        anyhow!(
            "trufflehog not found (checked $FUNES_TRUFFLEHOG, $PATH, Homebrew, /usr/local/bin, \
             /usr/bin, ~/go/bin, ~/.local/bin). The pre-publish secret scan is mandatory — \
             refusing to publish unscanned. Install it \
             (https://github.com/trufflesecurity/trufflehog) or set FUNES_TRUFFLEHOG=/path/to/trufflehog."
        )
    })
}

/// Scan the files under `dir` with trufflehog, offline (no verification). Empty vec = clean.
/// Errors if trufflehog is missing or exits abnormally; callers MUST treat an error as
/// fail-closed and not publish.
fn scan_dir(dir: &Path) -> Result<Vec<Finding>> {
    let bin = find_trufflehog()?;
    let out = Command::new(&bin)
        .arg("filesystem")
        .arg(dir)
        .args([
            "--json",
            "--no-verification",
            "--no-update",
            "--fail",
            "--results=verified,unknown,unverified",
        ])
        .output()
        .with_context(|| format!("running trufflehog at {}", bin.display()))?;

    match out.status.code() {
        Some(0) => Ok(Vec::new()),
        Some(FOUND) => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<Finding>(l).ok())
            .collect()),
        other => bail!(
            "trufflehog exited abnormally ({other:?}); refusing to publish unscanned:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// The pre-publish gate: scan `texts` (the chunk plaintext about to be uploaded) and refuse to
/// proceed if trufflehog flags anything. Fail-closed.
pub fn ensure_no_secrets(texts: &[String]) -> Result<()> {
    let dir = tempfile::tempdir().context("creating a temp dir for the secret scan")?;
    let mut f = std::fs::File::create(dir.path().join("chunks.txt"))?;
    for t in texts {
        writeln!(f, "{t}")?;
    }
    f.flush()?;

    let findings = scan_dir(dir.path())?;
    if findings.is_empty() {
        return Ok(());
    }
    let detail = findings
        .iter()
        .take(10)
        .map(|x| format!("  {} — {}", x.detector, x.redacted.lines().next().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "refusing to publish: trufflehog flagged {} potential secret(s):\n{detail}\n\
         Remove them from the source sessions and re-index before publishing.",
        findings.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_precedence() {
        // $FUNES_TRUFFLEHOG wins outright, even with a PATH set.
        let env = |k: &str| match k {
            "FUNES_TRUFFLEHOG" => Some(OsString::from("/custom/trufflehog")),
            "PATH" => Some(OsString::from("/usr/bin")),
            _ => None,
        };
        assert_eq!(find_in(env, |_| true).unwrap(), PathBuf::from("/custom/trufflehog"));

        // No override: the first existing PATH entry wins.
        let env = |k: &str| (k == "PATH").then(|| OsString::from("/aa:/bb"));
        assert_eq!(
            find_in(env, |p| p.starts_with("/aa") || p.starts_with("/bb")).unwrap(),
            PathBuf::from("/aa/trufflehog")
        );

        // Nothing anywhere: fail-closed with an actionable message.
        let err = find_in(|_| None, |_| false).unwrap_err().to_string();
        assert!(err.contains("not found") && err.contains("FUNES_TRUFFLEHOG"), "{err}");
    }

    #[test]
    fn flags_a_generated_private_key() {
        if find_trufflehog().is_err() {
            eprintln!("skip: trufflehog not found");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id_ed25519");
        // A throwaway key generated at test time — never committed, so funes ships no secret.
        let made = Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-q", "-f"])
            .arg(&key)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skip: ssh-keygen unavailable");
            return;
        }
        let findings = scan_dir(dir.path()).expect("scan");
        assert!(
            findings.iter().any(|f| f.detector == "PrivateKey"),
            "expected a PrivateKey finding, got {findings:?}"
        );
    }

    #[test]
    fn clean_text_passes() {
        if find_trufflehog().is_err() {
            eprintln!("skip: trufflehog not found");
            return;
        }
        assert!(ensure_no_secrets(&["how do we parse transcripts into turns".to_string()]).is_ok());
    }
}
