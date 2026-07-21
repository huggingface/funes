//! `funes update`: replace the running binary in place with the latest release, and the
//! version check behind `funes status`'s "update available" notice.
//!
//! The root `VERSION` marker resolves latest to a tagged directory containing `VERSION`,
//! `SHA256SUMS`, and the binaries. The checksum and tagged version are verified before a binary is
//! made executable. Replacement uses a same-filesystem rename, so the live process keeps its old
//! inode and the next run picks up the new binary.

use crate::hub;
use anyhow::{anyhow, bail, Context, Result};
use hf_hub::buckets::BucketDownload;
use hf_hub::HFBucket;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{File, Permissions};
use std::io::Read;
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
    let bucket = release_bucket(true)?;
    let latest = fetch_latest_version(&bucket)
        .await
        .context("checking the latest funes version")?;
    let tag = format!("v{latest}");

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
    let manifest = staging.path().join("SHA256SUMS");
    let tagged_version = staging.path().join("VERSION");

    println!("Downloading funes {latest} ({asset})…");
    bucket
        .download_files()
        .files(vec![
            BucketDownload::new(format!("{tag}/{asset}"), &staged),
            BucketDownload::new(format!("{tag}/SHA256SUMS"), &manifest),
            BucketDownload::new(format!("{tag}/VERSION"), &tagged_version),
        ])
        .send()
        .await
        .with_context(|| format!("downloading {tag} from the {BUCKET_OWNER}/{BUCKET_NAME} bucket"))?;

    let release_version = read_release_version(&tagged_version).with_context(|| format!("validating {tag}/VERSION"))?;
    if release_version != latest {
        bail!("release metadata for {tag} reports version {release_version} — nothing changed");
    }

    let installed = install_verified(&staged, &exe, &manifest, asset, &latest)?;
    println!("Updated {} ({current} → {installed}).", exe.display());
    println!("Running agents keep using the old funes until you restart them or start a new session.");
    Ok(())
}

/// Verify the staged binary, confirm its reported version, and atomically rename it over `exe`.
fn install_verified(staged: &Path, exe: &Path, manifest: &Path, asset: &str, expected_version: &str) -> Result<String> {
    verify_checksum(staged, manifest, asset)?;
    std::fs::set_permissions(staged, Permissions::from_mode(0o755)).context("making the new binary executable")?;

    let out = verify_runs(staged)?;
    if !out.status.success() {
        bail!("the downloaded binary failed to run (corrupt download, or wrong platform) — nothing changed");
    }
    let reported = String::from_utf8(out.stdout).context("reading the downloaded binary's version")?;
    if reported.trim() != format!("funes {expected_version}") {
        bail!("the downloaded binary does not report the expected version {expected_version} — nothing changed");
    }

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
    Ok(expected_version.to_string())
}

/// Verify `asset` against its one unambiguous entry in a strict SHA256SUMS manifest.
fn verify_checksum(path: &Path, manifest: &Path, asset: &str) -> Result<()> {
    let text = std::fs::read_to_string(manifest).context("reading SHA256SUMS")?;
    let expected = expected_digest(&text, asset)?;
    let actual = sha256_file(path)?;
    if actual != expected {
        bail!("checksum verification failed for {asset} — nothing changed");
    }
    Ok(())
}

fn expected_digest(manifest: &str, asset: &str) -> Result<[u8; 32]> {
    let mut names = HashSet::new();
    let mut expected = None;

    for (index, line) in manifest.lines().enumerate() {
        let line_number = index + 1;
        let mut fields = line.split_ascii_whitespace();
        let digest = fields
            .next()
            .ok_or_else(|| anyhow!("SHA256SUMS line {line_number} is empty"))?;
        let name = fields
            .next()
            .ok_or_else(|| anyhow!("SHA256SUMS line {line_number} has no asset name"))?;
        if fields.next().is_some() || name.contains('/') {
            bail!("SHA256SUMS line {line_number} is malformed");
        }
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("SHA256SUMS line {line_number} has an invalid digest");
        }
        if !names.insert(name) {
            bail!("SHA256SUMS contains duplicate entries for {name}");
        }
        if name == asset {
            let bytes = hex::decode(digest).context("decoding the release checksum")?;
            expected = Some(bytes.try_into().expect("a 64-character hex digest is 32 bytes"));
        }
    }

    expected.ok_or_else(|| anyhow!("SHA256SUMS does not contain {asset}"))
}

fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path).with_context(|| format!("opening {} for checksum verification", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {} for checksum verification", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
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
    let bucket = release_bucket(false).ok()?;
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
/// set (the bucket is public, so a token isn't required). `retries` is false for the fail-fast
/// status check, true for the update's default.
fn release_bucket(retries: bool) -> Result<HFBucket> {
    Ok(hub::client(hub::hf_token().as_deref(), retries)?.bucket(BUCKET_OWNER, BUCKET_NAME))
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
    read_release_version(&path).context("reading the VERSION marker")
}

fn read_release_version(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).context("reading release version metadata")?;
    let mut tokens = text.split_ascii_whitespace();
    let raw = tokens.next().ok_or_else(|| anyhow!("release version is empty"))?;
    if tokens.next().is_some() {
        bail!("release version must contain exactly one value");
    }
    let version = raw.strip_prefix('v').unwrap_or(raw);
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        bail!("release version must have the form MAJOR.MINOR.PATCH");
    }
    Ok(version.to_string())
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
    use super::{expected_digest, install_verified, notice_for, parse_semver, read_release_version, sha256_file};
    use std::path::Path;

    fn write_manifest(path: &Path, binary: &Path, asset: &str) {
        let digest = hex::encode(sha256_file(binary).unwrap());
        std::fs::write(path, format!("{digest}  {asset}\n")).unwrap();
    }

    #[test]
    fn install_verified_atomically_replaces() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        std::fs::write(&staged, "#!/bin/sh\necho 'funes 9.9.9'\n").unwrap();
        let manifest = dir.path().join("SHA256SUMS");
        write_manifest(&manifest, &staged, "funes-test");
        let exe = dir.path().join("funes");
        std::fs::write(&exe, "old binary").unwrap();
        std::fs::set_permissions(&exe, Permissions::from_mode(0o700)).unwrap();

        let installed = install_verified(&staged, &exe, &manifest, "funes-test", "9.9.9").unwrap();
        assert_eq!(installed, "9.9.9");
        assert!(!staged.exists());

        assert_eq!(
            std::fs::read_to_string(&exe).unwrap(),
            "#!/bin/sh\necho 'funes 9.9.9'\n"
        );
        assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn checksum_mismatch_is_rejected_before_execution() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        let marker = dir.path().join("executed");
        std::fs::write(
            &staged,
            format!("#!/bin/sh\ntouch {}\necho 'funes 9.9.9'\n", marker.display()),
        )
        .unwrap();
        let manifest = dir.path().join("SHA256SUMS");
        std::fs::write(&manifest, format!("{}  funes-test\n", "0".repeat(64))).unwrap();
        let exe = dir.path().join("funes");
        std::fs::write(&exe, "old binary").unwrap();

        assert!(install_verified(&staged, &exe, &manifest, "funes-test", "9.9.9").is_err());
        assert!(!marker.exists());
        assert_eq!(std::fs::metadata(&staged).unwrap().permissions().mode() & 0o111, 0);
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old binary");
    }

    #[test]
    fn reported_version_mismatch_does_not_replace() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        std::fs::write(&staged, "#!/bin/sh\necho 'funes 9.9.8'\n").unwrap();
        let manifest = dir.path().join("SHA256SUMS");
        write_manifest(&manifest, &staged, "funes-test");
        let exe = dir.path().join("funes");
        std::fs::write(&exe, "old binary").unwrap();

        assert!(install_verified(&staged, &exe, &manifest, "funes-test", "9.9.9").is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old binary");
    }

    #[test]
    fn checksum_manifest_is_strict_and_target_bound() {
        let a = "1".repeat(64);
        let b = "2".repeat(64);
        let valid = format!("{a}  funes-a\n{b}  funes-b\n");
        assert_eq!(expected_digest(&valid, "funes-b").unwrap(), [0x22; 32]);

        for malformed in [
            format!("{a}  funes-a extra\n"),
            format!("{}  funes-a\n", "A".repeat(64)),
            format!("{a}  nested/funes-a\n"),
            format!("{a}  funes-a\n{b}  funes-a\n"),
            "\n".to_string(),
        ] {
            assert!(expected_digest(&malformed, "funes-a").is_err(), "{malformed:?}");
        }
        assert!(expected_digest(&valid, "missing").is_err());
    }

    #[test]
    fn release_version_metadata_is_strict() {
        let dir = tempfile::tempdir().unwrap();
        let version = dir.path().join("VERSION");
        for (text, expected) in [("1.2.3\n", "1.2.3"), ("v1.2.3", "1.2.3")] {
            std::fs::write(&version, text).unwrap();
            assert_eq!(read_release_version(&version).unwrap(), expected);
        }
        for malformed in ["", "1.2", "1.2.3 extra", "1.2.x", "1..3"] {
            std::fs::write(&version, malformed).unwrap();
            assert!(read_release_version(&version).is_err(), "{malformed:?}");
        }
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
