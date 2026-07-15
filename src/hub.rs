//! Remote, shared memory tiers.
//!
//! A [`Store`] is either the local index or a remote Lance dataset on the HF Hub. A remote open pins
//! reads to the head commit and installs a read wrapper over Lance's object store; the pin is
//! re-resolved on every open, so a new push is picked up by the next command.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hf_hub::{HFClient, HFError, RepoTypeDataset};
use lance::dataset::Dataset;

use crate::dataset;
use crate::hf_dataset;
use crate::index::{DIM, MODEL};

/// A store to recall from: a local Lance directory or a remote dataset on the HF Hub.
#[derive(Debug, Clone)]
pub enum Store {
    /// A local Lance store directory (e.g. `~/.funes/store`).
    Local { path: PathBuf },
    /// A remote Lance dataset on the HF Hub, e.g. `hf://datasets/<org>/<repo>`.
    Remote { uri: String },
}

/// The branch reads pin to and resolve the head commit of. It must be the branch writes target, so a
/// read sees the commits a push produced.
const READ_BRANCH: &str = "main";

impl Store {
    /// The default local store (`$FUNES_HOME` / `~/.funes` → `…/store`).
    pub fn local() -> Self {
        Store::Local {
            path: PathBuf::from(dataset::local_store_dir()),
        }
    }

    /// Parse a store spec: `"local"` → the local store; an `hf://…` URI or `<org>/<repo>` shorthand
    /// → a remote; a path (`/`, `.`, `~`, or a bare name) → a local store there.
    pub fn parse(spec: &str) -> Self {
        if spec == "local" {
            Store::local()
        } else if spec.starts_with("hf://") {
            Store::Remote { uri: spec.to_string() }
        } else if is_remote_shorthand(spec) {
            Store::Remote {
                uri: format!("hf://datasets/{spec}"),
            }
        } else {
            Store::Local {
                path: PathBuf::from(spec),
            }
        }
    }

    /// Resolve the store the read commands should use: an explicit `spec` (a CLI `--store`), else
    /// the local index. There is no persisted default — a store binding lives in the caller's
    /// config (e.g. an agent's `funes mcp --store …` registration), not in funes.
    pub fn resolve(spec: Option<String>) -> Self {
        match spec.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            Some(s) => Store::parse(&s),
            None => Store::local(),
        }
    }

    /// True only for the default local store (`$FUNES_HOME`/`~/.funes`), so the hello-world
    /// fallback fires only there — never masking a missing explicit store.
    pub fn is_default_local(&self) -> bool {
        matches!(self, Store::Local { path } if path.as_path() == Path::new(&dataset::local_store_dir()))
    }

    /// Short label for output/provenance.
    pub fn label(&self) -> String {
        match self {
            Store::Local { path } => path.display().to_string(),
            Store::Remote { uri, .. } => uri.clone(),
        }
    }

    /// Open the `chunks` dataset for this store; remote stores stream lazily over `hf://`.
    /// Rejects a store whose `vector` dimension isn't funes's `DIM` — a coarse guard, since a
    /// matching dimension doesn't prove a matching embedding model.
    pub async fn open(&self) -> Result<Dataset> {
        let ds = match self {
            Store::Local { path } => {
                dataset::open(&dataset::table_uri(&path.to_string_lossy()), HashMap::new()).await?
            }
            Store::Remote { uri } => {
                let (owner, name, _) = parse_hf(uri)?;
                let token = hf_token();
                let mut opts = HashMap::new();
                if let Some(t) = &token {
                    opts.insert("hf_token".to_string(), t.clone());
                }
                let table = dataset::table_uri(uri);
                // Pin reads to the head commit and install the read wrapper. The pin is re-resolved
                // on every open, so a new push is picked up by the next command. If the head can't
                // be resolved (offline/transient), degrade to a plain live open rather than fail.
                match hf_dataset::fetch_wrapper(&owner, &name, token.as_deref(), READ_BRANCH).await {
                    Ok((wrapper, sha)) => {
                        opts.insert("hf_revision".to_string(), sha);
                        dataset::open_wrapped(&table, opts, wrapper).await?
                    }
                    Err(_) => dataset::open(&table, opts).await?,
                }
            }
        };
        check_compat(&ds)?;
        Ok(ds)
    }
}

/// `<org>/<repo>[/…]` with no scheme and not a path (`/` `.` `~`) → an HF dataset shorthand.
pub fn is_remote_shorthand(spec: &str) -> bool {
    !spec.starts_with(['/', '.', '~']) && spec.contains('/')
}

/// Parse `hf://datasets/<owner>/<name>[/<prefix…>]` into (owner, name, prefix). Empty prefix = repo
/// root, matching how reads resolve a remote.
pub fn parse_hf(uri: &str) -> Result<(String, String, String)> {
    let rest = uri.strip_prefix("hf://").context("remote store must be an hf:// URI")?;
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        ["datasets", owner, name, prefix @ ..] => Ok((owner.to_string(), name.to_string(), prefix.join("/"))),
        _ => anyhow::bail!("expected hf://datasets/<owner>/<name>[/<path>], got {uri}"),
    }
}

/// How long the reachability probe waits before treating a remote as offline.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a lightweight probe of a remote dataset repo found.
pub enum Reachability {
    /// The repo answered — a read or push can proceed.
    Ok,
    /// No usable response (no connection, DNS, timeout, or 5xx): treat as offline.
    Offline,
    /// The repo does not exist on the Hub. funes never creates it.
    Missing,
}

/// Probe the remote dataset repo at `uri`. A 403/auth answer counts as [`Reachability::Ok`] — that's
/// a real error the open or commit should surface, not an offline or missing signal.
pub async fn remote_reachability(uri: &str) -> Reachability {
    let Ok((owner, name, _)) = parse_hf(uri) else {
        return Reachability::Ok; // not an hf:// dataset URI — let the open/commit report the real error
    };
    // No retries: this is a reachability check, so one failed request already means offline. Without
    // this, hf-hub's default exponential backoff (5 attempts) would drag the probe out for seconds.
    let mut builder = HFClient::builder().retry_max_attempts(0);
    if let Some(token) = hf_token() {
        builder = builder.token(token);
    }
    let Ok(client) = builder.build() else {
        return Reachability::Ok;
    };
    let repo = client.dataset(owner, name);
    match tokio::time::timeout(PROBE_TIMEOUT, repo.info().send()).await {
        Err(_elapsed) => Reachability::Offline,
        Ok(Ok(_)) => Reachability::Ok,
        Ok(Err(HFError::RepoNotFound { .. })) => Reachability::Missing,
        Ok(Err(e)) if is_offline_error(&e) => Reachability::Offline,
        Ok(Err(_)) => Reachability::Ok,
    }
}

/// A transport-level failure (no usable HTTP response) or a 5xx — the cases where degrading to the
/// local index is appropriate (mirrors hf-hub's own transient-error classification).
fn is_offline_error(e: &HFError) -> bool {
    match e {
        HFError::Request { source, .. } => source.is_connect() || source.is_timeout(),
        HFError::Http { context } => matches!(context.status.as_u16(), 500 | 502 | 503 | 504),
        _ => false,
    }
}

/// The active remote repo doesn't exist on the Hub — funes never creates it. Shared by recall,
/// `push`, and `use` so the message is identical everywhere.
pub fn missing_remote(uri: &str) -> anyhow::Error {
    anyhow!(
        "{uri} doesn't exist on the Hub, and funes won't create it — create the dataset repo \
         first (https://huggingface.co/new-dataset)"
    )
}

/// The remote repo exists but holds no index yet.
pub fn empty_remote(uri: &str) -> anyhow::Error {
    anyhow!(
        "{uri} exists on the Hub but holds no index yet — `funes push {uri}` to publish your local \
         index there, or drop `--store` to read your local index"
    )
}

/// The Hub refused the read on auth (HTTP 401/403): no HF token, or one that can't read this
/// dataset. Shared so the message is identical everywhere.
pub fn unauthorized_remote(uri: &str) -> anyhow::Error {
    anyhow!(
        "not authorized to read {uri} — set a Hugging Face token with read access to this dataset \
         (HF_TOKEN, or `hf auth login`), or check the token you have can read it."
    )
}

/// Build an authenticated Hub client, erroring if no token is configured. For the write/identity
/// calls (`whoami`, `create_dataset_repo`) — reads pin their own revision separately.
fn authed_client() -> Result<HFClient> {
    let token = hf_token().context("no Hugging Face token — set HF_TOKEN, or run `hf auth login`")?;
    HFClient::builder()
        .token(token)
        .build()
        .context("building the Hugging Face client")
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
        .exist_ok(true)
        .send()
        .await
        .with_context(|| format!("creating dataset repo {owner}/{name}"))?;
    Ok(())
}

/// Whether a Hugging Face token is configured — the signal `funes add` uses to decide whether to
/// offer a Hub store, without exposing the token itself to the binary.
pub fn has_token() -> bool {
    hf_token().is_some()
}

/// HF token from the standard env var, else the `huggingface_hub` cached token file.
pub(crate) fn hf_token() -> Option<String> {
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

/// Reject a store funes can't query with its own embeddings: the `vector` dimension must be
/// funes's `DIM`, and — when the store records an embedding model in its schema metadata — that
/// model must be funes's. A store with no recorded model (pre-metadata) is guarded by the
/// dimension alone.
fn check_compat(ds: &Dataset) -> Result<()> {
    let schema = arrow_schema::Schema::from(ds.schema());

    if let Some(model) = schema.metadata().get("embedding_model") {
        if model != MODEL {
            return Err(anyhow!(
                "store built with embedding model {model:?}, not funes's {MODEL:?}"
            ));
        }
    }

    let field = schema
        .field_with_name("vector")
        .map_err(|_| anyhow!("store has no `vector` column"))?;
    if let arrow_schema::DataType::FixedSizeList(_, dim) = field.data_type() {
        if *dim != DIM {
            return Err(anyhow!(
                "store vector dim {dim} != funes's {DIM}; it was built with a different embedding model"
            ));
        }
        Ok(())
    } else {
        Err(anyhow!("store `vector` column is not a fixed-size list"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use arrow_array::types::Float32Type;
    use arrow_array::{ArrayRef, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator};
    use arrow_schema::{DataType, Field, Schema};

    #[test]
    fn store_parse_local_remote_and_shorthand() {
        assert!(matches!(Store::parse("local"), Store::Local { .. }));
        // explicit local paths (leading / . ~)
        match Store::parse("/tmp/store") {
            Store::Local { path } => assert_eq!(path, std::path::PathBuf::from("/tmp/store")),
            _ => panic!("expected a local path"),
        }
        assert!(matches!(Store::parse("./rel/dir"), Store::Local { .. }));
        // full hf:// URI
        match Store::parse("hf://datasets/org/kb") {
            Store::Remote { uri } => assert_eq!(uri, "hf://datasets/org/kb"),
            _ => panic!("expected remote"),
        }
        // org/repo shorthand expands to a dataset URI
        match Store::parse("acme/kb") {
            Store::Remote { uri } => assert_eq!(uri, "hf://datasets/acme/kb"),
            _ => panic!("expected remote from shorthand"),
        }
    }

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

    #[tokio::test]
    async fn non_hf_uri_is_reachable_ok() {
        // A spec that isn't an hf:// dataset URI can't be probed; it reports Ok so the open
        // surfaces the real error rather than masking it as offline. (No network is touched.)
        assert!(matches!(remote_reachability("/local/path").await, Reachability::Ok));
        assert!(matches!(remote_reachability("not a uri").await, Reachability::Ok));
    }

    #[test]
    fn store_label() {
        assert_eq!(Store::Local { path: "/tmp/x".into() }.label(), "/tmp/x");
        assert_eq!(Store::parse("hf://datasets/org/kb").label(), "hf://datasets/org/kb");
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

    #[test]
    fn resolve_prefers_explicit_spec_else_local() {
        // Explicit spec wins, with the org/repo shorthand applied.
        match Store::resolve(Some("acme/kb".into())) {
            Store::Remote { uri } => assert_eq!(uri, "hf://datasets/acme/kb"),
            _ => panic!("explicit spec should win"),
        }
        // No spec -> local (there is no persisted default).
        assert!(matches!(Store::resolve(None), Store::Local { .. }));
        // An explicit local path stays local.
        match Store::resolve(Some("/local/path".into())) {
            Store::Local { path } => assert_eq!(path, std::path::PathBuf::from("/local/path")),
            _ => panic!("explicit local path should resolve local"),
        }
        // Blank spec -> local.
        assert!(matches!(Store::resolve(Some("   ".into())), Store::Local { .. }));
    }

    // --- dim guard against real local datasets ---

    async fn dataset_with(fields: Vec<Field>, columns: Vec<ArrayRef>) -> (tempfile::TempDir, Dataset) {
        let dir = tempfile::tempdir().unwrap();
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        let uri = format!("{}/chunks.lance", dir.path().to_str().unwrap());
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        let ds = Dataset::write(reader, &uri, None).await.unwrap();
        (dir, ds)
    }

    fn ids(n: usize) -> ArrayRef {
        Arc::new((0..n as i64).map(Some).collect::<Int64Array>())
    }

    fn vectors(n: usize, dim: i32) -> ArrayRef {
        Arc::new(FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            (0..n).map(|_| Some((0..dim).map(|_| Some(0.0f32)).collect::<Vec<_>>())),
            dim,
        ))
    }

    fn vector_field(dim: i32) -> Field {
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            true,
        )
    }

    #[tokio::test]
    async fn check_compat_accepts_matching_dimension() {
        let (_d, ds) = dataset_with(
            vec![Field::new("id", DataType::Int64, true), vector_field(DIM)],
            vec![ids(2), vectors(2, DIM)],
        )
        .await;
        assert!(check_compat(&ds).is_ok());
    }

    #[tokio::test]
    async fn check_compat_rejects_wrong_dimension() {
        let (_d, ds) = dataset_with(
            vec![Field::new("id", DataType::Int64, true), vector_field(DIM / 2)],
            vec![ids(2), vectors(2, DIM / 2)],
        )
        .await;
        let err = check_compat(&ds).unwrap_err().to_string();
        assert!(err.contains("different embedding model"), "{err}");
    }

    #[tokio::test]
    async fn check_compat_rejects_missing_or_scalar_vector() {
        // no vector column
        let (_d, d1) = dataset_with(vec![Field::new("id", DataType::Int64, true)], vec![ids(2)]).await;
        assert!(check_compat(&d1).is_err());

        // a `vector` column that isn't a fixed-size list
        let (_d2, d2) = dataset_with(
            vec![
                Field::new("id", DataType::Int64, true),
                Field::new("vector", DataType::Int64, true),
            ],
            vec![ids(2), ids(2)],
        )
        .await;
        assert!(check_compat(&d2).is_err());
    }

    #[tokio::test]
    async fn check_compat_rejects_wrong_model() {
        let dir = tempfile::tempdir().unwrap();
        let schema = Arc::new(Schema::new_with_metadata(
            vec![Field::new("id", DataType::Int64, true), vector_field(DIM)],
            HashMap::from([("embedding_model".to_string(), "some/other-model".to_string())]),
        ));
        let batch = RecordBatch::try_new(schema.clone(), vec![ids(2), vectors(2, DIM)]).unwrap();
        let uri = format!("{}/chunks.lance", dir.path().to_str().unwrap());
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        let ds = Dataset::write(reader, &uri, None).await.unwrap();
        let err = check_compat(&ds).unwrap_err().to_string();
        assert!(err.contains("other-model") && err.contains("not funes's"), "{err}");
    }
}
