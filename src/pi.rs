//! `funes add pi`: install funes recall as a first-class pi tool.
//!
//! pi has no MCP client, so funes ships a small pi extension (a bridge that
//! spawns `funes mcp` over stdio — see `integrations/pi/`). The extension is
//! embedded in the binary here, so once `funes` is on PATH a single
//! `funes add pi` drops it where pi loads it — no separate package to fetch,
//! and it always matches this binary's MCP surface. The install is always
//! user-wide at a fixed `~/.funes/integrations/pi` — independent of the cwd and
//! of `FUNES_HOME`, because pi records the install path permanently — and is
//! registered with `pi install`. pi >= 0.80 no longer auto-loads
//! `.pi/extensions`, so the explicit register is required.

use crate::update::parse_semver;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const INDEX_TS: &str = include_str!("../integrations/pi/index.ts");
const PACKAGE_JSON: &str = include_str!("../integrations/pi/package.json");

/// The pi version funes' extension API (`pi.extensions` manifest, `registerTool`, the
/// provided `typebox`) was validated against — the `@earendil-works/pi-coding-agent` line
/// (the older `@mariozechner` scope is deprecated). Older pi may not load the extension.
const MIN_PI: (u32, u32, u32) = (0, 74, 2);

/// Install the embedded pi extension at `~/.funes/integrations/pi` and register it with pi.
/// That path is fixed — independent of the cwd and of `FUNES_HOME` — because pi records the
/// install location permanently, so it must outlive any session or demo. A copy that differs
/// from this binary's embedded extension (e.g. after an upgrade) is refreshed automatically;
/// `force` rewrites even when it matches.
pub fn install(force: bool) -> Result<()> {
    // Fixed at `~/.funes/integrations/pi`, deliberately not `dataset::funes_dir()`: pi stores
    // the install path by reference, so it can't follow a per-session FUNES_HOME (a demo points
    // that elsewhere and the dir may not outlive the install).
    let home = std::env::var_os("HOME").context("resolving $HOME for the pi install dir")?;
    let dir = PathBuf::from(home).join(".funes/integrations/pi");
    extract(&dir, force)?;
    let dir = dir.to_string_lossy().into_owned();

    // Probe pi: this confirms it's on PATH (else extract-and-instruct) and lets us flag a
    // version older than the one the extension API was validated against.
    let version = match Command::new("pi").arg("--version").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(_) => String::new(), // pi present but odd --version; proceed without a version
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("extracted the funes pi extension to {dir}");
            println!("`pi` isn't on PATH — once it is, run:  pi install {dir}");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `pi --version`")),
    };
    if matches!(parse_semver(&version), Some(v) if v < MIN_PI) {
        eprintln!(
            "warning: pi {version} is older than the tested {}.{}.{} — if `recall` doesn't appear in pi, run `pi update`.",
            MIN_PI.0, MIN_PI.1, MIN_PI.2
        );
    }

    match Command::new("pi").arg("install").arg(&dir).status() {
        Ok(s) if s.success() => {
            let v = if version.is_empty() {
                String::new()
            } else {
                format!(" {version}")
            };
            println!("installed funes recall into pi{v} — `recall`/`get` are now available (restart pi if it's running).");
            Ok(())
        }
        Ok(s) => anyhow::bail!(
            "`pi install {dir}` failed (exit {:?}); the extension is extracted there — retry that command manually.",
            s.code()
        ),
        Err(e) => Err(anyhow::Error::new(e).context("running `pi install`")),
    }
}

/// True if `path` exists and already holds exactly `want`.
fn file_matches(path: &Path, want: &str) -> bool {
    std::fs::read_to_string(path).map(|got| got == want).unwrap_or(false)
}

/// Write the embedded extension (index.ts + package.json) into `dir`. A copy that drifts
/// from this binary's embedded version is refreshed; `force` rewrites even when it matches.
fn extract(dir: &Path, force: bool) -> Result<()> {
    let current = !force
        && file_matches(&dir.join("index.ts"), INDEX_TS)
        && file_matches(&dir.join("package.json"), PACKAGE_JSON);
    if current {
        println!("funes pi extension already current at {}", dir.display());
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("index.ts"), INDEX_TS).context("writing index.ts")?;
    std::fs::write(dir.join("package.json"), PACKAGE_JSON).context("writing package.json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{file_matches, parse_semver, MIN_PI};

    #[test]
    fn parses_versions_and_compares_to_min() {
        assert_eq!(parse_semver("0.73.0"), Some((0, 73, 0)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("0.73.0-beta.1"), Some((0, 73, 0)));
        assert_eq!(parse_semver("0.73"), Some((0, 73, 0)));
        assert_eq!(parse_semver("1.0.0 (abc123)"), Some((1, 0, 0)));
        assert_eq!(parse_semver("not-a-version"), None);
        assert_eq!(parse_semver(""), None);

        // the floor comparison the install warning hinges on
        assert!(parse_semver("0.74.1").unwrap() < MIN_PI);
        assert!(parse_semver("0.74.2").unwrap() >= MIN_PI);
        assert!(parse_semver("1.0.0").unwrap() >= MIN_PI);
    }

    #[test]
    fn file_matches_detects_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.ts");
        std::fs::write(&path, "embedded").unwrap();
        assert!(file_matches(&path, "embedded")); // unchanged → skip rewrite
        assert!(!file_matches(&path, "embedded v2")); // drifted after upgrade → rewrite
        assert!(!file_matches(&dir.path().join("missing.ts"), "embedded")); // absent → extract
    }
}
