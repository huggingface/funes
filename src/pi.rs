//! `funes add pi` / `funes remove pi`: manage funes recall as a first-class pi tool.
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
/// from this binary's embedded extension (e.g. after an upgrade, or a different bound `memory`) is
/// refreshed automatically; `force` rewrites even when it matches.
pub fn install(memory: Option<String>, force: bool) -> Result<()> {
    // Fixed at `~/.funes/integrations/pi`, deliberately not `dataset::funes_dir()`: pi stores
    // the install path by reference, so it can't follow a per-session FUNES_HOME (a demo points
    // that elsewhere and the dir may not outlive the install).
    let home = std::env::var_os("HOME").context("resolving $HOME for the pi install dir")?;
    let dir = PathBuf::from(home).join(".funes/integrations/pi");
    extract(&dir, memory.as_deref(), force)?;
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
            println!(
                "installed funes recall into pi{v} — `recall`/`get` are now available (restart pi if it's running)."
            );
            Ok(())
        }
        Ok(s) => anyhow::bail!(
            "`pi install {dir}` failed (exit {:?}); the extension is extracted there — retry that command manually.",
            s.code()
        ),
        Err(e) => Err(anyhow::Error::new(e).context("running `pi install`")),
    }
}

/// Reverse [`install`]: unregister the fixed-source extension from pi, then delete funes's extracted
/// copy. The local memory and pi's session traces are deliberately untouched.
pub fn uninstall() -> Result<()> {
    let home = std::env::var_os("HOME").context("resolving $HOME for the pi install dir")?;
    let dir = PathBuf::from(home).join(".funes/integrations/pi");
    let source = dir.display().to_string();
    if crate::integration::run_remove("pi", &["remove", &source], &["No matching package found for"])?
        == crate::integration::RemoveCommand::MissingCli
    {
        crate::integration::remove_tree(&dir)?;
        if let Some(parent) = dir.parent() {
            crate::integration::remove_empty_dir(parent)?;
        }
        println!(
            "`pi` isn't on PATH — extracted integration files were removed. Once it is, remove the registration manually:  pi remove {source}"
        );
        return Ok(());
    }
    crate::integration::remove_tree(&dir)?;
    if let Some(parent) = dir.parent() {
        crate::integration::remove_empty_dir(parent)?;
    }
    println!("removed funes from pi — extension registration and extracted integration files.");
    Ok(())
}

/// True if `path` exists and already holds exactly `want`.
fn file_matches(path: &Path, want: &str) -> bool {
    std::fs::read_to_string(path).map(|got| got == want).unwrap_or(false)
}

/// Write the embedded extension (index.ts + package.json) into `dir`, plus the `memory` file that
/// binds this install's recall (the extension reads it at startup; `None` = local, so the file is
/// absent). A copy that drifts from what this install would write — a newer embedded version, or a
/// different bound memory — is refreshed; `force` rewrites even when it matches.
fn extract(dir: &Path, memory: Option<&str>, force: bool) -> Result<()> {
    // Tidy up a pre-rename install: the extension now reads only the `memory` file, so a leftover
    // `store` binding is already inert — remove it anyway so nothing stale lingers on disk.
    // Best-effort, before the `current` check so it runs even when nothing else needs rewriting.
    let _ = std::fs::remove_file(dir.join("store"));
    let current = !force
        && file_matches(&dir.join("index.ts"), INDEX_TS)
        && file_matches(&dir.join("package.json"), PACKAGE_JSON)
        && memory_matches(&dir.join("memory"), memory);
    if current {
        println!("funes pi extension already current at {}", dir.display());
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("index.ts"), INDEX_TS).context("writing index.ts")?;
    std::fs::write(dir.join("package.json"), PACKAGE_JSON).context("writing package.json")?;
    write_memory(&dir.join("memory"), memory)?;
    Ok(())
}

/// Write the `memory` binding file (the memory on its own line), or remove it when `memory` is `None`
/// so the extension falls back to the local memory.
fn write_memory(path: &Path, memory: Option<&str>) -> Result<()> {
    match memory {
        Some(s) => std::fs::write(path, format!("{s}\n")).context("writing the pi memory binding"),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::Error::new(e).context("clearing the pi memory binding")),
        },
    }
}

/// True if the `memory` file at `path` already reflects `memory`: absent for `None` (local), else its
/// trimmed contents equal the wanted memory. Mirrors how the extension reads it.
fn memory_matches(path: &Path, memory: Option<&str>) -> bool {
    let current = std::fs::read_to_string(path).ok().map(|s| s.trim().to_string());
    match memory {
        Some(s) => current.as_deref() == Some(s),
        None => current.is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::{file_matches, memory_matches, parse_semver, write_memory, MIN_PI};

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
    fn memory_file_reflects_the_binding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory");

        // Local (None): the file is absent, and that is "current".
        assert!(memory_matches(&path, None));
        assert!(!memory_matches(&path, Some("acme/kb")));

        // Binding a memory writes it; then it matches that memory and no longer matches local.
        write_memory(&path, Some("acme/kb")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "acme/kb\n");
        assert!(memory_matches(&path, Some("acme/kb")));
        assert!(!memory_matches(&path, Some("other/kb")));
        assert!(!memory_matches(&path, None));

        // Rebinding to local removes the file (idempotent — removing an absent file is fine).
        write_memory(&path, None).unwrap();
        assert!(memory_matches(&path, None));
        write_memory(&path, None).unwrap();
    }

    /// The extension resolves its memory from a `memory` file next to index.ts — so the embedded
    /// source must actually read that file (guards against the two drifting apart).
    #[test]
    fn embedded_extension_reads_the_memory_file() {
        assert!(super::INDEX_TS.contains(r#""memory""#));
        assert!(super::INDEX_TS.contains("FUNES_MEMORY"));
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
