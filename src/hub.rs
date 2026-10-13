//! Talking to the Hugging Face Hub: the client, credentials, and dataset-repo identity and
//! lifecycle. It knows nothing about what a memory is — [`crate::memory`] is the domain that
//! interprets what these calls answer. The public release bucket lives here too, with the
//! `SHA256SUMS` every download is checked against.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use hf_hub::{HFBucket, HFClient, HFError, RepoTypeDataset};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use uuid::Uuid;

const USER_AGENT_ORIGIN_ENV: &str = "HF_HUB_USER_AGENT_ORIGIN";
const FUNES_USER_AGENT_ORIGIN: &str = concat!("funes/", env!("CARGO_PKG_VERSION"));
/// Not under `$FUNES_HOME`: the id names the install, not a memory.
const INSTALL_ID_FILE: &str = ".funes/install-id";

/// Tag every Hub client the process creates, while retaining origins supplied by the caller or an
/// agent integration. Call this once at process startup, before starting worker threads.
pub fn configure_user_agent_origin() {
    let current = std::env::var(USER_AGENT_ORIGIN_ENV).ok();
    let opted_out = ["DO_NOT_TRACK", "FUNES_NO_TELEMETRY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
    let install = (!opted_out)
        .then(|| std::env::var_os("HOME"))
        .flatten()
        .and_then(|home| install_id(&PathBuf::from(home).join(INSTALL_ID_FILE)));
    std::env::set_var(
        USER_AGENT_ORIGIN_ENV,
        user_agent_origin(current.as_deref(), install.as_deref()),
    );
}

fn user_agent_origin(current: Option<&str>, install_id: Option<&str>) -> String {
    let install = install_id.map(|id| format!("install/{id}"));
    let mut origins = Vec::new();
    let mut added_funes = false;

    for origin in current
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        // An install/… token was inherited from the funes that spawned this process.
        .filter(|s| !s.is_empty() && !s.starts_with("install/"))
    {
        if origin == "funes" || origin.starts_with("funes/") {
            if !added_funes {
                origins.push(FUNES_USER_AGENT_ORIGIN);
                origins.extend(install.as_deref());
                added_funes = true;
            }
        } else {
            origins.push(origin);
        }
    }

    if !added_funes {
        origins.push(FUNES_USER_AGENT_ORIGIN);
        origins.extend(install.as_deref());
    }
    origins.join("; ")
}

/// Read from `path`, or created there. None when it can't be kept: a fresh id per run would
/// count one install many times.
fn install_id(path: &Path) -> Option<String> {
    if let Some(id) = read_install_id(path) {
        return Some(id);
    }
    let id = Uuid::new_v4().hyphenated().to_string();
    std::fs::create_dir_all(path.parent()?).ok()?;
    // Exclusive, so processes starting together end up with one id: the first writer's.
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file.write_all(format!("{id}\n").as_bytes()).ok().map(|()| id),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => read_install_id(path),
        Err(_) => None,
    }
}

fn read_install_id(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(Uuid::parse_str(text.trim()).ok()?.hyphenated().to_string())
}

/// `<org>/<repo>[/…]` with no scheme and not a path (`/` `.` `~`) → an HF dataset shorthand.
pub fn is_remote_shorthand(spec: &str) -> bool {
    !spec.starts_with(['/', '.', '~']) && spec.contains('/')
}

/// Parse `hf://datasets/<owner>/<name>[/<prefix…>]` into (owner, name, prefix). Empty prefix = repo
/// root, matching how reads resolve a remote.
pub fn parse_hf(uri: &str) -> Result<(String, String, String)> {
    let rest = uri
        .strip_prefix("hf://")
        .context("remote memory must be an hf:// URI")?;
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        ["datasets", owner, name, prefix @ ..] => Ok((owner.to_string(), name.to_string(), prefix.join("/"))),
        _ => anyhow::bail!("expected hf://datasets/<owner>/<name>[/<path>], got {uri}"),
    }
}

/// A transport-level failure (no usable HTTP response) or a 5xx — the cases where degrading to the
/// local index is appropriate (mirrors hf-hub's own transient-error classification).
pub(crate) fn is_offline_error(e: &HFError) -> bool {
    match e {
        HFError::Request { source, .. } => source.is_connect() || source.is_timeout(),
        HFError::Http { context } => matches!(context.status.as_u16(), 500 | 502 | 503 | 504),
        _ => false,
    }
}

/// Build an hf-hub client — the one place the crate does. `retries` is false for the fail-fast
/// reachability/status probes, where hf-hub's default backoff would drag a single offline check out
/// for seconds, and for push, as [`compact`](crate::memory::remote::compact) requires.
pub(crate) fn client(token: Option<&str>, retries: bool) -> Result<HFClient> {
    let mut builder = HFClient::builder();
    if !retries {
        builder = builder.retry_max_attempts(0);
    }
    if let Some(token) = token {
        builder = builder.token(token.to_string());
    }
    builder.build().context("building the Hugging Face client")
}

/// Build an authenticated Hub client, erroring if no token is configured. For the write/identity
/// calls (`whoami`, `create_dataset_repo`) — reads pin their own revision separately.
fn authed_client() -> Result<HFClient> {
    let token = hf_token().context("no Hugging Face token — set HF_TOKEN, or run `hf auth login`")?;
    client(Some(&token), true)
}

/// The authenticated user's Hub handle (`whoami`). Errors if the token is missing or invalid — the
/// caller treats that as "no usable token" and stays local.
pub async fn whoami() -> Result<String> {
    let user = authed_client()?
        .whoami()
        .send()
        .await
        .context("querying your Hugging Face identity")?;
    Ok(user.username)
}

/// Create the dataset repo `<owner>/<name>` on the Hub. Idempotent (`exist_ok`), so it's safe to
/// call when unsure whether it already exists. funes only ever calls this on explicit interactive
/// consent — never implicitly.
pub async fn create_dataset_repo(owner: &str, name: &str) -> Result<()> {
    authed_client()?
        .create_repository()
        .repo_id(format!("{owner}/{name}"))
        .repo_type(RepoTypeDataset)
        // Agent memory is the user's own data — create private; going public is a deliberate act
        // on the Hub. `exist_ok` means an already-created repo keeps whatever visibility it has.
        .private(true)
        .exist_ok(true)
        .send()
        .await
        .with_context(|| format!("creating dataset repo {owner}/{name}"))?;
    Ok(())
}

/// Whether a Hugging Face token is configured — the signal `funes add` uses to decide whether to
/// offer a Hub memory, without exposing the token itself to the binary.
pub fn has_token() -> bool {
    hf_token().is_some()
}

/// HF token from the standard env var, else the `huggingface_hub` cached token file.
pub fn hf_token() -> Option<String> {
    let token_file = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".cache/huggingface/token"));
    token_from(|k| std::env::var(k).ok(), token_file.as_deref())
}

/// Pure core of [`hf_token`]: env vars (in precedence order) win over the token file; blank
/// values are ignored and surrounding whitespace trimmed. Split out so it's testable without
/// mutating process env.
fn token_from(env: impl Fn(&str) -> Option<String>, token_file: Option<&Path>) -> Option<String> {
    for var in ["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN", "HUGGINGFACE_TOKEN"] {
        if let Some(t) = env(var) {
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    let cached = std::fs::read_to_string(token_file?).ok()?;
    let t = cached.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The public bucket (`huggingface/funes`) holding the released binaries, the agent integrations,
/// and the `VERSION` marker.
const BUCKET_OWNER: &str = "huggingface";
const BUCKET_NAME: &str = "funes";

/// An [`HFBucket`] handle for the funes release bucket, with the standard HF token if one is
/// set (the bucket is public, so a token isn't required). `retries` is false for the fail-fast
/// status check, true for the update's default.
pub(crate) fn release_bucket(retries: bool) -> Result<HFBucket> {
    bucket(BUCKET_OWNER, BUCKET_NAME, retries)
}

/// An [`HFBucket`] handle for `owner/name`, with the standard HF token if one is set.
pub(crate) fn bucket(owner: &str, name: &str, retries: bool) -> Result<HFBucket> {
    Ok(client(hf_token().as_deref(), retries)?.bucket(owner, name))
}

/// Split an `hf://buckets/<owner>/<name>/<path>` URL into its three parts.
pub(crate) fn parse_bucket_url(url: &str) -> Result<(String, String, String)> {
    let malformed = || anyhow!("{url} is not an hf://buckets/<owner>/<bucket>/<path> URL");
    let rest = url.strip_prefix("hf://buckets/").ok_or_else(malformed)?;
    let mut parts = rest.splitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), Some(path)) if !owner.is_empty() && !name.is_empty() && !path.is_empty() => {
            Ok((owner.to_string(), name.to_string(), path.to_string()))
        }
        _ => Err(malformed()),
    }
}

/// Verify `asset` against its one unambiguous entry in a strict SHA256SUMS manifest, and say
/// what the digest was.
pub(crate) fn verify_checksum(path: &Path, manifest: &Path, asset: &str) -> Result<[u8; 32]> {
    let text = std::fs::read_to_string(manifest).context("reading SHA256SUMS")?;
    let expected = expected_digest(&text, asset)?;
    let actual = sha256_file(path)?;
    if actual != expected {
        bail!("checksum verification failed for {asset} — nothing changed");
    }
    Ok(actual)
}

/// The public bucket holding the maintained integrations' releases and the catalog that names
/// them — its own, so publishing an integration grants nothing over the binary's releases.
const INTEGRATIONS_BUCKET_OWNER: &str = "huggingface";
const INTEGRATIONS_BUCKET_NAME: &str = "funes-integrations";

/// The catalog of maintained integrations, as an `hf://buckets/…` URL.
pub(crate) fn catalog_url() -> String {
    format!("hf://buckets/{INTEGRATIONS_BUCKET_OWNER}/{INTEGRATIONS_BUCKET_NAME}/catalog.json")
}

pub(crate) fn expected_digest(manifest: &str, asset: &str) -> Result<[u8; 32]> {
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

pub(crate) fn sha256_file(path: &Path) -> Result<[u8; 32]> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_origin_defaults_to_versioned_funes() {
        assert_eq!(user_agent_origin(None, None), FUNES_USER_AGENT_ORIGIN);
        assert_eq!(
            user_agent_origin(None, Some("abc")),
            format!("{FUNES_USER_AGENT_ORIGIN}; install/abc")
        );
    }

    #[test]
    fn user_agent_origin_keeps_caller_origins() {
        assert_eq!(
            user_agent_origin(Some("custom/2.0; deployment/private"), None),
            format!("custom/2.0; deployment/private; {FUNES_USER_AGENT_ORIGIN}")
        );
    }

    #[test]
    fn user_agent_origin_versions_and_deduplicates_integration_token() {
        assert_eq!(
            user_agent_origin(Some("funes; agent/claude; agent_version/2.1.296"), Some("abc")),
            format!("{FUNES_USER_AGENT_ORIGIN}; install/abc; agent/claude; agent_version/2.1.296")
        );
        assert_eq!(
            user_agent_origin(Some("funes/1.6.0; agent/hermes; funes"), None),
            format!("{FUNES_USER_AGENT_ORIGIN}; agent/hermes")
        );
    }

    #[test]
    fn user_agent_origin_replaces_an_inherited_install_token() {
        let inherited = format!("{FUNES_USER_AGENT_ORIGIN}; install/parent; agent/hermes");
        assert_eq!(
            user_agent_origin(Some(&inherited), Some("child")),
            format!("{FUNES_USER_AGENT_ORIGIN}; install/child; agent/hermes")
        );
        assert_eq!(
            user_agent_origin(Some(&inherited), None),
            format!("{FUNES_USER_AGENT_ORIGIN}; agent/hermes")
        );
    }

    #[test]
    fn install_id_is_created_once_and_reused() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(INSTALL_ID_FILE);
        let id = install_id(&path).unwrap();
        assert_eq!(Uuid::parse_str(&id).unwrap().get_version(), Some(uuid::Version::Random));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{id}\n"));
        assert_eq!(install_id(&path).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn install_id_ignores_a_file_that_is_not_a_uuid() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("install-id");
        std::fs::write(&path, "not an id\n").unwrap();
        assert_eq!(install_id(&path), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not an id\n");
    }

    #[test]
    fn concurrent_first_runs_share_one_install_id() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("install-id");
        let ids: Vec<Option<String>> = std::thread::scope(|scope| {
            let runs: Vec<_> = (0..20).map(|_| scope.spawn(|| install_id(&path))).collect();
            runs.into_iter().map(|run| run.join().unwrap()).collect()
        });
        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(ids.iter().flatten().count() >= 1);
        assert!(ids.iter().flatten().all(|id| format!("{id}\n") == kept));
    }

    #[test]
    fn install_id_is_omitted_where_it_cannot_be_kept() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(install_id(&file.join("install-id")), None);
    }

    #[test]
    fn bucket_urls_split_into_owner_name_and_path() {
        assert_eq!(
            parse_bucket_url("hf://buckets/acme/funes-clyde/clyde/1.0.0/clyde.tar.gz").unwrap(),
            ("acme".into(), "funes-clyde".into(), "clyde/1.0.0/clyde.tar.gz".into())
        );
        for malformed in [
            "hf://datasets/acme/kb/x",
            "https://example.com/x.tar.gz",
            "hf://buckets/acme/funes-clyde",
            "hf://buckets//funes-clyde/x",
            "hf://buckets/acme/funes-clyde/",
        ] {
            assert!(parse_bucket_url(malformed).is_err(), "{malformed}");
        }
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
    use std::collections::HashMap;

    #[test]
    fn parse_hf_accepts_repo_root_and_a_prefix() {
        // repo root: no path after <owner>/<name> -> empty prefix
        assert_eq!(
            parse_hf("hf://datasets/acme/kb").unwrap(),
            ("acme".into(), "kb".into(), "".into())
        );
        // an explicit path within the repo is kept
        assert_eq!(
            parse_hf("hf://datasets/acme/kb/sub/dir").unwrap(),
            ("acme".into(), "kb".into(), "sub/dir".into())
        );
        // not a dataset URI -> error
        assert!(parse_hf("hf://acme/kb").is_err());
        assert!(parse_hf("s3://acme/kb").is_err());
    }

    #[test]
    fn token_env_beats_file_and_trims() {
        let env: HashMap<&str, &str> = [("HF_TOKEN", "  hf_envtok \n")].into_iter().collect();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "hf_filetok").unwrap();
        let got = token_from(|k| env.get(k).map(|s| s.to_string()), Some(file.path()));
        assert_eq!(got.as_deref(), Some("hf_envtok")); // env wins, trimmed
    }

    #[test]
    fn token_falls_back_to_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "  hf_filetok\n").unwrap();
        let got = token_from(|_| None, Some(file.path()));
        assert_eq!(got.as_deref(), Some("hf_filetok"));
    }

    #[test]
    fn token_blank_env_is_skipped_none_when_no_file() {
        let env: HashMap<&str, &str> = [("HF_TOKEN", "   ")].into_iter().collect();
        assert_eq!(token_from(|k| env.get(k).map(|s| s.to_string()), None), None);
    }
}
