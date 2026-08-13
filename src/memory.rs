//! The memory funes reads from and writes to: a local Lance directory, or a shared dataset on the
//! Hugging Face Hub.
//!
//! This root is the **domain**: what a [`Memory`] is, how a spec resolves to one, what state it is
//! in ([`MemoryState`], via [`Memory::state`]) and the messages that state renders as. A remote open
//! pins reads to the head commit and installs a read wrapper over Lance's object store; the pin is
//! re-resolved on every open, so a new push is picked up by the next command.
//!
//! Below it: [`dataset`], [`fetch_store`] and [`capture_store`] are **mechanics** — Lance and object
//! stores, knowing nothing about the Hub. [`remote`] is **transport** — the pinned reads and the
//! single guarded commit an append or reindex lands as, over the Hub client in [`crate::hub`].
//! [`card`] serves a published memory's dataset card, [`lock`] the local writer lock.
//!
//! The commands ask this module what state a memory is in; they never infer it from error shapes.

pub mod capture_store;
pub mod card;
pub mod dataset;
pub mod fetch_store;
pub mod lock;
pub mod remote;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use hf_hub::HFError;
use lance::dataset::Dataset;

use crate::hub::{client, hf_token, is_offline_error, is_remote_shorthand, parse_hf};
use dataset::{DIM, MODEL};

/// A memory to recall from: a local Lance directory or a remote dataset on the HF Hub.
#[derive(Debug, Clone)]
pub enum Memory {
    /// A local Lance memory directory (e.g. `~/.funes/memory`).
    Local { path: PathBuf },
    /// A remote Lance dataset on the HF Hub, e.g. `hf://datasets/<org>/<repo>`.
    Remote { uri: String },
}

/// The branch reads pin to and resolve the head commit of. It must be the branch writes target, so a
/// read sees the commits a push produced.
const READ_BRANCH: &str = "main";

impl Memory {
    /// The default local memory (`$FUNES_HOME` / `~/.funes` → `…/memory`).
    pub fn local() -> Self {
        Memory::Local {
            path: PathBuf::from(dataset::local_memory_dir()),
        }
    }

    /// Parse a memory spec: `"local"` → the local memory; an `hf://…` URI or `<org>/<repo>` shorthand
    /// → a remote; a path (`/`, `.`, `~`, or a bare name) → a local memory there.
    pub fn parse(spec: &str) -> Self {
        if spec == "local" {
            Memory::local()
        } else if spec.starts_with("hf://") {
            Memory::Remote { uri: spec.to_string() }
        } else if is_remote_shorthand(spec) {
            Memory::Remote {
                uri: format!("hf://datasets/{spec}"),
            }
        } else {
            Memory::Local {
                path: PathBuf::from(spec),
            }
        }
    }

    /// Resolve the memory the read commands should use: an explicit `spec` (a CLI `--memory`), else
    /// the local index. There is no persisted default — a memory binding lives in the caller's
    /// config (e.g. an agent's `funes mcp <memory>` registration), not in funes.
    pub fn resolve(spec: Option<String>) -> Self {
        match spec.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            Some(s) => Memory::parse(&s),
            None => Memory::local(),
        }
    }

    /// True only for the default local memory (`$FUNES_HOME`/`~/.funes`), so the hello-world
    /// fallback fires only there — never masking a missing explicit memory.
    pub fn is_default_local(&self) -> bool {
        matches!(self, Memory::Local { path } if path.as_path() == Path::new(&dataset::local_memory_dir()))
    }

    /// Short label for output/provenance.
    pub fn label(&self) -> String {
        match self {
            Memory::Local { path } => path.display().to_string(),
            Memory::Remote { uri, .. } => uri.clone(),
        }
    }

    /// Open the `chunks` dataset for this memory; remote memories stream lazily over `hf://`.
    /// Rejects a memory whose `vector` dimension isn't funes's `DIM` — a coarse guard, since a
    /// matching dimension doesn't prove a matching embedding model.
    pub async fn open(&self) -> Result<Dataset> {
        let ds = match self {
            Memory::Local { path } => {
                dataset::open(&dataset::table_uri(&path.to_string_lossy()), HashMap::new()).await?
            }
            Memory::Remote { uri } => {
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
                match remote::fetch_wrapper(&owner, &name, token.as_deref(), READ_BRANCH).await {
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

    /// What state this memory is in — the one answer the commands act on, so none of them has to
    /// read it out of an error. A remote is probed first (an unreachable or absent repo costs no
    /// open), then the open decides between ready, empty and refused.
    ///
    /// `Err` is reserved for a memory that exists and still can't be read — a permission problem, a
    /// corrupt dataset, an incompatible schema. Callers must never treat that as [`Empty`]:
    /// mistaking an unreadable memory for an absent one turns a mixed-version teammate's push into
    /// a first publish over live data.
    ///
    /// [`Empty`]: MemoryState::Empty
    pub async fn state(&self) -> Result<MemoryState> {
        if let Memory::Remote { uri } = self {
            match remote_reachability(uri).await {
                Reachability::Offline => return Ok(MemoryState::Offline),
                Reachability::Missing => return Ok(MemoryState::Missing),
                Reachability::Ok => {}
            }
        }
        match self.open().await {
            Ok(ds) => Ok(MemoryState::Ready(ds)),
            // A gated repo answers `info()` fine and 403s only here, on the file read — so the open
            // is the only place this is knowable. Local paths keep their own error: a
            // `PermissionDenied` there is a filesystem problem, not the Hub refusing a read.
            Err(e) if matches!(self, Memory::Remote { .. }) && is_auth_error(&e) => Ok(MemoryState::Unauthorized),
            Err(e) if dataset_absent(&e) => Ok(MemoryState::Empty),
            Err(e) => Err(e),
        }
    }
}

/// How the states that always refuse read to the user. One method per [`MemoryState`] variant that
/// a command can only stop on, so the state and its wording are named the same thing at the call
/// site and can't drift apart: `MemoryState::Missing => Err(memory.missing_error())`.
impl Memory {
    /// [`MemoryState::Missing`]: the repo isn't on the Hub, and funes never creates it.
    pub fn missing_error(&self) -> anyhow::Error {
        anyhow!(
            "{} doesn't exist on the Hub, and funes won't create it — create the dataset repo \
             first (https://huggingface.co/new-dataset)",
            self.label()
        )
    }

    /// [`MemoryState::Unauthorized`]: the Hub refused the read (401/403) — no token, a token
    /// without access to this dataset, or terms not accepted.
    pub fn unauthorized_error(&self) -> anyhow::Error {
        anyhow!(
            "not authorized to read {} — set a Hugging Face token with read access to this dataset \
             (HF_TOKEN, or `hf auth login`), or check the token you have can read it.",
            self.label()
        )
    }

    /// [`MemoryState::Empty`]: there's nothing to read. A remote is one push away from useful; a
    /// local path just isn't a memory yet. (The *default* local memory means a fresh install, which
    /// the read verbs answer with their own onboarding line.)
    pub fn empty_error(&self) -> anyhow::Error {
        match self {
            Memory::Remote { uri } => anyhow!(
                "{uri} exists on the Hub but holds no index yet — `funes push {uri}` to publish \
                 your local index there, or drop `--memory` to read your local memory"
            ),
            Memory::Local { path } => anyhow!("no index found at {}", path.display()),
        }
    }
}

/// What state a memory is in. Produced by [`Memory::state`] — the one classifier — so a command
/// matches on facts instead of sniffing error shapes.
// A transient return value, never stored en masse, so the `Ready(Dataset)`/unit size gap is fine —
// boxing would only add indirection.
#[allow(clippy::large_enum_variant)]
pub enum MemoryState {
    /// Opened, compatible, ready to query.
    Ready(Dataset),
    /// Nothing there yet: a local memory with no dataset, or a repo never pushed to.
    Empty,
    /// A remote repo that doesn't exist on the Hub. funes never creates it.
    Missing,
    /// A remote funes can't reach right now (no connection, DNS, timeout, 5xx).
    Offline,
    /// The Hub refused the read (401/403): no token, a token without access, or terms not accepted.
    Unauthorized,
}

/// True if `e` is the Hub refusing a remote read on auth (401/403). lance has no typed auth variant
/// — it buries an opendal `PermissionDenied` (with the HTTP status) in an `IO` error — so match the
/// chain's text.
fn is_auth_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string();
        s.contains("PermissionDenied") || s.contains("status: 401") || s.contains("status: 403")
    })
}

/// How long the reachability probe waits before treating a remote as offline.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a lightweight probe of a remote dataset **repo** found — repo-level only, so it costs no
/// dataset open. [`Memory::state`] builds on it; `funes add`'s typo guard uses it on its own, where
/// opening the dataset would be overreach (the repo is expected to be empty at that point).
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
    // No retries: this is a reachability check, so one failed request already means offline.
    let repo = match client(hf_token().as_deref(), false) {
        Ok(c) => c.dataset(owner, name),
        Err(_) => return Reachability::Ok,
    };
    match tokio::time::timeout(PROBE_TIMEOUT, repo.info().send()).await {
        Err(_elapsed) => Reachability::Offline,
        Ok(Ok(_)) => Reachability::Ok,
        Ok(Err(HFError::RepoNotFound { .. })) => Reachability::Missing,
        Ok(Err(e)) if is_offline_error(&e) => Reachability::Offline,
        Ok(Err(_)) => Reachability::Ok,
    }
}

/// Whether a [`Memory::open`] failure means the dataset does not exist (a missing table in an
/// otherwise-reachable repo) — as opposed to one that exists but can't be read (a
/// [`check_compat`] rejection, a transport failure). Callers must treat only the former as
/// "empty": mistaking an unreadable memory for an absent one turns a mixed-version teammate's
/// push into a first publish over live data.
fn dataset_absent(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(hf) = cause.downcast_ref::<HFError>() {
            return matches!(hf, HFError::EntryNotFound { .. });
        }
        if let Some(lance) = cause.downcast_ref::<lance::Error>() {
            return matches!(lance, lance::Error::DatasetNotFound { .. });
        }
        false
    })
}

/// Reject a memory funes can't query with its own embeddings: the `vector` dimension must be
/// funes's `DIM`, and — when the memory records an embedding model in its schema metadata — that
/// model must be funes's. A memory with no recorded model (pre-metadata) is guarded by the
/// dimension alone.
fn check_compat(ds: &Dataset) -> Result<()> {
    let schema = arrow_schema::Schema::from(ds.schema());

    if let Some(model) = schema.metadata().get("embedding_model") {
        if model != MODEL {
            return Err(anyhow!(
                "memory built with embedding model {model:?}, not funes's {MODEL:?}"
            ));
        }
    }

    let field = schema
        .field_with_name("vector")
        .map_err(|_| anyhow!("memory has no `vector` column"))?;
    if let arrow_schema::DataType::FixedSizeList(_, dim) = field.data_type() {
        if *dim != DIM {
            return Err(anyhow!(
                "memory vector dim {dim} != funes's {DIM}; it was built with a different embedding model"
            ));
        }
        Ok(())
    } else {
        Err(anyhow!("memory `vector` column is not a fixed-size list"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow_array::types::Float32Type;
    use arrow_array::{ArrayRef, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator};
    use arrow_schema::{DataType, Field, Schema};

    #[test]
    fn dataset_absent_matches_the_type_not_the_message() {
        // Chain-typed only — text that merely mentions "not found" is not an absence signal.
        // (The Hub-side positive path carries a #[non_exhaustive] error that can't be built here;
        // the gated round-trip exercises it live — its first publish runs through this.)
        assert!(!dataset_absent(&anyhow::anyhow!("404 Entry not found")));
        assert!(!dataset_absent(&anyhow::anyhow!("Dataset not found: chunks")));
    }

    #[test]
    fn auth_error_is_detected() {
        // Shape verified against a live 401: an opendal PermissionDenied with the HTTP status.
        let err = anyhow::anyhow!(
            "LanceError(IO): Generic PermissionDenied error: PermissionDenied (permanent) at list, \
             response: status: 401"
        );
        assert!(is_auth_error(&err));
    }

    #[test]
    fn unrelated_error_is_not_auth_error() {
        assert!(!is_auth_error(&anyhow::anyhow!("some other failure")));
        // a missing-dataset error must not be misread as auth
        assert!(!is_auth_error(&anyhow::anyhow!(
            "LanceError: DatasetNotFound, no such file"
        )));
    }

    #[tokio::test]
    async fn dataset_absent_matches_a_real_missing_dataset() {
        // Opening a path with no dataset is lance's DatasetNotFound — the absent case every read
        // and the first publish classify on.
        let err = dataset::open("/nonexistent/funes-empty-memory/chunks.lance", HashMap::new())
            .await
            .unwrap_err();
        assert!(dataset_absent(&err));
    }

    #[test]
    fn memory_parse_local_remote_and_shorthand() {
        assert!(matches!(Memory::parse("local"), Memory::Local { .. }));
        // explicit local paths (leading / . ~)
        match Memory::parse("/tmp/memory") {
            Memory::Local { path } => assert_eq!(path, std::path::PathBuf::from("/tmp/memory")),
            _ => panic!("expected a local path"),
        }
        assert!(matches!(Memory::parse("./rel/dir"), Memory::Local { .. }));
        // full hf:// URI
        match Memory::parse("hf://datasets/org/kb") {
            Memory::Remote { uri } => assert_eq!(uri, "hf://datasets/org/kb"),
            _ => panic!("expected remote"),
        }
        // org/repo shorthand expands to a dataset URI
        match Memory::parse("acme/kb") {
            Memory::Remote { uri } => assert_eq!(uri, "hf://datasets/acme/kb"),
            _ => panic!("expected remote from shorthand"),
        }
    }

    #[tokio::test]
    async fn non_hf_uri_is_reachable_ok() {
        // A spec that isn't an hf:// dataset URI can't be probed; it reports Ok so the open
        // surfaces the real error rather than masking it as offline. (No network is touched.)
        assert!(matches!(remote_reachability("/local/path").await, Reachability::Ok));
        assert!(matches!(remote_reachability("not a uri").await, Reachability::Ok));
    }

    #[test]
    fn memory_label() {
        assert_eq!(Memory::Local { path: "/tmp/x".into() }.label(), "/tmp/x");
        assert_eq!(Memory::parse("hf://datasets/org/kb").label(), "hf://datasets/org/kb");
    }

    #[test]
    fn resolve_prefers_explicit_spec_else_local() {
        // Explicit spec wins, with the org/repo shorthand applied.
        match Memory::resolve(Some("acme/kb".into())) {
            Memory::Remote { uri } => assert_eq!(uri, "hf://datasets/acme/kb"),
            _ => panic!("explicit spec should win"),
        }
        // No spec -> local (there is no persisted default).
        assert!(matches!(Memory::resolve(None), Memory::Local { .. }));
        // An explicit local path stays local.
        match Memory::resolve(Some("/local/path".into())) {
            Memory::Local { path } => assert_eq!(path, std::path::PathBuf::from("/local/path")),
            _ => panic!("explicit local path should resolve local"),
        }
        // Blank spec -> local.
        assert!(matches!(Memory::resolve(Some("   ".into())), Memory::Local { .. }));
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
