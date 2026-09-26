//! The registry of agent integrations: where one lives, what it declares, and how funes runs it.
//!
//! An integration is a directory `<root>/<id>/` holding a `manifest.json` and an executable
//! `setup`, run as `setup add [MEMORY]` or `setup remove` with `$FUNES_BIN`, `$FUNES_HOME` and
//! `$FUNES_AGENT_ID` in its environment.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use hf_hub::buckets::{BucketDownload, BucketTreeEntry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::hub;
use crate::memory::dataset;
use crate::traces::spool;

/// The integration contract this funes speaks.
pub const CONTRACT_VERSION: u32 = 1;

/// The executable every integration provides: `setup add [MEMORY]`, `setup remove`.
const SETUP: &str = "setup";

/// The most an integration's archive may weigh: the four maintained ones are a few hundred
/// kilobytes, and an archive is unpacked before anything about it is checked but its listing.
const MAX_ARCHIVE_BYTES: u64 = 16 << 20;

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

/// The owner in a `<publisher>/<name>` repo.
pub fn publisher(repo: &str) -> &str {
    repo.split('/').next().unwrap_or_default()
}

/// `<publisher>/<name>`, both non-empty.
fn is_repo(s: &str) -> bool {
    matches!(s.split_once('/'), Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/'))
}

/// `MAJOR.MINOR.PATCH`, digits only.
fn is_release_version(s: &str) -> bool {
    release_key(s).is_some()
}

/// A release version as something to order by, or `None` for what is not one.
fn release_key(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split('.').map(|p| {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<u64>().ok())
            .flatten()
    });
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(Some(a)), Some(Some(b)), Some(Some(c)), None) => Some((a, b, c)),
        _ => None,
    }
}

/// The catalog format this funes reads.
pub const CATALOG_VERSION: u32 = 1;

/// The catalog of maintained integrations: each id's releases. Fields it does not know are
/// ignored, so the catalog may say more than this funes reads.
#[derive(Debug, Deserialize)]
pub struct Catalog {
    pub catalog_version: u32,
    pub integrations: BTreeMap<String, Listing>,
}

/// One maintained integration's releases.
#[derive(Debug, Deserialize)]
pub struct Listing {
    pub repo: String,
    pub releases: Vec<Release>,
}

/// One release: where its archive is, and what it must digest to.
#[derive(Debug, Deserialize, PartialEq)]
pub struct Release {
    pub version: String,
    pub contract_version: u32,
    pub url: String,
    pub sha256: String,
}

impl Catalog {
    pub fn parse(text: &str) -> Result<Catalog> {
        let catalog: Catalog = serde_json::from_str(text).context("parsing the integrations catalog")?;
        if catalog.catalog_version != CATALOG_VERSION {
            bail!(
                "the integrations catalog is version {}, and this funes reads {CATALOG_VERSION} — run `funes update`",
                catalog.catalog_version
            );
        }
        Ok(catalog)
    }

    /// The newest release of `id` for this funes's contract; `None` when `id` is not listed.
    pub fn resolve(&self, id: &str) -> Result<Option<&Release>> {
        let Some(listing) = self.integrations.get(id) else {
            return Ok(None);
        };
        let mut compatible: Vec<(&Release, (u64, u64, u64))> = Vec::new();
        for release in &listing.releases {
            let key = release_key(&release.version).ok_or_else(|| {
                anyhow::anyhow!(
                    "the catalog lists {id} {:?}, which is not a release version",
                    release.version
                )
            })?;
            if release.contract_version == CONTRACT_VERSION {
                compatible.push((release, key));
            }
        }
        let Some(newest) = compatible.iter().max_by_key(|(_, key)| *key) else {
            let contracts: BTreeSet<u32> = listing.releases.iter().map(|r| r.contract_version).collect();
            let contracts: Vec<String> = contracts.iter().map(u32::to_string).collect();
            bail!(
                "the catalog lists {id} for contract {} only, and this funes speaks {CONTRACT_VERSION} — \
                 run `funes update`, or name a release built for it with --from",
                contracts.join(", ")
            );
        };
        Ok(Some(newest.0))
    }
}

/// A resolved integration: its directory and what it declares.
#[derive(Debug)]
pub struct Integration {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// Where an integration's files came from, as funes resolved them.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// A directory the user pointed funes at.
    Directory { path: PathBuf },
    /// A published archive, verified against the checksum beside it.
    Archive { url: String, sha256: String },
    /// The release the catalog named, verified against the digest it gave.
    Catalog { url: String, sha256: String },
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::Directory { path } => write!(f, "{}", path.display()),
            Origin::Archive { url, .. } | Origin::Catalog { url, .. } => f.write_str(url),
        }
    }
}

/// The package's files, by path relative to its directory, each with its sha256 as hex. What
/// `setup` keeps beside them is not in it.
pub type Files = BTreeMap<String, String>;

/// What funes installed at `<root>/<id>`, kept beside the directory as `<root>/<id>.json`: the
/// package as its manifest declared it, where the files came from, and which files they are.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    pub contract_version: u32,
    pub id: String,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub origin: Origin,
    #[serde(default)]
    pub files: Files,
    pub installed_at: String,
}

impl Installed {
    /// The record of `manifest`'s `files`, installed from `origin` now.
    pub fn new(manifest: &Manifest, origin: Origin, files: Files) -> Installed {
        Installed {
            contract_version: manifest.contract_version,
            id: manifest.id.clone(),
            repo: manifest.repo.clone(),
            version: manifest.version.clone(),
            origin,
            files,
            installed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }
}

/// How `root/<id>`'s files compare with the record of their installation.
#[derive(Debug, PartialEq)]
pub enum Verification {
    /// Every file the record names is as funes installed it.
    Intact,
    /// These files differ from the record, or are gone.
    Changed(Vec<String>),
    /// No record: funes has no account of installing what is there.
    Unrecorded,
}

/// Compare `root/<id>`'s files with what funes recorded installing there. The record is judged
/// as `setup` is — the user's own, writable by nobody else — since what it vouches for runs.
pub fn verify_installed(root: &Path, id: &str) -> Result<Verification> {
    let Some(record) = installed(root, id) else {
        return Ok(Verification::Unrecorded);
    };
    owned_by_me(root, &record_path(root, id), 0o022)?;
    let dir = root.join(id);
    let changed: Vec<String> = record
        .files
        .iter()
        .filter(|(path, digest)| {
            std::fs::read(dir.join(path))
                .map(|bytes| hex::encode(Sha256::digest(&bytes)) != **digest)
                .unwrap_or(true)
        })
        .map(|(path, _)| path.clone())
        .collect();
    Ok(if changed.is_empty() {
        Verification::Intact
    } else {
        Verification::Changed(changed)
    })
}

fn record_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.json"))
}

/// The record of what is installed at `root/<id>`, when funes wrote one.
pub fn installed(root: &Path, id: &str) -> Option<Installed> {
    let text = std::fs::read_to_string(record_path(root, id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write `record` as `root/<id>.json`, closed to other writers whatever the umask.
pub fn record(root: &Path, record: &Installed) -> Result<()> {
    let path = record_path(root, &record.id);
    let mut text = serde_json::to_string_pretty(record).context("serializing the install record")?;
    text.push('\n');
    let tmp = root.join(format!(".{}.json.funes-tmp{}", record.id, std::process::id()));
    let written = std::fs::write(&tmp, text)
        .with_context(|| format!("writing {}", tmp.display()))
        .and_then(|()| {
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
                .with_context(|| format!("setting the mode of {}", tmp.display()))
        })
        .and_then(|()| std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display())));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
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
    require_executable(&setup)?;
    owned_by_me(root, &setup, 0o022)?;

    Ok(Integration { dir, manifest })
}

/// Refuse a `setup` that is not there to run, or could not.
fn require_executable(setup: &Path) -> Result<()> {
    let meta = std::fs::metadata(setup).with_context(|| format!("no {SETUP} at {}", setup.display()))?;
    if meta.permissions().mode() & 0o111 == 0 {
        bail!("{} is not executable", setup.display());
    }
    Ok(())
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
/// finds there. `others` is the mode bits that give it away: group and world write for the
/// registry. Below `top`, a link is refused rather than followed: what it points at has parents of
/// its own that this walk would never see.
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

/// Whether the copy installed at `root/<id>` speaks a contract this funes does not: one it cannot
/// run, however it came to be there.
pub fn speaks_another_contract(root: &Path, id: &str) -> bool {
    installed_manifest(root, id)
        .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
        .is_some_and(|manifest| manifest.contract_version != CONTRACT_VERSION)
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
    /// A directory `$FUNES_INTEGRATIONS` points at.
    Redirected(PathBuf),
    /// A directory named on the command line.
    Named(PathBuf),
    /// An archive named on the command line, `hf://buckets/…/<id>.tar.gz`.
    Archive(String),
    /// The release the catalog of maintained integrations names.
    Catalog,
}

/// No files for an id anywhere funes looks: `$FUNES_INTEGRATIONS` holds none, or the catalog lists
/// none. A source funes could not reach is any other error.
#[derive(Debug)]
pub struct Absent(String);

impl std::fmt::Display for Absent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Absent {}

/// Another publisher's files where an integration is installed.
#[derive(Debug)]
pub struct Takeover(String);

impl std::fmt::Display for Takeover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Takeover {}

/// Whether funes vouches for the files it installed: a published archive it verified, or the
/// checkout it was built from. Anything else is someone's files on this disk, and the caller
/// confirms before funes executes them.
#[derive(Debug)]
pub enum Provenance {
    Vouched,
    /// Where they came from, for the confirmation.
    Unvouched(String),
}

/// What a provision resolved: whether funes vouches for the files, where they came from, and
/// which files they are.
#[derive(Debug)]
pub struct Provisioned {
    pub provenance: Provenance,
    pub origin: Origin,
    pub files: Files,
}

/// Resolve `id`'s files: what `from` names, else `$FUNES_INTEGRATIONS` if set — authoritative, so
/// a test or a fork cannot reach the network by accident — else the catalog. A directory is made
/// absolute here: the record names it, and an update follows it, from wherever funes runs then.
fn source_for(id: &str, from: Option<&str>) -> Result<Source> {
    if let Some(from) = from {
        if from.starts_with("hf://") {
            return Ok(Source::Archive(from.to_string()));
        }
        let dir = std::path::absolute(from).with_context(|| format!("resolving {from}"))?;
        if !dir.is_dir() {
            bail!("{from} is not a directory, nor an hf://buckets/… archive URL");
        }
        return Ok(Source::Named(dir));
    }
    if let Some(dir) = std::env::var_os("FUNES_INTEGRATIONS") {
        let dir = std::path::absolute(PathBuf::from(dir).join(id)).context("resolving $FUNES_INTEGRATIONS")?;
        if !dir.is_dir() {
            return Err(Absent(format!("$FUNES_INTEGRATIONS holds no {id}")).into());
        }
        return Ok(Source::Redirected(dir));
    }
    Ok(Source::Catalog)
}

/// Install `id`'s files into the registry, from what `from` names when it names one. Only a file
/// that differs is rewritten, and nothing is pruned — an integration's `setup` keeps its own state
/// beside them. The id names the directory written, so it is checked here, before anything is.
pub async fn provision(root: &Path, id: &str, from: Option<&str>) -> Result<Provisioned> {
    if !spool::is_id(id) {
        bail!("{id:?} is not an integration id (lowercase [a-z0-9_-])");
    }
    match source_for(id, from)? {
        Source::Redirected(src) => {
            let files = install_from(root, id, &src)?;
            Ok(Provisioned {
                provenance: Provenance::Unvouched(format!("$FUNES_INTEGRATIONS ({})", src.display())),
                origin: Origin::Directory { path: src },
                files,
            })
        }
        Source::Named(src) => {
            let files = install_from(root, id, &src)?;
            Ok(Provisioned {
                provenance: Provenance::Unvouched(src.display().to_string()),
                origin: Origin::Directory { path: src },
                files,
            })
        }
        Source::Archive(url) => {
            let staging = tempfile::tempdir().context("creating a staging directory")?;
            let (archive, sha256) = fetch_archive(&url, staging.path()).await?;
            validate_archive(&archive)?;
            let unpacked = staging.path().join("unpacked");
            unpack(&archive, &unpacked)?;
            let files = install_from(root, id, &unpacked)?;
            Ok(Provisioned {
                provenance: Provenance::Unvouched(url.clone()),
                origin: Origin::Archive { url, sha256 },
                files,
            })
        }
        Source::Catalog => {
            let staging = tempfile::tempdir().context("creating a staging directory")?;
            let catalog = fetch_catalog(staging.path()).await?;
            let release = catalog.resolve(id)?.ok_or_else(|| {
                Absent(format!(
                    "the integrations catalog lists no {id} — name where it comes from with --from"
                ))
            })?;
            eprintln!("fetching the {id} integration {}…", release.version);
            let (archive, sha256) = fetch_archive(&release.url, staging.path()).await?;
            if sha256 != release.sha256 {
                bail!(
                    "{} is not the release the catalog names: it digests to {sha256}, the catalog says {}",
                    release.url,
                    release.sha256
                );
            }
            validate_archive(&archive)?;
            let unpacked = staging.path().join("unpacked");
            unpack(&archive, &unpacked)?;
            let declared = read_manifest(&unpacked.join("manifest.json"), id)?;
            if declared.version.as_deref() != Some(release.version.as_str()) {
                bail!(
                    "{} declares version {}, and the catalog lists it as {}",
                    release.url,
                    declared.version.as_deref().unwrap_or("none"),
                    release.version
                );
            }
            let files = install_from(root, id, &unpacked)?;
            Ok(Provisioned {
                provenance: Provenance::Vouched,
                origin: Origin::Catalog {
                    url: release.url.clone(),
                    sha256,
                },
                files,
            })
        }
    }
}

/// Copy `src` over `root/<id>` once what it declares checks out: a manifest that would be refused
/// installed is refused here, before a byte moves, and so is another publisher's, and so is a
/// package without its `setup` — the copy prunes nothing, so the installed one would run outside
/// the record. The files copied, with their digests.
fn install_from(root: &Path, id: &str, src: &Path) -> Result<Files> {
    let incoming = read_manifest(&src.join("manifest.json"), id)?;
    require_executable(&src.join(SETUP))?;
    refuse_takeover(root, id, &incoming)?;
    copy_into(src, &root.join(id))
}

/// Refuse another publisher's files where `id`'s are installed. What funes recorded at install
/// names the publisher, else the installed manifest does; with neither there is nothing to keep.
fn refuse_takeover(root: &Path, id: &str, incoming: &Manifest) -> Result<()> {
    let (owner, from) = match installed(root, id) {
        Some(record) => (publisher(&record.repo).to_string(), format!(", from {}", record.origin)),
        None => match installed_manifest(root, id).and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok()) {
            Some(manifest) => (publisher(&manifest.repo).to_string(), String::new()),
            None => return Ok(()),
        },
    };
    if owner != publisher(&incoming.repo) {
        return Err(Takeover(format!(
            "the {id} integration installed here is {owner}'s{from}; this one is {}'s ({}) — \
             `funes remove {id}` first to replace it",
            publisher(&incoming.repo),
            incoming.repo
        ))
        .into());
    }
    Ok(())
}

/// Download the catalog of maintained integrations into `dir` and read it.
async fn fetch_catalog(dir: &Path) -> Result<Catalog> {
    let url = hub::catalog_url();
    let (owner, name, path) = hub::parse_bucket_url(&url)?;
    let local = dir.join("catalog.json");
    hub::bucket(&owner, &name, true)?
        .download_files()
        .files(vec![BucketDownload::new(path, &local)])
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    let text = std::fs::read_to_string(&local).with_context(|| format!("reading {}", local.display()))?;
    Catalog::parse(&text)
}

/// Download the archive an `hf://buckets/…/<name>.tar.gz` URL names into `dir`, with the
/// `SHA256SUMS` beside it, and check the one against the other; the archive, and its digest as
/// hex. Its size is checked before a byte of it is fetched.
async fn fetch_archive(url: &str, dir: &Path) -> Result<(PathBuf, String)> {
    let (owner, name, path) = hub::parse_bucket_url(url)?;
    let (prefix, asset) = match path.rsplit_once('/') {
        Some((prefix, asset)) if asset.ends_with(".tar.gz") => (prefix.to_string(), asset.to_string()),
        _ => bail!("{url} does not name a <prefix>/<name>.tar.gz archive"),
    };
    let bucket = hub::bucket(&owner, &name, true)?;
    // Listed, not resolved: the resolve endpoint answers with a redirect the metadata call will
    // not follow.
    let size = bucket
        .get_paths_info()
        .paths(vec![path.clone()])
        .send()
        .await
        .with_context(|| format!("looking up {url}"))?
        .into_iter()
        .find_map(|entry| match entry {
            BucketTreeEntry::File { path: listed, size, .. } if listed == path => Some(size),
            _ => None,
        })
        .ok_or_else(|| anyhow!("the bucket lists no {url}"))?;
    if size > MAX_ARCHIVE_BYTES {
        bail!("{url} is {size} bytes, more than the {MAX_ARCHIVE_BYTES} an integration may weigh");
    }
    let archive = dir.join(&asset);
    let manifest = dir.join("SHA256SUMS");
    bucket
        .download_files()
        .files(vec![
            BucketDownload::new(path, &archive),
            BucketDownload::new(format!("{prefix}/SHA256SUMS"), &manifest),
        ])
        .send()
        .await
        .with_context(|| format!("downloading {url} and the SHA256SUMS beside it"))?;
    let digest = hub::verify_checksum(&archive, &manifest, &asset)?;
    Ok((archive, hex::encode(digest)))
}

/// Refuse an archive whose listing names anything an integration may not carry — a path outside
/// its root, or a link — before a byte of it is extracted.
fn validate_archive(archive: &Path) -> Result<()> {
    let names = tar_list(archive, "-tzf")?;
    let entries = tar_list(archive, "-tvzf")?;
    if names.len() != entries.len() {
        bail!("{} lists differently twice", archive.display());
    }
    for (name, entry) in names.iter().zip(&entries) {
        archive_entry_allowed(name, entry).with_context(|| format!("refusing {}", archive.display()))?;
    }
    Ok(())
}

/// One archive entry: its `name` as `tar -t` lists it, and its `-tv` line, whose first character
/// is its type.
fn archive_entry_allowed(name: &str, entry: &str) -> Result<()> {
    let path = Path::new(name);
    if path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("{name} lies outside the archive's root");
    }
    if matches!(entry.chars().next(), Some('l') | Some('h')) {
        bail!("{name} is a link, which an integration may not carry");
    }
    Ok(())
}

/// The lines `tar <flag> archive` prints.
fn tar_list(archive: &Path, flag: &str) -> Result<Vec<String>> {
    let output = Command::new("tar")
        .arg(flag)
        .arg(archive)
        .output()
        .context("running tar to list the integration")?;
    if !output.status.success() {
        bail!(
            "listing {} failed (exit {:?}): {}",
            archive.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
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

/// Delete an integration's directory, including the state its `setup` wrote there, and funes's
/// record of it.
pub fn discard(root: &Path, id: &str) -> Result<()> {
    super::remove_tree(&root.join(id))?;
    super::remove_tree(&record_path(root, id))
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

/// Copy `src`'s tree over `dst`; the files copied, by path under `dst`, with their digests.
fn copy_into(src: &Path, dst: &Path) -> Result<Files> {
    check_source(src)?;
    let mut files = Files::new();
    copy_tree(src, dst, "", &mut files)?;
    Ok(files)
}

fn copy_tree(src: &Path, dst: &Path, under: &str, files: &mut Files) -> Result<()> {
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
        let path = format!("{under}{}", name.to_string_lossy());
        if meta.is_dir() {
            copy_tree(&from, &to, &format!("{path}/"), files)?;
            continue;
        }
        let bytes = std::fs::read(&from).with_context(|| format!("reading {}", from.display()))?;
        files.insert(path, hex::encode(Sha256::digest(&bytes)));
        // The source's mode, closed to others: `open` refuses a file others could write, and a
        // private group's umask leaves a source directory's files group-writable.
        let mode = std::fs::Permissions::from_mode(meta.permissions().mode() & 0o777 & !0o022);
        // Written beside and renamed over: a link left at the destination is replaced, never
        // followed to wherever it points, and a reader never sees a half-written file.
        let stale_link = to.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
        if stale_link || std::fs::read(&to).map(|old| old != bytes).unwrap_or(true) {
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
        copy_into(&unpacked, &dst).unwrap();

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
    fn copying_a_directory_follows_links_keeps_modes_and_keeps_local_state() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared.sh");
        std::fs::write(&shared, "shared v1").unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();

        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("scripts")).unwrap();
        std::fs::write(src.join("manifest.json"), "{}").unwrap();
        std::os::unix::fs::symlink(&shared, src.join("scripts/shared.sh")).unwrap();

        let dst = tmp.path().join("dst");
        copy_into(&src, &dst).unwrap();

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
        copy_into(&src, &dst).unwrap();

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
        copy_into(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "not the bundle's");
        let manifest = dst.join("manifest.json");
        assert!(!manifest.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&manifest).unwrap(), "{}");

        // A link where a directory goes is refused outright, and nothing lands where it points.
        std::fs::remove_dir_all(dst.join("scripts")).unwrap();
        let elsewhere_dir = tmp.path().join("elsewhere-dir");
        std::fs::create_dir_all(&elsewhere_dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere_dir, dst.join("scripts")).unwrap();
        let err = copy_into(&src, &dst).unwrap_err().to_string();
        assert!(err.contains("is a symlink"), "{err}");
        assert!(
            std::fs::read_dir(&elsewhere_dir).unwrap().next().is_none(),
            "nothing written through"
        );
        std::fs::remove_file(dst.join("scripts")).unwrap();

        // A link to a directory in the source is refused before anything is copied.
        std::fs::write(&shared, "shared v3").unwrap();
        std::os::unix::fs::symlink(tmp.path(), src.join("everything")).unwrap();
        let err = copy_into(&src, &dst).unwrap_err().to_string();
        assert!(err.contains("symlink to a directory"), "{err}");
        assert!(!dst.join("scripts/shared.sh").exists(), "refused before a byte moved");
    }

    /// A private group's umask leaves a source directory's files group-writable; the registry's
    /// copy is closed to others, or `open` would refuse what funes itself just wrote.
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
        copy_into(&src, &dst).unwrap();
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
            "group write passes when world write alone is asked about"
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

    /// The record round-trips, is not an integration, and goes with the directory.
    #[test]
    fn what_was_installed_is_recorded_beside_the_directory_and_discarded_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("agents");
        integration(&root, "pi", &manifest("pi", CONTRACT_VERSION), "true");
        assert!(installed(&root, "pi").is_none());

        let declared = open(&root, "pi").unwrap().manifest;
        let rec = Installed::new(
            &declared,
            Origin::Archive {
                url: "hf://buckets/huggingface/funes-integrations/pi/1.0.0/pi.tar.gz".to_string(),
                sha256: "ab".repeat(32),
            },
            Files::new(),
        );
        record(&root, &rec).unwrap();
        assert_eq!(installed(&root, "pi").unwrap(), rec);
        assert_eq!(registered_ids(&root), vec!["pi"]);
        let text = std::fs::read_to_string(root.join("pi.json")).unwrap();
        assert!(
            text.contains(r#""kind": "archive""#) && !text.contains(r#""version""#),
            "{text}"
        );

        discard(&root, "pi").unwrap();
        assert!(!root.join("pi").exists() && !root.join("pi.json").exists());
    }

    /// The copy names the package's files with their digests, and the record is verified against
    /// them: what `setup` keeps beside them is not the package's, an edit to one of them is.
    #[test]
    fn the_record_names_the_files_and_verification_tells_them_apart_from_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("agents");
        let src = integration(&tmp.path().join("src"), "pi", &manifest("pi", CONTRACT_VERSION), "true");
        std::fs::create_dir_all(src.join("scripts")).unwrap();
        std::fs::write(src.join("scripts/hook.sh"), "hook").unwrap();

        assert_eq!(verify_installed(&root, "pi").unwrap(), Verification::Unrecorded);
        create_owned(&root).unwrap();
        let files = copy_into(&src, &root.join("pi")).unwrap();
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            vec!["manifest.json", "scripts/hook.sh", "setup"]
        );
        assert_eq!(files["scripts/hook.sh"], hex::encode(Sha256::digest(b"hook")));

        let rec = Installed::new(
            &open(&root, "pi").unwrap().manifest,
            Origin::Directory { path: src.clone() },
            files,
        );
        record(&root, &rec).unwrap();
        assert_eq!(installed(&root, "pi").unwrap().files, rec.files);
        assert_eq!(verify_installed(&root, "pi").unwrap(), Verification::Intact);

        // State beside the package is the integration's own business.
        std::fs::write(root.join("pi/memory"), "acme/kb\n").unwrap();
        assert_eq!(verify_installed(&root, "pi").unwrap(), Verification::Intact);

        // A changed or missing package file is named.
        std::fs::write(root.join("pi/scripts/hook.sh"), "hooked").unwrap();
        std::fs::remove_file(root.join("pi/setup")).unwrap();
        assert_eq!(
            verify_installed(&root, "pi").unwrap(),
            Verification::Changed(vec!["scripts/hook.sh".to_string(), "setup".to_string()])
        );

        // A record others could have written vouches for nothing.
        std::fs::set_permissions(root.join("pi.json"), std::fs::Permissions::from_mode(0o666)).unwrap();
        let err = verify_installed(&root, "pi").unwrap_err().to_string();
        assert!(err.contains("writable by other users"), "{err}");
    }

    /// A package without its executable would leave the installed one running outside the
    /// record: refused before the copy, like one whose executable cannot run.
    #[test]
    fn a_package_without_its_executable_is_refused_before_the_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("agents");
        integration(&root, "pi", &manifest("pi", CONTRACT_VERSION), "true");
        let installed_setup = || std::fs::read_to_string(root.join("pi").join(SETUP)).unwrap();

        let bare = integration(
            &tmp.path().join("bare"),
            "pi",
            &manifest("pi", CONTRACT_VERSION),
            "echo bare",
        );
        std::fs::remove_file(bare.join(SETUP)).unwrap();
        let err = format!("{:#}", install_from(&root, "pi", &bare).unwrap_err());
        assert!(err.contains(&format!("no {SETUP} at")), "{err}");
        assert_eq!(installed_setup(), "#!/bin/sh\ntrue\n", "nothing copied");

        let inert = integration(
            &tmp.path().join("inert"),
            "pi",
            &manifest("pi", CONTRACT_VERSION),
            "echo inert",
        );
        std::fs::set_permissions(inert.join(SETUP), std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = install_from(&root, "pi", &inert).unwrap_err().to_string();
        assert!(err.contains("not executable"), "{err}");
        assert_eq!(installed_setup(), "#!/bin/sh\ntrue\n", "nothing copied");
    }

    /// Another publisher's files are refused before a byte moves, whether funes recorded where
    /// the install came from or only its manifest says whose it is.
    #[test]
    fn another_publishers_files_are_refused_where_an_integration_is_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("agents");
        integration(&root, "pi", &manifest("pi", CONTRACT_VERSION), "true");
        let setup = || std::fs::read_to_string(root.join("pi").join(SETUP)).unwrap();

        let theirs = integration(
            &tmp.path().join("theirs"),
            "pi",
            r#"{"contract_version": 1, "id": "pi", "label": "pi", "repo": "other/pi"}"#,
            "echo theirs",
        );
        let err = install_from(&root, "pi", &theirs).unwrap_err();
        assert!(err.downcast_ref::<Takeover>().is_some(), "{err}");
        assert!(err.to_string().contains("`funes remove pi` first"), "{err}");
        assert_eq!(setup(), "#!/bin/sh\ntrue\n", "nothing copied");

        // With a record, the refusal says where the install came from.
        let rec = Installed::new(
            &open(&root, "pi").unwrap().manifest,
            Origin::Catalog {
                url: "hf://buckets/huggingface/funes-integrations/pi/1.0.0/pi.tar.gz".to_string(),
                sha256: "ab".repeat(32),
            },
            Files::new(),
        );
        record(&root, &rec).unwrap();
        let err = install_from(&root, "pi", &theirs).unwrap_err().to_string();
        assert!(
            err.contains("acme's, from hf://buckets/huggingface/funes-integrations/pi/1.0.0/pi.tar.gz"),
            "{err}"
        );

        // The same publisher's files replace it.
        let mine = integration(
            &tmp.path().join("mine"),
            "pi",
            &manifest("pi", CONTRACT_VERSION),
            "echo mine",
        );
        install_from(&root, "pi", &mine).unwrap();
        assert!(setup().contains("echo mine"));

        // A manifest that would be refused installed is refused before the copy too.
        let bad = integration(
            &tmp.path().join("bad"),
            "pi",
            &manifest("pi", CONTRACT_VERSION + 1),
            "echo bad",
        );
        let err = install_from(&root, "pi", &bad).unwrap_err().to_string();
        assert!(err.contains("contract version"), "{err}");
        assert!(setup().contains("echo mine"), "untouched");
    }

    /// What an archive lists is judged before any of it is extracted.
    #[test]
    fn an_archive_is_refused_for_what_it_lists() {
        for (name, entry, refused) in [
            (
                "./manifest.json",
                "-rw-r--r--  0 me me 12 Sep 25 10:00 ./manifest.json",
                false,
            ),
            (
                "scripts/hook.sh",
                "-rwxr-xr-x  0 me me 12 Sep 25 10:00 scripts/hook.sh",
                false,
            ),
            ("../evil", "-rw-r--r--  0 me me 12 Sep 25 10:00 ../evil", true),
            (
                "./a/../../evil",
                "-rw-r--r--  0 me me 12 Sep 25 10:00 ./a/../../evil",
                true,
            ),
            ("/etc/passwd", "-rw-r--r--  0 me me 12 Sep 25 10:00 /etc/passwd", true),
            (
                "./setup",
                "lrwxr-xr-x  0 me me 12 Sep 25 10:00 ./setup -> /bin/sh",
                true,
            ),
            (
                "./twin",
                "hrw-r--r--  0 me me 12 Sep 25 10:00 ./twin link to ./setup",
                true,
            ),
        ] {
            assert_eq!(archive_entry_allowed(name, entry).is_err(), refused, "{name}");
        }

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("manifest.json"), "{}").unwrap();
        std::fs::write(src.join("setup"), "#!/bin/sh\n").unwrap();
        let pack = |archive: &Path, extra: &[&str]| {
            let status = Command::new("tar")
                .arg("-czf")
                .arg(archive)
                .args(extra)
                .arg("-C")
                .arg(&src)
                .arg(".")
                .status()
                .unwrap();
            assert!(status.success());
        };
        let clean = tmp.path().join("clean.tar.gz");
        pack(&clean, &[]);
        validate_archive(&clean).unwrap();

        std::os::unix::fs::symlink("/bin/sh", src.join("sh")).unwrap();
        let linked = tmp.path().join("linked.tar.gz");
        pack(&linked, &[]);
        let err = format!("{:#}", validate_archive(&linked).unwrap_err());
        assert!(err.contains("sh is a link"), "{err}");
        std::fs::remove_file(src.join("sh")).unwrap();

        // Packed with absolute names kept.
        let absolute = tmp.path().join("absolute.tar.gz");
        let status = Command::new("tar")
            .arg("-czPf")
            .arg(&absolute)
            .arg(src.join("setup"))
            .status()
            .unwrap();
        assert!(status.success());
        let err = format!("{:#}", validate_archive(&absolute).unwrap_err());
        assert!(err.contains("outside the archive's root"), "{err}");
    }

    /// The catalog names an alias's releases; funes takes the newest for its contract.
    #[test]
    fn the_catalog_resolves_an_alias_to_its_newest_release_for_this_contract() {
        let text = format!(
            r#"{{"catalog_version": 1, "integrations": {{
                "pi": {{"repo": "huggingface/funes-integrations", "releases": [
                    {{"version": "1.0.0", "contract_version": {c}, "url": "hf://buckets/huggingface/funes-integrations/pi/1.0.0/pi.tar.gz", "sha256": "aa"}},
                    {{"version": "1.10.0", "contract_version": {c}, "url": "hf://buckets/huggingface/funes-integrations/pi/1.10.0/pi.tar.gz", "sha256": "bb"}},
                    {{"version": "1.9.0", "contract_version": {c}, "url": "hf://buckets/huggingface/funes-integrations/pi/1.9.0/pi.tar.gz", "sha256": "cc"}},
                    {{"version": "2.0.0", "contract_version": {next}, "url": "hf://buckets/huggingface/funes-integrations/pi/2.0.0/pi.tar.gz", "sha256": "dd", "notes": "ignored"}}
                ]}},
                "codex": {{"repo": "huggingface/funes-integrations", "releases": [
                    {{"version": "3.0.0", "contract_version": {next}, "url": "hf://buckets/x/y/codex.tar.gz", "sha256": "ee"}}
                ]}}
            }}}}"#,
            c = CONTRACT_VERSION,
            next = CONTRACT_VERSION + 1
        );
        let catalog = Catalog::parse(&text).unwrap();
        let pi = catalog.resolve("pi").unwrap().unwrap();
        assert_eq!(pi.version, "1.10.0", "newest by number, not by text, for this contract");
        assert!(catalog.resolve("clyde").unwrap().is_none(), "unlisted is not an error");
        let err = catalog.resolve("codex").unwrap_err().to_string();
        assert!(
            err.contains(&format!("contract {} only", CONTRACT_VERSION + 1)) && err.contains("--from"),
            "{err}"
        );

        let err = Catalog::parse(r#"{"catalog_version": 2, "integrations": {}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("funes update"), "{err}");
    }

    #[test]
    fn a_copy_this_funes_cannot_run_is_told_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("agents");
        assert!(!speaks_another_contract(&root, "pi"), "nothing installed");
        integration(&root, "pi", &manifest("pi", CONTRACT_VERSION), "true");
        assert!(!speaks_another_contract(&root, "pi"));
        integration(&root, "pi", &manifest("pi", CONTRACT_VERSION + 1), "true");
        assert!(speaks_another_contract(&root, "pi"));
        integration(&root, "pi", "not json", "true");
        assert!(!speaks_another_contract(&root, "pi"), "unreadable is open's to refuse");
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
            let err = provision(root.path(), id, None).await.unwrap_err().to_string();
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
        assert_eq!(publisher(&declared.repo), "acme");

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
