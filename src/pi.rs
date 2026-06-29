//! `funes install pi`: register funes recall as a first-class pi tool.
//!
//! pi has no MCP client, so funes ships a small pi extension (a bridge that
//! spawns `funes mcp` over stdio — see `integrations/pi/`). The extension is
//! embedded in the binary here, so once `funes` is on PATH a single
//! `funes install pi` extracts it and registers it with pi — no separate
//! package to fetch, and it always matches this binary's MCP surface.

use crate::dataset;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const INDEX_TS: &str = include_str!("../integrations/pi/index.ts");
const PACKAGE_JSON: &str = include_str!("../integrations/pi/package.json");

/// The pi version funes' extension API (the `pi.extensions` manifest + `registerTool`
/// + provided `typebox`) was validated against. Older pi may not load the extension.
const MIN_PI: (u32, u32, u32) = (0, 73, 0);

/// Extract the embedded pi extension and register it with pi. Defaults to the
/// current project; `global` installs it user-wide. `dest` overrides where the
/// extension is extracted (default: funes's home); `force` re-extracts even when
/// it's already there.
pub fn install(global: bool, dest: Option<PathBuf>, force: bool) -> Result<()> {
    let dir = dest.unwrap_or_else(|| dataset::funes_dir().join("integrations").join("pi"));
    let present = dir.join("index.ts").exists() && dir.join("package.json").exists();
    if present && !force {
        println!(
            "funes pi extension already at {} (use --force to refresh)",
            dir.display()
        );
    } else {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(dir.join("index.ts"), INDEX_TS).context("writing index.ts")?;
        std::fs::write(dir.join("package.json"), PACKAGE_JSON).context("writing package.json")?;
    }
    let dir = dir.to_string_lossy().into_owned();
    let scope = if global { "user" } else { "project" };

    // Probe pi: this confirms it's on PATH (else extract-and-instruct) and lets us flag a
    // version older than the one the extension API was validated against.
    let version = match Command::new("pi").arg("--version").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(_) => String::new(), // pi present but odd --version; proceed without a version
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let flag = if global { "" } else { "-l " };
            println!("extracted the funes pi extension to {dir}");
            println!("`pi` isn't on PATH — once it is, run:  pi install {flag}{dir}");
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

    // Let pi resolve its own settings location. pi records the package as a path
    // reference, so the extracted dir must persist — funes's home does.
    let mut cmd = Command::new("pi");
    cmd.arg("install");
    if !global {
        cmd.arg("-l");
    }
    cmd.arg(&dir);

    match cmd.status() {
        Ok(s) if s.success() => {
            let v = if version.is_empty() {
                String::new()
            } else {
                format!(" {version}")
            };
            println!("installed funes recall into pi{v} ({scope} scope) — `recall`/`get` are now available (restart pi if it's running).");
            Ok(())
        }
        Ok(s) => anyhow::bail!(
            "`pi install {dir}` failed (exit {:?}); the extension is extracted there — retry that command manually.",
            s.code()
        ),
        Err(e) => Err(anyhow::Error::new(e).context("running `pi install`")),
    }
}

/// Parse a leading `MAJOR.MINOR.PATCH` from pi's `--version` output (tolerates a `v`
/// prefix, extra tokens, and a pre-release/build suffix on the patch).
fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let tok = s.trim().trim_start_matches('v').split_whitespace().next()?;
    let mut parts = tok.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts
        .next()
        .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::{parse_semver, MIN_PI};

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
        assert!(parse_semver("0.72.9").unwrap() < MIN_PI);
        assert!(parse_semver("0.73.0").unwrap() >= MIN_PI);
        assert!(parse_semver("1.0.0").unwrap() >= MIN_PI);
    }
}
