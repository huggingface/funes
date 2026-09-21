//! The registry of agent integrations: where one lives, what it declares, and how funes runs it.
//!
//! An integration is a directory `<root>/<id>/` holding a `manifest.json` and an executable
//! `setup`. funes's knowledge of an agent is the lookup: resolve `<id>`, then exec
//! `setup add [MEMORY]` or `setup remove`. What the script can count on — the funes binary, the
//! home it writes to, and its own id — arrives in the environment.

use anyhow::{bail, Context, Result};
use hf_hub::buckets::BucketDownload;
use serde::Deserialize;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::hub;
use crate::memory::dataset;

/// The integration contract this funes speaks. One built for another version is refused before its
/// `setup` runs at all: the manifest fields and the argv it expects are that version's, not this
/// one's.
pub const CONTRACT_VERSION: u32 = 1;

/// The executable every integration provides: `setup add [MEMORY]`, `setup remove`.
const SETUP: &str = "setup";

/// What an integration declares about itself, in `manifest.json`. An unknown field is rejected
/// rather than ignored, so a manifest written against a later contract fails here instead of
/// installing something half-understood.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The contract version it is written against.
    pub contract_version: u32,
    /// The agent's id.
    pub id: String,
    /// The agent's name as a human writes it, for listings.
    pub label: String,
    /// Where the integration came from, for listings.
    pub repo: String,
}

/// A resolved integration: its directory and what it declares.
#[derive(Debug)]
pub struct Integration {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// The registry root, `~/.funes/agents` — fixed, not under `$FUNES_HOME`: an agent records the
/// install path it is handed, so these files must outlive any one home, and every integration
/// writes to the agent's own user-scoped config regardless. Which home an install binds travels in
/// the environment instead, per run.
pub fn default_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("resolving $HOME for the agent registry")?;
    Ok(PathBuf::from(home).join(".funes/agents"))
}

/// The ids in `root`: every directory holding a `manifest.json`, sorted. Cheap and unvalidating —
/// it answers what could be resolved, not what is well-formed.
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

/// Resolve `id` in `root`, reading and checking everything the contract requires. Every refusal the
/// registry can make happens here, before `setup` is ever run.
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

    let text =
        std::fs::read_to_string(&manifest_path).with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", manifest_path.display()))?;

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
            manifest_path.display(),
            manifest.id
        );
    }
    // The id is the agent's name everywhere funes uses it: the directory, the `add`/`remove`
    // argument, and the harness facet its turns carry — so it is held to the facet's charset.
    if manifest.id.is_empty()
        || !manifest
            .id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        bail!("the integration id {:?} must be lowercase [a-z0-9_-]", manifest.id);
    }

    let setup = dir.join(SETUP);
    let mode = std::fs::metadata(&setup)
        .with_context(|| format!("{id} has no {SETUP} at {}", setup.display()))?
        .permissions()
        .mode();
    if mode & 0o111 == 0 {
        bail!("{} is not executable", setup.display());
    }

    Ok(Integration { dir, manifest })
}

impl Integration {
    /// Install funes into the agent, bound to `memory` — absent being the local memory, which the
    /// script sees as a missing argument rather than a name it has to special-case.
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

    /// Exec `setup` with the contract's environment, its output passing straight through to the
    /// user. A non-zero exit is the integration reporting failure, and fails the command.
    fn run(&self, args: &[&str]) -> Result<()> {
        let setup = self.dir.join(SETUP);
        let command = super::shell_command(&setup.to_string_lossy(), args);
        let funes = std::env::current_exe().context("locating the running funes binary")?;
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
    /// A directory on this machine, copied as it stands.
    Local(PathBuf),
    /// The archive published in the funes release bucket.
    Published,
}

/// Resolve `id`'s files: `$FUNES_INTEGRATIONS` if set — authoritative, so a test or a fork can
/// never silently reach the network — else the `integrations/` directory of the checkout this
/// binary was built from, else the bucket. A released binary's build path is the builder's and
/// does not exist on the machine that runs it, so it falls through on its own; that is also how a
/// source build is recognised, with no flag to pass.
fn source_for(id: &str) -> Result<Source> {
    if let Some(dir) = std::env::var_os("FUNES_INTEGRATIONS") {
        let dir = PathBuf::from(dir).join(id);
        return dir
            .is_dir()
            .then_some(Source::Local(dir))
            .with_context(|| format!("$FUNES_INTEGRATIONS holds no {id}"));
    }
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations").join(id);
    if checkout.is_dir() {
        return Ok(Source::Local(checkout));
    }
    Ok(Source::Published)
}

/// The bucket prefix holding the integrations written against the contract this funes speaks. One
/// prefix per contract: a fixed integration reaches installed binaries without a new release, and a
/// binary only ever reads the layout it understands.
fn published_prefix() -> String {
    format!("integrations/v{CONTRACT_VERSION}")
}

/// Install `id`'s files into the registry. They are copied, never run where they were found: an
/// agent records the path it is handed, and a checkout can move. Only a file that differs is
/// rewritten, so editing a checkout's script and re-running `add` picks it up; `force` rewrites
/// regardless. Nothing is pruned — an integration's `setup` keeps its own state beside these files.
pub async fn provision(root: &Path, id: &str, force: bool) -> Result<()> {
    let dst = root.join(id);
    match source_for(id)? {
        Source::Local(src) => copy_into(&src, &dst, force),
        Source::Published => {
            let staging = tempfile::tempdir().context("creating a staging directory")?;
            let archive = fetch_published(id, staging.path()).await?;
            let unpacked = staging.path().join("unpacked");
            unpack(&archive, &unpacked)?;
            copy_into(&unpacked, &dst, force)
        }
    }
}

/// Download `id`'s published archive into `dir` and check it against the prefix's `SHA256SUMS`,
/// which is what stands between the bucket and a script funes is about to run.
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
        .with_context(|| format!("downloading {prefix}/{asset} from the funes release bucket"))?;
    hub::verify_checksum(&archive, &manifest, &asset)?;
    Ok(archive)
}

/// Unpack a verified integration archive: its files are at the archive's root, and the executable
/// bits come from the archive.
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

/// Delete an integration's directory: what [`provision`] installed, and whatever its `setup` wrote
/// beside it.
pub fn discard(root: &Path, id: &str) -> Result<()> {
    super::remove_tree(&root.join(id))
}

fn copy_into(src: &Path, dst: &Path, force: bool) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let from = entry?.path();
        let to = dst.join(from.file_name().expect("a directory entry has a file name"));
        // Follows a symlink: a checkout that links one shared script into several integrations
        // still copies the file itself.
        let meta = std::fs::metadata(&from).with_context(|| format!("reading {}", from.display()))?;
        if meta.is_dir() {
            copy_into(&from, &to, force)?;
            continue;
        }
        let bytes = std::fs::read(&from).with_context(|| format!("reading {}", from.display()))?;
        if force || std::fs::read(&to).map(|old| old != bytes).unwrap_or(true) {
            std::fs::write(&to, &bytes).with_context(|| format!("writing {}", to.display()))?;
        }
        let mode = meta.permissions().mode() & 0o777;
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting the mode of {}", to.display()))?;
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

    /// The `add` contract in one run: the argv the script is handed and the environment it can
    /// count on.
    /// The published shape: an archive whose files sit at its root, unpacked and installed with its
    /// executable bits intact. Packed here the way the release workflow packs it.
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

    /// What [`provision`] does with a checkout: nested directories and symlinked shared files come
    /// across as files, the executable bit survives, a drifted copy is refreshed, and state the
    /// `setup` wrote beside them is left alone.
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
        assert_eq!(lines[4], std::env::current_exe().unwrap().to_string_lossy());
    }

    /// A local add and a remove pass the verb alone — no placeholder memory to special-case.
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

    /// A manifest is understood exactly or not at all: a missing field and an unknown one are both
    /// refusals, never a default or a shrug.
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
