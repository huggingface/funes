//! The registry of agent integrations: where one lives, what it declares, and how funes runs it.
//!
//! An integration is a directory `<root>/<id>/` holding a `manifest.json` and an executable
//! `setup`, run as `setup add [MEMORY]` or `setup remove` with `$FUNES_BIN`, `$FUNES_HOME` and
//! `$FUNES_AGENT_ID` in its environment.

use anyhow::{bail, Context, Result};
use hf_hub::buckets::BucketDownload;
use hf_hub::HFError;
use serde::Deserialize;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::hub;
use crate::memory::dataset;
use crate::traces::spool;

/// The integration contract this funes speaks.
pub const CONTRACT_VERSION: u32 = 1;

/// The executable every integration provides: `setup add [MEMORY]`, `setup remove`.
const SETUP: &str = "setup";

/// What an integration declares about itself, in `manifest.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub contract_version: u32,
    pub id: String,
    /// The agent's name as a human writes it.
    pub label: String,
    /// Where it is published from, `<publisher>/<name>`: with `id`, what the package is.
    pub repo: String,
    /// Its own release, `MAJOR.MINOR.PATCH`, moving independently of the contract.
    #[serde(default)]
    pub version: Option<String>,
}

impl Manifest {
    /// Who publishes it: the owner in `repo`.
    pub fn publisher(&self) -> &str {
        self.repo.split('/').next().unwrap_or_default()
    }
}

/// `<publisher>/<name>`, both non-empty.
fn is_repo(s: &str) -> bool {
    matches!(s.split_once('/'), Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/'))
}

/// `MAJOR.MINOR.PATCH`, digits only.
fn is_release_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// A resolved integration: its directory and what it declares.
#[derive(Debug)]
pub struct Integration {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// The registry root, `~/.funes/agents` — fixed, not under `$FUNES_HOME`: an agent records the
/// install path it is handed, so these files must outlive any one home.
pub fn default_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("resolving $HOME for the agent registry")?;
    Ok(PathBuf::from(home).join(".funes/agents"))
}

/// The ids in `root`: every directory holding a `manifest.json`, sorted. Unvalidated.
pub fn registered_ids(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join("manifest.json").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    ids.sort();
    ids
}

/// The registered integrations whose manifest speaks another contract than this funes, with the
/// contract each speaks: every one was installed by another binary, and what its hooks do may not
/// be what this one expects. A manifest that does not parse is not counted; `open` reports it.
pub fn mismatched(root: &Path) -> Vec<(String, u32)> {
    registered_ids(root)
        .into_iter()
        .filter(|id| spool::is_id(id))
        .filter_map(|id| {
            let text = std::fs::read_to_string(root.join(&id).join("manifest.json")).ok()?;
            let manifest: Manifest = serde_json::from_str(&text).ok()?;
            (manifest.contract_version != CONTRACT_VERSION).then_some((id, manifest.contract_version))
        })
        .collect()
}

/// Resolve `id` in `root` and check what it declares. Every refusal happens here, before `setup`
/// runs.
pub fn open(root: &Path, id: &str) -> Result<Integration> {
    let dir = root.join(id);
    let manifest_path = dir.join("manifest.json");
    if !manifest_path.is_file() {
        let known = registered_ids(root);
        let listing = if known.is_empty() {
            "no agents are registered".to_string()
        } else {
            format!("registered: {}", known.join(", "))
        };
        bail!("{id} is not a registered agent ({listing})");
    }

    let manifest = read_manifest(&manifest_path, id)?;

    let setup = dir.join(SETUP);
    let meta = std::fs::metadata(&setup).with_context(|| format!("{id} has no {SETUP} at {}", setup.display()))?;
    if meta.permissions().mode() & 0o111 == 0 {
        bail!("{} is not executable", setup.display());
    }
    owned_by_me(root, &setup, 0o022)?;

    Ok(Integration { dir, manifest })
}

/// Read what the manifest at `path` declares for the integration `id`, and check it. Every
/// refusal of a declaration happens here, the same for files installed and files about to be.
fn read_manifest(path: &Path, id: &str) -> Result<Manifest> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: Manifest = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    if manifest.contract_version != CONTRACT_VERSION {
        bail!(
            "the {id} integration needs funes contract version {}, and this funes speaks \
             {CONTRACT_VERSION} — run `funes update`, or install a {id} integration built for \
             contract {CONTRACT_VERSION}.",
            manifest.contract_version
        );
    }
    if manifest.id != id {
        bail!(
            "{} declares the id {:?} — an integration's id is its directory name",
            path.display(),
            manifest.id
        );
    }
    if !spool::is_id(&manifest.id) {
        bail!("the integration id {:?} must be lowercase [a-z0-9_-]", manifest.id);
    }
    if !is_repo(&manifest.repo) {
        bail!(
            "{} declares the repo {:?} — an integration is published from `<publisher>/<name>`",
            path.display(),
            manifest.repo
        );
    }
    if let Some(version) = &manifest.version {
        if !is_release_version(version) {
            bail!(
                "{} declares the version {:?} — a release is `MAJOR.MINOR.PATCH`",
                path.display(),
                version
            );
        }
    }
    Ok(manifest)
}

/// Refuse a `path` that someone other than the user running funes could have written, checking
/// every directory from `top` down as well as the file itself: funes is about to execute what it
/// finds there. `others` is the mode bits that give it away — group and world write for the
/// registry, world write alone for a checkout, whose files a private group's umask leaves
/// group-writable. Below `top`, a link is refused rather than followed: what it points at has
/// parents of its own that this walk would never see.
fn owned_by_me(top: &Path, path: &Path, others: u32) -> Result<()> {
    // SAFETY: `geteuid` reads a process attribute and cannot fail.
    let me = unsafe { libc::geteuid() };
    let mut path = path.to_path_buf();
    loop {
        let meta = if path == top {
            std::fs::metadata(&path)
        } else {
            std::fs::symlink_metadata(&path)
        }
        .with_context(|| format!("reading {}", path.display()))?;
        if meta.file_type().is_symlink() {
            bail!(
                "{} is a symlink — funes will not run through it; remove it and reinstall",
                path.display()
            );
        }
        if meta.permissions().mode() & others != 0 {
            bail!(
                "{} is writable by other users — funes will not run it; `chmod go-w` it or reinstall",
                path.display()
            );
        }
        if meta.uid() != me {
            bail!(
                "{} is owned by uid {} rather than you — funes will not run it",
                path.display(),
                meta.uid()
            );
        }
        if path == top {
            return Ok(());
        }
        match path.parent() {
            Some(parent) => path = parent.to_path_buf(),
            None => return Ok(()),
        }
    }
}

/// The manifest `root/<id>` holds, when one is installed.
pub fn installed_manifest(root: &Path, id: &str) -> Option<Vec<u8>> {
    std::fs::read(root.join(id).join("manifest.json")).ok()
}

/// Put `manifest` back as `root/<id>`'s: the registry's manifest says what its `setup` last
/// installed, so a refresh whose `setup add` never ran must not leave the new one.
pub fn restore_manifest(root: &Path, id: &str, manifest: &[u8]) -> Result<()> {
    let dir = root.join(id);
    if !dir.is_dir() {
        return Ok(());
    }
    let path = dir.join("manifest.json");
    if std::fs::read(&path).is_ok_and(|now| now == manifest) {
        return Ok(());
    }
    let tmp = dir.join(format!(".manifest.json.funes-tmp{}", std::process::id()));
    std::fs::write(&tmp, manifest).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))
}

impl Integration {
    /// Install funes into the agent, bound to `memory`; absent is the local memory.
    pub fn add(&self, memory: Option<&str>) -> Result<()> {
        match memory {
            Some(m) => self.run(&["add", m]),
            None => self.run(&["add"]),
        }
    }

    /// Remove funes from the agent.
    pub fn remove(&self) -> Result<()> {
        self.run(&["remove"])
    }

    /// Exec `setup` with the contract's environment. A non-zero exit fails the command.
    fn run(&self, args: &[&str]) -> Result<()> {
        let setup = self.dir.join(SETUP);
        let command = super::shell_command(&setup.to_string_lossy(), args);
        // What the integration records or invokes: the user's pin, else `funes` from PATH.
        let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
        let status = Command::new(&setup)
            .args(args)
            .env("FUNES_BIN", &funes)
            .env("FUNES_HOME", dataset::funes_dir())
            .env("FUNES_AGENT_ID", &self.manifest.id)
            .status()
            .with_context(|| format!("running `{command}`"))?;
        if !status.success() {
            bail!("`{command}` failed (exit {:?})", status.code());
        }
        Ok(())
    }
}

/// Where an integration's files come from.
enum Source {
    /// The `integrations/` directory of the checkout this binary was built from.
    Checkout(PathBuf),
    /// A directory `$FUNES_INTEGRATIONS` points at.
    Redirected(PathBuf),
    /// The archive published in the release bucket.
    Published,
}

/// No files for an id anywhere funes looks: `$FUNES_INTEGRATIONS` holds none, or the release bucket
/// publishes none for this contract. A source funes could not reach is any other error.
#[derive(Debug)]
pub struct Absent(String);

impl std::fmt::Display for Absent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Absent {}

/// Whether funes vouches for the files it installed: a published archive it verified, or the
/// checkout it was built from. Anything else is someone's files on this disk, and the caller
/// confirms before funes executes them.
#[derive(Debug)]
pub enum Provenance {
    Vouched,
    /// Where they came from, for the confirmation.
    Unvouched(String),
}

/// Resolve `id`'s files: `$FUNES_INTEGRATIONS` if set — authoritative, so a test or a fork cannot
/// reach the network by accident — else the checkout this binary was built from, else the bucket.
/// A released binary's build path does not exist where it runs, so it falls through; and what sits
/// at that path is vouched for only while it is the user's own, since anyone could have put a tree
/// there once the checkout is gone.
fn source_for(id: &str) -> Result<Source> {
    if let Some(dir) = std::env::var_os("FUNES_INTEGRATIONS") {
        let dir = PathBuf::from(dir).join(id);
        if !dir.is_dir() {
            return Err(Absent(format!("$FUNES_INTEGRATIONS holds no {id}")).into());
        }
        return Ok(Source::Redirected(dir));
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let checkout = manifest_dir.join("integrations").join(id);
    if checkout.is_dir() {
        match owned_by_me(manifest_dir, &checkout.join(SETUP), 0o002) {
            Ok(()) => return Ok(Source::Checkout(checkout)),
            Err(e) => eprintln!(
                "note: the checkout at {} is not yours alone ({e:#}) — using the published {id} integration instead.",
                checkout.display()
            ),
        }
    }
    Ok(Source::Published)
}

/// The bucket prefix for the contract this funes speaks: an integration fix reaches installed
/// binaries without a release, and a binary only reads the layout it understands.
fn published_prefix() -> String {
    format!("integrations/v{CONTRACT_VERSION}")
}

/// Install `id`'s files into the registry. Only a file that differs is rewritten (`force` rewrites
/// regardless), and nothing is pruned — an integration's `setup` keeps its own state beside them.
/// The id names the directory written, so it is checked here, before anything is.
pub async fn provision(root: &Path, id: &str, force: bool) -> Result<Provenance> {
    if !spool::is_id(id) {
        bail!("{id:?} is not an integration id (lowercase [a-z0-9_-])");
    }
    let dst = root.join(id);
    match source_for(id)? {
        Source::Checkout(src) => {
            copy_into(&src, &dst, force)?;
            Ok(Provenance::Vouched)
        }
        Source::Redirected(src) => {
            copy_into(&src, &dst, force)?;
            Ok(Provenance::Unvouched(format!(
                "$FUNES_INTEGRATIONS ({})",
                src.display()
            )))
        }
        Source::Published => {
            let staging = tempfile::tempdir().context("creating a staging directory")?;
            let archive = fetch_published(id, staging.path()).await?;
            let unpacked = staging.path().join("unpacked");
            unpack(&archive, &unpacked)?;
            copy_into(&unpacked, &dst, force)?;
            Ok(Provenance::Vouched)
        }
    }
}

/// Download `id`'s published archive into `dir` and check it against the prefix's `SHA256SUMS`.
async fn fetch_published(id: &str, dir: &Path) -> Result<PathBuf> {
    let asset = format!("{id}.tar.gz");
    let archive = dir.join(&asset);
    let manifest = dir.join("SHA256SUMS");
    let prefix = published_prefix();
    eprintln!("fetching the {id} integration…");
    hub::release_bucket(true)?
        .download_files()
        .files(vec![
            BucketDownload::new(format!("{prefix}/{asset}"), &archive),
            BucketDownload::new(format!("{prefix}/SHA256SUMS"), &manifest),
        ])
        .send()
        .await
        .map_err(|e| match e {
            HFError::EntryNotFound { .. } => Absent(format!(
                "the funes release bucket publishes no {id} integration for contract {CONTRACT_VERSION}"
            ))
            .into(),
            e => anyhow::Error::from(e).context(format!("downloading {prefix}/{asset} from the funes release bucket")),
        })?;
    hub::verify_checksum(&archive, &manifest, &asset)?;
    Ok(archive)
}

/// Unpack a verified archive: an integration's files sit at its root.
fn unpack(archive: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dir)
        .status()
        .context("running tar to unpack the integration")?;
    if !status.success() {
        bail!("unpacking {} failed (exit {:?})", archive.display(), status.code());
    }
    Ok(())
}

/// Delete an integration's directory, including the state its `setup` wrote there.
pub fn discard(root: &Path, id: &str) -> Result<()> {
    super::remove_tree(&root.join(id))
}

/// Create `dir`, and any parent missing, as funes's own: 0755 whatever the umask, since `open`
/// refuses a registry anyone else could write to and a umask of 002 would make one. A directory
/// that already exists is left as it is, to be judged then.
fn create_owned(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Refuse a source tree an integration may not carry before any of it is copied: a link to a
/// directory could name any tree on the disk. A link to a file is followed and copied as a file.
fn check_source(src: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let from = entry?.path();
        if from.is_dir() {
            if from.symlink_metadata()?.file_type().is_symlink() {
                bail!(
                    "{} is a symlink to a directory, which an integration may not carry",
                    from.display()
                );
            }
            check_source(&from)?;
        }
    }
    Ok(())
}

fn copy_into(src: &Path, dst: &Path, force: bool) -> Result<()> {
    check_source(src)?;
    copy_tree(src, dst, force)
}

fn copy_tree(src: &Path, dst: &Path, force: bool) -> Result<()> {
    // Nothing is written through a link at the destination: a directory's would send the whole
    // copy wherever it points, so it is refused; a file's is replaced below.
    if dst.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        bail!(
            "{} is a symlink — funes will not write through it; remove it and retry",
            dst.display()
        );
    }
    create_owned(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let from = entry?.path();
        let name = from.file_name().expect("a directory entry has a file name").to_owned();
        let to = dst.join(&name);
        let meta = std::fs::metadata(&from).with_context(|| format!("reading {}", from.display()))?;
        if meta.is_dir() {
            copy_tree(&from, &to, force)?;
            continue;
        }
        let bytes = std::fs::read(&from).with_context(|| format!("reading {}", from.display()))?;
        // The source's mode, closed to others: `open` refuses a file others could write, and a
        // private group's umask leaves a checkout's files group-writable.
        let mode = std::fs::Permissions::from_mode(meta.permissions().mode() & 0o777 & !0o022);
        // Written beside and renamed over: a link left at the destination is replaced, never
        // followed to wherever it points, and a reader never sees a half-written file.
        let stale_link = to.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
        if force || stale_link || std::fs::read(&to).map(|old| old != bytes).unwrap_or(true) {
            let tmp = dst.join(format!(".{}.funes-tmp{}", name.to_string_lossy(), std::process::id()));
            let written = std::fs::write(&tmp, &bytes)
                .with_context(|| format!("writing {}", tmp.display()))
                .and_then(|()| {
                    std::fs::set_permissions(&tmp, mode)
                        .with_context(|| format!("setting the mode of {}", tmp.display()))
                })
                .and_then(|()| std::fs::rename(&tmp, &to).with_context(|| format!("replacing {}", to.display())));
            if written.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            written?;
        } else {
            std::fs::set_permissions(&to, mode).with_context(|| format!("setting the mode of {}", to.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, contract: u32) -> String {
        format!(r#"{{"contract_version": {contract}, "id": "{id}", "label": "An Agent", "repo": "acme/funes-pi"}}"#)
    }

    /// Write an integration at `root/<dir_name>` declaring `manifest`, whose `setup` runs `body`.
    fn integration(root: &Path, dir_name: &str, manifest: &str, body: &str) -> PathBuf {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), manifest).unwrap();
        let setup = dir.join(SETUP);
        std::fs::write(&setup, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    /// The whole `add` contract in one run: the argv, then the environment.
    #[test]
    fn a_setup_others_could_have_written_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = integration(root.path(), "pi", &manifest("pi", CONTRACT_VERSION), "true");
        assert!(open(root.path(), "pi").is_ok());

        let setup = dir.join(SETUP);
        std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = open(root.path(), "pi").unwrap_err().to_string();
        assert!(err.contains("writable by other users"), "{err}");

        // A directory above it counts too: write access there is write access to the file.
        std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = open(root.path(), "pi").unwrap_err().to_string();
        assert!(err.contains("writable by other users"), "{err}");
    }

    /// Packed here the way the release workflow packs it.
    #[test]
    fn a_published_archive_installs_with_its_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("scripts")).unwrap();
        std::fs::write(src.join("manifest.json"), "{}").unwrap();
        std::fs::write(src.join("setup"), "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(src.join("setup"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(src.join("scripts/funes-index.sh"), "shared").unwrap();

        let archive = tmp.path().join("pi.tar.gz");
        let packed = Command::new("tar")
            .arg("-czhf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg(".")
            .status()
            .unwrap();
        assert!(packed.success());

        let unpacked = tmp.path().join("unpacked");
        unpack(&archive, &unpacked).unwrap();
        let dst = tmp.path().join("registry/pi");
        copy_into(&unpacked, &dst, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("scripts/funes-index.sh")).unwrap(),
            "shared"
        );
        assert!(
            std::fs::metadata(dst.join("setup")).unwrap().permissions().mode() & 0o111 != 0,
            "setup arrives executable"
        );
        assert!(
            open(&tmp.path().join("registry"), "pi").is_err(),
            "that manifest declares nothing"
        );
    }

    #[test]
    fn copying_a_checkout_follows_links_keeps_modes_and_keeps_local_state() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared.sh");
        std::fs::write(&shared, "shared v1").unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();

        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("scripts")).unwrap();
        std::fs::write(src.join("manifest.json"), "{}").unwrap();
        std::os::unix::fs::symlink(&shared, src.join("scripts/shared.sh")).unwrap();

        let dst = tmp.path().join("dst");
        copy_into(&src, &dst, false).unwrap();

        let copied = dst.join("scripts/shared.sh");
        assert!(
            !copied.symlink_metadata().unwrap().file_type().is_symlink(),
            "copied as a file"
        );
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "shared v1");
        assert!(std::fs::metadata(&copied).unwrap().permissions().mode() & 0o111 != 0);

        // The integration's own state, and a second run that must not disturb it.
        std::fs::write(dst.join("memory"), "acme/kb\n").unwrap();
        std::fs::write(&shared, "shared v2").unwrap();
        copy_into(&src, &dst, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&copied).unwrap(),
            "shared v2",
            "drift is refreshed"
        );
        assert_eq!(std::fs::read_to_string(dst.join("memory")).unwrap(), "acme/kb\n");

        // A link left where a file goes is replaced, not written through: the file it pointed at
        // is untouched, and the bundle's file is a file.
        let elsewhere = tmp.path().join("elsewhere.json");
        std::fs::write(&elsewhere, "not the bundle's").unwrap();
        std::fs::remove_file(dst.join("manifest.json")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dst.join("manifest.json")).unwrap();
        copy_into(&src, &dst, false).unwrap();
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "not the bundle's");
        let manifest = dst.join("manifest.json");
        assert!(!manifest.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&manifest).unwrap(), "{}");

        // A link where a directory goes is refused outright, and nothing lands where it points.
        std::fs::remove_dir_all(dst.join("scripts")).unwrap();
        let elsewhere_dir = tmp.path().join("elsewhere-dir");
        std::fs::create_dir_all(&elsewhere_dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere_dir, dst.join("scripts")).unwrap();
        let err = copy_into(&src, &dst, false).unwrap_err().to_string();
        assert!(err.contains("is a symlink"), "{err}");
        assert!(
            std::fs::read_dir(&elsewhere_dir).unwrap().next().is_none(),
            "nothing written through"
        );
        std::fs::remove_file(dst.join("scripts")).unwrap();

        // A link to a directory in the source is refused before anything is copied.
        std::fs::write(&shared, "shared v3").unwrap();
        std::os::unix::fs::symlink(tmp.path(), src.join("everything")).unwrap();
        let err = copy_into(&src, &dst, false).unwrap_err().to_string();
        assert!(err.contains("symlink to a directory"), "{err}");
        assert!(!dst.join("scripts/shared.sh").exists(), "refused before a byte moved");
    }

    /// A private group's umask leaves a checkout's files group-writable; the registry's copy is
    /// closed to others, or `open` would refuse what funes itself just wrote.
    #[test]
    fn a_copied_file_is_closed_to_other_writers() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("setup"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(src.join("setup"), std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::write(src.join("manifest.json"), "{}").unwrap();
        std::fs::set_permissions(src.join("manifest.json"), std::fs::Permissions::from_mode(0o666)).unwrap();

        let dst = tmp.path().join("dst");
        copy_into(&src, &dst, false).unwrap();
        assert_eq!(
            std::fs::metadata(dst.join("setup")).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(dst.join("manifest.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert!(owned_by_me(&dst, &dst.join("setup"), 0o022).is_ok());
    }

    /// Ownership is judged against the user running funes, file by file up to the top, and by the
    /// bits that matter there: group write is fine for a checkout, not for the registry.
    #[test]
    fn ownership_is_the_running_users_up_to_the_top() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().join("top");
        std::fs::create_dir_all(top.join("a")).unwrap();
        let file = top.join("a/setup");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(
            owned_by_me(&top, &file, 0o002).is_ok(),
            "group write is a checkout's own business"
        );
        let err = owned_by_me(&top, &file, 0o022).unwrap_err().to_string();
        assert!(err.contains("writable by other users"), "{err}");
        std::fs::set_permissions(top.join("a"), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = owned_by_me(&top, &file, 0o002).unwrap_err().to_string();
        assert!(err.contains("a is writable"), "a directory on the way counts: {err}");
    }

    /// A link anywhere below the top is refused, not followed: its target's parents are nobody's
    /// to vouch for here.
    #[test]
    fn a_setup_reached_through_a_link_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("agents");
        let dir = integration(&root, "pi", &manifest("pi", 1), "true");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::write(&elsewhere, "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_file(dir.join(SETUP)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join(SETUP)).unwrap();
        let err = open(&root, "pi").unwrap_err().to_string();
        assert!(err.contains("setup is a symlink"), "{err}");

        // A linked directory on the way is refused the same.
        let real = tmp.path().join("real-clyde");
        integration(tmp.path(), "real-clyde", &manifest("clyde", 1), "true");
        std::os::unix::fs::symlink(&real, root.join("clyde")).unwrap();
        let err = open(&root, "clyde").unwrap_err().to_string();
        assert!(err.contains("clyde is a symlink"), "{err}");

        // The top itself may be reached through a link: that is the user's own layout.
        let linked_root = tmp.path().join("agents-link");
        std::os::unix::fs::symlink(&root, &linked_root).unwrap();
        integration(&root, "hermes", &manifest("hermes", 1), "true");
        open(&linked_root, "hermes").unwrap();
    }

    /// The registry's manifest says what `setup` last installed: a refresh whose `setup add` never
    /// ran puts the previous one back.
    #[test]
    fn a_manifest_is_restored_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("agents");
        assert!(installed_manifest(&root, "pi").is_none(), "nothing installed");
        restore_manifest(&root, "pi", b"{}").unwrap();
        assert!(!root.join("pi").exists(), "nothing to restore into");

        integration(&root, "pi", &manifest("pi", 1), "true");
        let before = installed_manifest(&root, "pi").unwrap();
        std::fs::write(root.join("pi/manifest.json"), manifest("pi", 2)).unwrap();
        restore_manifest(&root, "pi", &before).unwrap();
        assert_eq!(installed_manifest(&root, "pi").unwrap(), before);
    }

    /// A directory in the registry that is not an id is `open`'s to refuse, and never a note's
    /// to name.
    #[test]
    fn a_mismatch_is_reported_for_ids_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("agents");
        integration(&root, "pi (old)", &manifest("pi", 99), "true");
        integration(&root, "clyde", &manifest("clyde", 99), "true");
        assert_eq!(mismatched(&root), vec![("clyde".to_string(), 99)]);
    }

    #[test]
    fn add_hands_setup_the_memory_and_the_contract_environment() {
        let root = tempfile::tempdir().unwrap();
        let dir = integration(
            root.path(),
            "pi",
            &manifest("pi", CONTRACT_VERSION),
            r#"printf '%s\n' "$1" "$2" "$FUNES_AGENT_ID" "$FUNES_HOME" "$FUNES_BIN" > "$(dirname "$0")/ran""#,
        );

        open(root.path(), "pi").unwrap().add(Some("acme/kb")).unwrap();

        let ran = std::fs::read_to_string(dir.join("ran")).unwrap();
        let lines: Vec<&str> = ran.lines().collect();
        assert_eq!(&lines[..3], &["add", "acme/kb", "pi"]);
        assert_eq!(lines[3], dataset::funes_dir().to_string_lossy());
        assert_eq!(lines[4], "funes", "the command an integration records");
    }

    #[test]
    fn a_local_add_and_a_remove_pass_the_verb_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = integration(
            root.path(),
            "pi",
            &manifest("pi", CONTRACT_VERSION),
            r#"printf '%s|%s\n' "$1" "$2" >> "$(dirname "$0")/ran""#,
        );

        let b = open(root.path(), "pi").unwrap();
        b.add(None).unwrap();
        b.remove().unwrap();

        assert_eq!(std::fs::read_to_string(dir.join("ran")).unwrap(), "add|\nremove|\n");
    }

    #[test]
    fn a_contract_mismatch_is_refused_before_setup_runs() {
        let root = tempfile::tempdir().unwrap();
        let dir = integration(
            root.path(),
            "pi",
            &manifest("pi", CONTRACT_VERSION + 1),
            r#"touch "$(dirname "$0")/ran""#,
        );

        let err = open(root.path(), "pi").unwrap_err().to_string();
        assert!(err.contains(&(CONTRACT_VERSION + 1).to_string()), "{err}");
        assert!(err.contains(&CONTRACT_VERSION.to_string()), "{err}");
        assert!(!dir.join("ran").exists(), "setup must not run");
    }

    /// The id is a path segment funes writes under the registry, so a bad one is refused before a
    /// source is even looked for.
    #[tokio::test]
    async fn an_id_that_is_not_one_is_refused_before_anything_is_written() {
        let root = tempfile::tempdir().unwrap();
        for id in ["../docs", "Pi", "a/b", ""] {
            let err = provision(root.path(), id, false).await.unwrap_err().to_string();
            assert!(err.contains("not an integration id"), "{id:?}: {err}");
        }
        assert!(
            std::fs::read_dir(root.path()).unwrap().next().is_none(),
            "nothing was written"
        );
    }

    #[test]
    fn the_mismatched_integrations_are_listed_with_their_contract() {
        let root = tempfile::tempdir().unwrap();
        integration(root.path(), "pi", &manifest("pi", CONTRACT_VERSION), "true");
        integration(root.path(), "codex", &manifest("codex", CONTRACT_VERSION + 1), "true");
        integration(root.path(), "hermes", "not json", "true");
        assert_eq!(
            mismatched(root.path()),
            vec![("codex".to_string(), CONTRACT_VERSION + 1)]
        );
    }

    #[test]
    fn an_unknown_id_names_what_is_registered() {
        let root = tempfile::tempdir().unwrap();
        integration(root.path(), "pi", &manifest("pi", CONTRACT_VERSION), "true");
        integration(root.path(), "codex", &manifest("codex", CONTRACT_VERSION), "true");
        assert_eq!(registered_ids(root.path()), vec!["codex", "pi"]);

        let err = open(root.path(), "clyde").unwrap_err().to_string();
        assert!(err.contains("clyde") && err.contains("codex, pi"), "{err}");

        let empty = tempfile::tempdir().unwrap();
        assert!(registered_ids(empty.path()).is_empty());
        let err = open(empty.path(), "pi").unwrap_err().to_string();
        assert!(err.contains("no agents are registered"), "{err}");
    }

    #[test]
    fn the_id_must_be_the_directory_name_and_facet_safe() {
        let root = tempfile::tempdir().unwrap();
        integration(root.path(), "pi", &manifest("codex", CONTRACT_VERSION), "true");
        let err = open(root.path(), "pi").unwrap_err().to_string();
        assert!(err.contains("directory name"), "{err}");

        integration(root.path(), "Pi", &manifest("Pi", CONTRACT_VERSION), "true");
        let err = open(root.path(), "Pi").unwrap_err().to_string();
        assert!(err.contains("[a-z0-9_-]"), "{err}");
    }

    #[test]
    fn a_manifest_declares_its_release_and_publisher() {
        let root = tempfile::tempdir().unwrap();
        integration(
            root.path(),
            "pi",
            r#"{"contract_version": 1, "id": "pi", "label": "pi", "repo": "acme/funes-pi", "version": "1.2.3"}"#,
            "true",
        );
        let declared = open(root.path(), "pi").unwrap().manifest;
        assert_eq!(declared.version.as_deref(), Some("1.2.3"));
        assert_eq!(declared.publisher(), "acme");

        // Left out, there is no release to speak of.
        integration(root.path(), "pi", &manifest("pi", CONTRACT_VERSION), "true");
        assert_eq!(open(root.path(), "pi").unwrap().manifest.version, None);

        for version in ["1.2", "v1.2.3", "1.2.3-beta", "1..3", ""] {
            integration(
                root.path(),
                "pi",
                &format!(
                    r#"{{"contract_version": 1, "id": "pi", "label": "pi", "repo": "acme/funes-pi", "version": "{version}"}}"#
                ),
                "true",
            );
            let err = open(root.path(), "pi").unwrap_err().to_string();
            assert!(err.contains("MAJOR.MINOR.PATCH"), "{version:?}: {err}");
        }
        for repo in ["acme", "acme/", "/pi", "acme/funes/pi", ""] {
            integration(
                root.path(),
                "pi",
                &format!(r#"{{"contract_version": 1, "id": "pi", "label": "pi", "repo": "{repo}"}}"#),
                "true",
            );
            let err = open(root.path(), "pi").unwrap_err().to_string();
            assert!(err.contains("<publisher>/<name>"), "{repo:?}: {err}");
        }
    }

    #[test]
    fn a_manifest_is_rejected_rather_than_guessed() {
        let root = tempfile::tempdir().unwrap();
        integration(
            root.path(),
            "pi",
            r#"{"contract_version": 1, "id": "pi", "label": "Pi"}"#,
            "true",
        );
        let err = format!("{:#}", open(root.path(), "pi").unwrap_err());
        assert!(err.contains("repo"), "{err}");

        integration(
            root.path(),
            "pi",
            r#"{"contract_version": 1, "id": "pi", "label": "Pi", "repo": "acme/b", "hook": "x"}"#,
            "true",
        );
        let err = format!("{:#}", open(root.path(), "pi").unwrap_err());
        assert!(err.contains("hook"), "{err}");
    }

    #[test]
    fn setup_must_exist_and_be_executable() {
        let root = tempfile::tempdir().unwrap();
        let dir = integration(root.path(), "pi", &manifest("pi", CONTRACT_VERSION), "true");

        let setup = dir.join(SETUP);
        std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = open(root.path(), "pi").unwrap_err().to_string();
        assert!(err.contains("not executable"), "{err}");

        std::fs::remove_file(&setup).unwrap();
        let err = format!("{:#}", open(root.path(), "pi").unwrap_err());
        assert!(err.contains(SETUP), "{err}");
    }

    #[test]
    fn a_failing_setup_fails_the_command() {
        let root = tempfile::tempdir().unwrap();
        integration(root.path(), "pi", &manifest("pi", CONTRACT_VERSION), "exit 3");
        let err = open(root.path(), "pi").unwrap().add(None).unwrap_err().to_string();
        assert!(err.contains('3'), "{err}");
    }
}
