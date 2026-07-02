//! `funes update`: replace the running binary in place with the latest release, and the
//! version check behind `funes status`'s "update available" notice.
//!
//! Both read from the same Hugging Face bucket `install.sh` and the binary download use — a
//! plain `VERSION` marker at the bucket root (written by the release workflow) for the latest
//! version, and `<asset>` for the binary — via hf-hub, so there's one host and one failure
//! mode. The self-replace is the standard unix trick: you can't open the running executable
//! for writing (ETXTBSY), but you can `rename` a freshly-downloaded file over its path — the
//! live process keeps its original inode, and the next run picks up the new binary.

use crate::hub;
use anyhow::{anyhow, bail, Context, Result};
use hf_hub::buckets::BucketDownload;
use hf_hub::{HFBucket, HFClient};
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// The public bucket (`huggingface/funes`) holding the latest binaries and the `VERSION` marker.
const BUCKET_OWNER: &str = "huggingface";
const BUCKET_NAME: &str = "funes";

/// Repo, for the build-from-source pointer on platforms with no prebuilt binary.
const REPO: &str = "https://github.com/huggingface/funes";

/// How long the CLI `funes status` version check waits before giving up (silently). Short so an
/// offline or slow Hub barely delays status; the update command itself has no such cap.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(3);

/// The release asset for *this build's* target — resolved at compile time so `funes update`
/// only ever fetches the same target triple it was built as (no accidental cross-grade), and
/// so an unsupported platform is `None` here rather than a runtime mis-detection. `None` ⇒ no
/// prebuilt binary; update points at build-from-source instead. Mirrors `install.sh`'s map.
const ASSET: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("funes-x86_64-linux")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("funes-aarch64-linux")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("funes-arm64-apple-darwin")
} else {
    None
};

/// `funes update`: fetch the latest release binary for this platform and replace the running
/// executable in place. Idempotent — with `force`, reinstalls even when already up to date.
pub async fn run(force: bool) -> Result<()> {
    let asset = ASSET.ok_or_else(|| {
        anyhow!(
            "no prebuilt funes binary for this platform ({}/{}) — build from source: {REPO}#building-from-source",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    })?;

    let current = env!("CARGO_PKG_VERSION");
    let bucket = release_bucket(None)?;
    let latest = fetch_latest_version(&bucket)
        .await
        .context("checking the latest funes version")?;

    // Compare when both parse; if either is unreadable, proceed rather than block an update.
    let up_to_date = matches!(
        (parse_semver(&latest), parse_semver(current)),
        (Some(l), Some(c)) if l <= c
    );
    if up_to_date && !force {
        println!("funes {current} is already up to date (latest release: {latest}). Re-run with --force to reinstall.");
        return Ok(());
    }

    // Replacing the live binary means renaming over its path, so the download must land on the
    // same filesystem — stage it in a temp dir beside the executable.
    let exe = std::env::current_exe().context("locating the running funes binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("cannot determine the install directory of {}", exe.display()))?;
    let staging = tempfile::tempdir_in(dir).with_context(|| {
        format!(
            "creating a staging dir in {} — need write access there to update in place",
            dir.display()
        )
    })?;
    let staged = staging.path().join("funes");

    println!("Downloading funes {latest} ({asset})…");
    bucket
        .download_files()
        .files(vec![BucketDownload::new(asset, &staged)])
        .send()
        .await
        .with_context(|| format!("downloading {asset} from the {BUCKET_OWNER}/{BUCKET_NAME} bucket"))?;

    let installed = install_over(&staged, &exe)?;
    println!("Updated {} ({current} → {installed}).", exe.display());
    println!("Running agents keep using the old funes until you restart them or start a new session.");
    Ok(())
}

/// Make the staged binary runnable, run it once to confirm it executes on this platform, then
/// atomically rename it over `exe` with the existing binary's permissions. Returns the version the
/// new binary reports. A truncated, corrupt, or wrong-arch download fails the verify step and
/// never lands on PATH.
fn install_over(staged: &Path, exe: &Path) -> Result<String> {
    std::fs::set_permissions(staged, Permissions::from_mode(0o755)).context("making the new binary executable")?;

    let out = verify_runs(staged)?;
    if !out.status.success() {
        bail!("the downloaded binary failed to run (corrupt download, or wrong platform) — nothing changed");
    }
    let installed = String::from_utf8_lossy(&out.stdout)
        .trim()
        .trim_start_matches("funes")
        .trim()
        .to_string();

    // Preserve the existing binary's mode so an update never broadens it (e.g. 0700 → 0755); fall
    // back to 0755 when there's no existing binary to copy the mode from.
    let mode = std::fs::metadata(exe)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o755);
    std::fs::set_permissions(staged, Permissions::from_mode(mode)).context("setting the new binary's permissions")?;

    // Same-filesystem rename (staged sits in a temp dir beside exe), so this is atomic. The live
    // process holds the old inode; replacing the path it launched from is safe.
    std::fs::rename(staged, exe).map_err(|e| {
        anyhow!(
            "could not replace {}: {e} — need write access to {} (re-run the install script, or with the right permissions)",
            exe.display(),
            exe.parent().unwrap_or(exe).display(),
        )
    })?;
    Ok(installed)
}

/// Run `<path> --version`, retrying briefly on `ETXTBSY`. A just-written executable can
/// transiently report "text file busy" if another thread forked while its writable fd was
/// still open (children inherit the fd until they exec); a short retry rides that window out.
fn verify_runs(path: &Path) -> Result<std::process::Output> {
    let mut last = None;
    for _ in 0..5 {
        match Command::new(path).arg("--version").output() {
            Ok(out) => return Ok(out),
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(100));
                last = Some(e);
            }
            Err(e) => return Err(e).context("running the downloaded binary to verify it"),
        }
    }
    Err(last.unwrap()).context("running the downloaded binary to verify it (still busy after retries)")
}

/// The one-line "update available" notice for `funes status`, or `None` when up to date,
/// offline, or the check fails. Never errors and never blocks for long — a short timeout and
/// any failure just yield no notice, so the check can't break or stall status.
pub async fn upgrade_notice() -> Option<String> {
    let bucket = release_bucket(Some(0)).ok()?;
    let latest = match tokio::time::timeout(NOTICE_TIMEOUT, fetch_latest_version(&bucket)).await {
        Ok(Ok(v)) => v,
        _ => return None,
    };
    notice_for(&latest, env!("CARGO_PKG_VERSION"))
}

/// Pure core of [`upgrade_notice`]: the notice text when `latest_raw` is a strictly newer
/// release than `current_raw`, else `None`. Build metadata (`+dev`) is ignored by
/// [`parse_semver`], so a dev build of the current release doesn't nag; an unparseable
/// version yields no notice (fail quiet, never a false alarm).
fn notice_for(latest_raw: &str, current_raw: &str) -> Option<String> {
    let latest = parse_semver(latest_raw)?;
    let current = parse_semver(current_raw)?;
    (latest > current).then(|| {
        format!(
            "update available: funes {} (you have {current_raw}) — run `funes update`; restart running agents/MCP servers afterward to load it.\n",
            latest_raw.trim().trim_start_matches('v'),
        )
    })
}

/// An [`HFBucket`] handle for the funes release bucket, with the standard HF token if one is
/// set (the bucket is public, so a token isn't required). `retry_max_attempts` caps hf-hub's
/// retry loop — `Some(0)` for the fail-fast status check, `None` for the update's default.
fn release_bucket(retry_max_attempts: Option<usize>) -> Result<HFBucket> {
    let mut builder = HFClient::builder();
    if let Some(n) = retry_max_attempts {
        builder = builder.retry_max_attempts(n);
    }
    if let Some(token) = hub::hf_token() {
        builder = builder.token(token);
    }
    let client = builder.build().context("building the Hugging Face client")?;
    Ok(client.bucket(BUCKET_OWNER, BUCKET_NAME))
}

/// Download the bucket's `VERSION` marker (the latest published release) into a scratch dir and
/// return it trimmed.
async fn fetch_latest_version(bucket: &HFBucket) -> Result<String> {
    let scratch = tempfile::tempdir().context("creating a temp dir for the VERSION marker")?;
    let path = scratch.path().join("VERSION");
    bucket
        .download_files()
        .files(vec![BucketDownload::new("VERSION", &path)])
        .send()
        .await
        .context("downloading the VERSION marker from the bucket")?;
    let text = std::fs::read_to_string(&path).context("reading the VERSION marker")?;
    Ok(text.trim().to_string())
}

/// Parse a leading `MAJOR.MINOR.PATCH` (tolerates a `v` prefix, extra tokens, and a
/// pre-release/build suffix on the patch, e.g. `0.8.0+dev` → `(0, 8, 0)`).
pub(crate) fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
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
    use super::{install_over, notice_for, parse_semver};

    #[test]
    fn install_over_verifies_then_atomically_replaces() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        // A stand-in "binary" that reports a version, like `funes --version` would.
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        std::fs::write(&staged, "#!/bin/sh\necho 'funes 9.9.9'\n").unwrap();
        let exe = dir.path().join("funes");
        std::fs::write(&exe, "old binary").unwrap();
        // The existing binary is user-only (0700): the update must preserve this, not broaden it.
        std::fs::set_permissions(&exe, Permissions::from_mode(0o700)).unwrap();

        // install_over makes it runnable, runs it to verify, and renames it over the target.
        let installed = install_over(&staged, &exe).unwrap();
        assert_eq!(installed, "9.9.9"); // reported version, `funes` prefix stripped (so verify ran it)
        assert!(!staged.exists()); // staged was moved, not copied

        // Compared by content, not a second exec (which would reintroduce the fork/exec race under
        // parallel tests): the target now holds the staged binary, with the preserved 0700 mode.
        assert_eq!(
            std::fs::read_to_string(&exe).unwrap(),
            "#!/bin/sh\necho 'funes 9.9.9'\n"
        );
        assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn install_over_rejects_a_binary_that_will_not_run() {
        // A non-executable, non-binary staged file: the verify step must fail and leave the
        // target untouched — a bad download never lands on PATH.
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        std::fs::write(&staged, "not a real binary").unwrap();
        let exe = dir.path().join("funes");
        std::fs::write(&exe, "old binary").unwrap();

        assert!(install_over(&staged, &exe).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old binary"); // untouched
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_semver("0.8.0"), Some((0, 8, 0)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("0.8.0+dev"), Some((0, 8, 0))); // build metadata ignored
        assert_eq!(parse_semver("0.8.0-beta.1"), Some((0, 8, 0)));
        assert_eq!(parse_semver("0.8"), Some((0, 8, 0)));
        assert_eq!(parse_semver("1.0.0 (abc123)"), Some((1, 0, 0)));
        assert_eq!(parse_semver("not-a-version"), None);
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn notice_only_when_strictly_newer() {
        // newer release available → notice, naming both versions and the command
        let n = notice_for("0.8.1", "0.8.0").unwrap();
        assert!(n.contains("0.8.1") && n.contains("0.8.0") && n.contains("funes update"));
        // a `v` prefix on the published version is stripped in the message
        assert!(notice_for("v0.9.0", "0.8.0").unwrap().contains("funes 0.9.0"));
        // equal, and older, → no notice
        assert_eq!(notice_for("0.8.0", "0.8.0"), None);
        assert_eq!(notice_for("0.8.0", "0.9.0"), None);
    }

    #[test]
    fn dev_build_of_current_release_does_not_nag() {
        // running a `+dev` build of the current release → not behind
        assert_eq!(notice_for("0.8.0", "0.8.0+dev"), None);
        // but a real newer release still notifies a dev build
        assert!(notice_for("0.8.1", "0.8.0+dev").is_some());
    }

    #[test]
    fn unreadable_version_yields_no_false_alarm() {
        assert_eq!(notice_for("garbage", "0.8.0"), None);
        assert_eq!(notice_for("0.8.1", "garbage"), None);
    }
}
