//! `funes update`: replace the running binary in place with the latest release, bring every
//! installed integration to its newest release, and the version check behind `funes status`'s
//! "update available" notice.
//!
//! The root `VERSION` marker resolves latest to a tagged directory containing `VERSION`,
//! `SHA256SUMS`, and the binaries. The checksum and tagged version are verified before a binary is
//! made executable. Replacement uses a same-filesystem rename, so the live process keeps its old
//! inode and the next run picks up the new binary. Windows will not rename over a running program,
//! so there the old binary is renamed aside first, and deleted by a later update.

use super::add::{confirm_trust, setup_add};
use super::bind::migrate_bindings;
use crate::agents::registry;
use crate::hub::{release_bucket, verify_checksum};
use crate::memory::{self, lock};
use crate::traces::spool;
use anyhow::{anyhow, bail, Context, Result};
use hf_hub::buckets::BucketDownload;
use hf_hub::HFBucket;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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
} else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
    Some("funes-x86_64-windows.exe")
} else {
    None
};

/// Wait until no funes up to 1.6 writes the home's memory. Those lock it from the funes home, not
/// from the memory, so an index one started before an update replaced it would not exclude a
/// current writer. Holds the memory's lock meanwhile, so no current writer starts alongside it.
/// Temporary: drop it once no older binary can still be running.
pub async fn wait_for_older_writers() -> Result<()> {
    let mut waiting = false;
    let _memory = loop {
        if let Some(memory) = lock::MemoryLock::try_acquire()? {
            break memory;
        }
        announce_wait(&mut waiting).await;
    };
    while lock::older_writer_running()? {
        announce_wait(&mut waiting).await;
    }
    Ok(())
}

async fn announce_wait(waiting: &mut bool) {
    if !*waiting {
        eprintln!("waiting for a running funes index to finish…");
        *waiting = true;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
}

/// `funes update`: the binary, then every installed integration. A replaced binary is run to update
/// them, since its contract decides which releases they take; one this host cannot replace does not
/// hold them back.
pub async fn run(force: bool) -> Result<()> {
    // First: an older release that just replaced its binary hands over to this `funes update`.
    wait_for_older_writers().await?;
    let binary = binary(force).await;
    if let Ok(Some(exe)) = &binary {
        return hand_over(exe);
    }
    let agents = agents().await;
    binary.and(agents)
}

/// Run the binary at `exe` as `funes update`, in this process's place.
#[cfg(unix)]
fn hand_over(exe: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let e = Command::new(exe).arg("update").exec();
    Err(anyhow::Error::new(e).context(format!("running {} to update the integrations", exe.display())))
}

/// Run the binary at `exe` as `funes update`, and exit as it does: Windows has no exec, so it runs
/// as a child this process waits for.
#[cfg(windows)]
fn hand_over(exe: &Path) -> Result<()> {
    let status = Command::new(exe)
        .arg("update")
        .status()
        .with_context(|| format!("running {} to update the integrations", exe.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Fetch the latest release binary for this platform and replace the running executable in
/// place. Idempotent — with `force`, reinstalls even when already up to date. The executable it
/// replaced, or `None` when it was up to date.
async fn binary(force: bool) -> Result<Option<PathBuf>> {
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
        println!("funes {current} is up to date (latest release: {latest}).");
        return Ok(None);
    }

    // Replacing the live binary means renaming over its path, so the download must land on the
    // same filesystem — stage it in a temp dir beside the executable.
    let exe = std::env::current_exe().context("locating the running funes binary")?;
    let exe = std::fs::canonicalize(&exe).map(plain).unwrap_or(exe);
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("cannot determine the install directory of {}", exe.display()))?;
    let staging = tempfile::tempdir_in(dir).with_context(|| {
        format!(
            "creating a staging dir in {} — need write access there to update in place",
            dir.display()
        )
    })?;
    // Named as a program is on this platform: it is run before it is installed.
    let staged = staging.path().join(format!("funes{}", std::env::consts::EXE_SUFFIX));
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
        .with_context(|| format!("downloading {tag} from the funes release bucket"))?;

    let release_version = read_release_version(&tagged_version).with_context(|| format!("validating {tag}/VERSION"))?;
    if release_version != latest {
        bail!(
            "release metadata mismatch: {tag}/VERSION reports {release_version}, expected {latest}; update aborted and the installed binary was left unchanged"
        );
    }

    let installed = install_verified(&staged, &exe, &manifest, asset, &latest)?;
    println!("Updated {} ({current} → {installed}).", exe.display());
    println!("Running agents keep using the old funes until you restart them or start a new session.");
    Ok(Some(exe))
}

/// Bring every installed integration forward.
async fn agents() -> Result<()> {
    let root = registry::default_root()?;
    // A host from before the binding is bound as its agents were; one that cannot be is still
    // brought forward, each agent bound as it was.
    match migrate_bindings(&root, None).await {
        Ok(Some(disagreement)) => eprintln!("note: {disagreement}"),
        Ok(None) => {}
        Err(e) => eprintln!("note: this host could not be bound as an older funes bound its agents: {e:#}"),
    }
    let mut failed = Vec::new();
    for id in registry::registered_ids(&root)
        .into_iter()
        .filter(|id| spool::is_id(id))
    {
        match agent(&root, &id).await {
            Ok(done) => println!("{done}"),
            Err(e) => {
                eprintln!("the {id} integration could not be updated: {e:#}");
                failed.push(id);
            }
        }
    }
    if !failed.is_empty() {
        bail!("not updated: {}", failed.join(", "));
    }
    Ok(())
}

/// Bring `id`'s install to what its source holds now and re-run its setup, bound as it was — only
/// when that changed its files. What it did, as a line to print.
async fn agent(root: &Path, id: &str) -> Result<String> {
    // Named with `--from`, an install stays as named: the one copy not refreshed on every run.
    let record = registry::installed(root, id);
    if let Some(record) = record.as_ref().filter(|_| !registry::refreshes_on_every_run(root, id)) {
        return Ok(format!(
            "The {id} integration stays as installed from {} until `funes add {id} --from` names its source again.",
            record.origin
        ));
    }
    let fetched = match registry::provision(root, id, None).await {
        // A copy nothing recorded, that its source does not list: nothing to bring it forward to.
        Err(e) if record.is_none() && e.downcast_ref::<registry::Absent>().is_some() => {
            return Ok(format!("The {id} integration stays as placed: {e}."));
        }
        fetched => fetched?,
    };
    // Recorded from the same source: confirmed already, and up to date unless the installed copy changed.
    let recorded = fetched
        .as_ref()
        .is_some_and(|p| registry::is_recorded(root, id, &p.origin, &p.files));
    let intact = recorded && registry::verify_installed(root, id)? == registry::Verification::Intact;
    let Some(provisioned) = fetched.filter(|_| !intact) else {
        return Ok(format!("The {id} integration is up to date."));
    };
    if !recorded {
        confirm_trust(&root.join(id), &provisioned.manifest, &provisioned.provenance)?;
    }
    // Bound as the host is; on a host without the binding, as an older funes bound the agent.
    let bound = memory::binding()?.or(registry::binding(root, id)?);
    let integration = provisioned.install(root)?;
    setup_add(&integration, bound.as_deref())?;
    let to = integration
        .manifest
        .version
        .as_ref()
        .map(|v| format!(" to {v}"))
        .unwrap_or_default();
    Ok(format!("Updated the {id} integration{to}."))
}

/// Verify the staged binary, confirm its reported version, and atomically rename it over `exe`.
fn install_verified(staged: &Path, exe: &Path, manifest: &Path, asset: &str, expected_version: &str) -> Result<String> {
    verify_checksum(staged, manifest, asset)?;
    set_mode(staged, 0o755).context("making the new binary executable")?;

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
    set_mode(staged, mode_of(exe).unwrap_or(0o755)).context("setting the new binary's permissions")?;

    // Same-filesystem rename (staged sits in a temp dir beside exe), so this is atomic. The live
    // process holds the old inode; replacing the path it launched from is safe.
    replace(staged, exe).map_err(|e| {
        anyhow!(
            "could not replace {}: {e} — need write access to {} (re-run the install script, or with the right permissions)",
            exe.display(),
            exe.parent().unwrap_or(exe).display(),
        )
    })?;
    Ok(expected_version.to_string())
}

/// A resolved path as a user reads one. Windows resolves `C:\bin\funes.exe` to
/// `\\?\C:\bin\funes.exe`, a spelling no message need show: the prefix goes when a plain drive path
/// follows it.
fn plain(resolved: PathBuf) -> PathBuf {
    match resolved.to_str().and_then(|path| path.strip_prefix(r"\\?\")) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => resolved,
    }
}

/// A file's permission bits, where the platform has them: Windows has none, and a file there is a
/// program by its name.
#[cfg(unix)]
fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).ok().map(|m| m.permissions().mode() & 0o777)
}

#[cfg(windows)]
fn mode_of(_: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(windows)]
fn set_mode(_: &Path, _: u32) -> std::io::Result<()> {
    Ok(())
}

/// Put `staged` where `exe` is, atomically.
#[cfg(unix)]
fn replace(staged: &Path, exe: &Path) -> std::io::Result<()> {
    std::fs::rename(staged, exe)
}

/// Put `staged` where `exe` is. Windows will not rename over a running program but lets one be
/// renamed, so the old binary steps aside first, as `<exe>.<pid>.old`, numbered past one that
/// still runs, and comes back if the new one cannot take its place. It cannot be deleted while it
/// runs: the ones earlier updates left are deleted here, those still running excepted.
#[cfg(windows)]
fn replace(staged: &Path, exe: &Path) -> std::io::Result<()> {
    if let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().and_then(|n| n.to_str())) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let stale = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix(name))
                .is_some_and(|rest| rest.starts_with('.') && rest.ends_with(".old"));
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    if !exe.exists() {
        return std::fs::rename(staged, exe);
    }
    let pid = std::process::id();
    let old = (0u32..)
        .map(|n| {
            let mut old = exe.as_os_str().to_owned();
            old.push(if n == 0 {
                format!(".{pid}.old")
            } else {
                format!(".{pid}.{n}.old")
            });
            PathBuf::from(old)
        })
        .find(|old| !old.exists())
        .expect("a name no aside has");
    std::fs::rename(exe, &old)?;
    std::fs::rename(staged, exe).inspect_err(|_| {
        let _ = std::fs::rename(&old, exe);
    })
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
    use super::{install_verified, notice_for, parse_semver, read_release_version};
    use crate::hub::sha256_file;
    use std::path::{Path, PathBuf};

    fn write_manifest(path: &Path, binary: &Path, asset: &str) {
        let digest = hex::encode(sha256_file(binary).unwrap());
        std::fs::write(path, format!("{digest}  {asset}\n")).unwrap();
    }

    /// A downloaded binary in `dir` that reports `version`, creating `marker` first when one is
    /// named: a shell script on Unix, a batch file on Windows. Not yet executable, as a download
    /// is. Its path, and its text.
    fn staged_binary(dir: &Path, version: &str, marker: Option<&Path>) -> (PathBuf, String) {
        let (name, text) = if cfg!(windows) {
            let touch = marker.map_or(String::new(), |m| format!("type nul > \"{}\"\r\n", m.display()));
            ("staged.cmd", format!("@echo off\r\n{touch}echo funes {version}\r\n"))
        } else {
            let touch = marker.map_or(String::new(), |m| format!("touch {}\n", m.display()));
            ("staged", format!("#!/bin/sh\n{touch}echo 'funes {version}'\n"))
        };
        let staged = dir.join(name);
        std::fs::write(&staged, &text).unwrap();
        (staged, text)
    }

    #[test]
    fn install_verified_atomically_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let (staged, text) = staged_binary(dir.path(), "9.9.9", None);
        let manifest = dir.path().join("SHA256SUMS");
        write_manifest(&manifest, &staged, "funes-test");
        let exe = dir.path().join("funes");
        std::fs::write(&exe, "old binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        let installed = install_verified(&staged, &exe, &manifest, "funes-test", "9.9.9").unwrap();
        assert_eq!(installed, "9.9.9");
        assert!(!staged.exists());

        assert_eq!(std::fs::read_to_string(&exe).unwrap(), text);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    /// Windows will not rename over a running program, so the binary an update replaces steps
    /// aside, and the update after it deletes what that one left.
    #[cfg(windows)]
    #[test]
    fn a_replaced_binary_steps_aside_until_the_next_update() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("funes.exe");
        let asides = || {
            let mut names: Vec<String> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| n.ends_with(".old"))
                .collect();
            names.sort();
            names
        };
        let update = |text: &str| {
            let staged = dir.path().join("staged.exe");
            std::fs::write(&staged, text).unwrap();
            super::replace(&staged, &exe).unwrap();
            assert!(!staged.exists());
            assert_eq!(std::fs::read_to_string(&exe).unwrap(), text);
        };

        update("first");
        assert!(asides().is_empty(), "nothing steps aside on a first install");

        update("second");
        let aside = format!("funes.exe.{}.old", std::process::id());
        assert_eq!(asides(), vec![aside.clone()]);
        assert_eq!(std::fs::read_to_string(dir.path().join(&aside)).unwrap(), "first");

        // What another update left is deleted; a neighbour that only looks like it is not.
        std::fs::write(dir.path().join("funes.exe.1.old"), "stale").unwrap();
        std::fs::write(dir.path().join("other.exe.1.old"), "not funes's").unwrap();
        update("third");
        assert_eq!(asides(), vec![aside.clone(), "other.exe.1.old".to_string()]);
        assert_eq!(std::fs::read_to_string(dir.path().join(&aside)).unwrap(), "second");

        // An aside of this pid's that still runs, which cannot be deleted, is stepped past.
        use std::os::windows::fs::OpenOptionsExt;
        let running = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.path().join(&aside))
            .unwrap();
        update("fourth");
        let next = format!("funes.exe.{}.1.old", std::process::id());
        assert_eq!(asides(), vec![next.clone(), aside, "other.exe.1.old".to_string()]);
        assert_eq!(std::fs::read_to_string(dir.path().join(&next)).unwrap(), "third");
        drop(running);
    }

    #[test]
    fn checksum_mismatch_is_rejected_before_execution() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let (staged, _) = staged_binary(dir.path(), "9.9.9", Some(&marker));
        let manifest = dir.path().join("SHA256SUMS");
        std::fs::write(&manifest, format!("{}  funes-test\n", "0".repeat(64))).unwrap();
        let exe = dir.path().join("funes");
        std::fs::write(&exe, "old binary").unwrap();

        assert!(install_verified(&staged, &exe, &manifest, "funes-test", "9.9.9").is_err());
        assert!(!marker.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&staged).unwrap().permissions().mode() & 0o111, 0);
        }
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old binary");
    }

    #[test]
    fn reported_version_mismatch_does_not_replace() {
        let dir = tempfile::tempdir().unwrap();
        let (staged, _) = staged_binary(dir.path(), "9.9.8", None);
        let manifest = dir.path().join("SHA256SUMS");
        write_manifest(&manifest, &staged, "funes-test");
        let exe = dir.path().join("funes");
        std::fs::write(&exe, "old binary").unwrap();

        assert!(install_verified(&staged, &exe, &manifest, "funes-test", "9.9.9").is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old binary");
    }

    #[test]
    fn a_resolved_path_is_shown_without_the_windows_prefix() {
        let shown = |path: &str| super::plain(PathBuf::from(path));
        assert_eq!(shown(r"\\?\C:\bin\funes.exe"), PathBuf::from(r"C:\bin\funes.exe"));
        // A share has no plain spelling of the kind, and a Unix path no prefix.
        for kept in [
            r"\\?\UNC\server\share\funes.exe",
            "/usr/local/bin/funes",
            r"C:\bin\funes.exe",
        ] {
            assert_eq!(shown(kept), PathBuf::from(kept));
        }
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
}
