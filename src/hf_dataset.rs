//! Append and reindex for the remote HF Lance dataset.
//!
//! [`append`] adds rows; [`reindex`] folds the unindexed backlog into the FTS/IVF indexes. Each
//! runs a native Lance op and lands the result in one `create_commit` on the branch, guarded by a
//! `parent_commit` against the head it read — atomic. Each is a single attempt: if the head moved
//! first it reports a conflict ([`Appended::Conflict`] / [`Reindexed::Conflict`]) and the caller
//! retries against the new head.
//!
//! The result goes up as a *single* `create_commit` because Lance, left to write straight to
//! `hf://`, would commit each file on its own: that store is OpenDAL's HuggingFace service, where
//! every `put` is its own git commit.
//!
//! ```text
//!   Lance Dataset → object_store → OpenDAL hf service → HF Hub
//!       put = XET upload + one git commit, per file
//! ```
//!
//! A multi-file write would then be several commits — non-atomic, no CAS. So the op runs through a
//! [`CaptureStore`](crate::capture_store::CaptureStore) installed via Lance's
//! [`WrappingObjectStore`] seam: Lance's writes are captured in memory instead of hitting the Hub,
//! and we ship the whole set as one guarded `create_commit`.
//!
//! # Why this shape
//!
//! **Intercept at the object-store layer.** Every file an append or optimize produces — data
//! fragment, manifest, transaction, index — is written through `object_store`, so it is the one
//! hook that captures the *whole* write set with no knowledge of Lance's on-disk layout. A
//! narrower seam can't do it: a custom `CommitHandler` only governs the final manifest commit and
//! never sees the data fragments, which are written earlier.
//!
//! **Decorate Lance's store rather than inject our own.** Lance does support dependency injection
//! (`DatasetBuilder::with_object_store`, now deprecated, or an `ObjectStoreProvider`), but both
//! make *us* construct the HF store — reproducing Lance's OpenDAL-hf setup, XET wiring, and
//! token/revision plumbing, and keeping it in lockstep. [`WrappingObjectStore`] instead hands us
//! the store Lance already built (`wrap`'s `original`), so we decorate it and never reconstruct
//! anything. It is also the non-deprecated seam.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use arrow_array::{RecordBatch, RecordBatchIterator};
use arrow_schema::SchemaRef;
use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use hf_hub::progress::{Progress, ProgressEvent, ProgressHandler, UploadEvent};
use hf_hub::repository::files::RepoTreeEntry;
use hf_hub::repository::{CommitInfo, CommitOperation, GitRefs};
use hf_hub::{HFClient, HFError, HFRepository, RepoTypeDataset};
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::{Dataset, NewColumnTransform};
use lance::index::DatasetIndexExt;
use lance_index::optimize::OptimizeOptions;
use lance_io::object_store::WrappingObjectStore;
use object_store::ObjectStore as OSObjectStore;

use crate::capture_store::{CaptureStore, Captured};
use crate::fetch_store::{FetchStore, FileFetcher};

/// Outcome of an [`append`] commit.
pub(crate) enum Appended {
    /// The data was committed; carries the new commit oid and the resulting unindexed-row backlog.
    Committed { oid: String, unindexed: u64 },
    /// The branch head moved before our commit; the caller may retry against the new head.
    Conflict,
}

/// Outcome of a [`reindex`] commit.
pub(crate) enum Reindexed {
    /// The index delta was committed; carries the new commit oid.
    Committed(String),
    /// Nothing to optimize — the index was already current.
    AlreadyCurrent,
    /// The branch head moved before our commit; the caller may retry against the new head.
    Conflict,
}

/// Append `batches` to the remote Lance dataset at `dataset_uri` (an `hf://…/<table>.lance` URI)
/// and land them in one `create_commit` on branch `rev`, guarded by the current head. The append
/// writes only data — a new fragment, manifest, and transaction — and leaves the new rows
/// unindexed (refresh the index separately with [`reindex`]). `extra_files` (repo path → bytes,
/// e.g. the dataset card) ride the same guarded commit; cloned per attempt, so a conflict retry
/// re-attaches them. Returns [`Appended::Committed`] with the new commit oid and the resulting
/// unindexed-row backlog (the largest across the dataset's indexes — what `push` thresholds on),
/// or [`Appended::Conflict`] if the head moved first — a single attempt against the head it read,
/// so the caller drives the retry.
#[allow(clippy::too_many_arguments)] // internal orchestration, one call site (`push`)
pub(crate) async fn append(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    storage_options: HashMap<String, String>,
    rev: &str,
    message: String,
    batches: Vec<RecordBatch>,
    schema: SchemaRef,
    extra_files: &BTreeMap<String, Bytes>,
) -> Result<Appended> {
    let parent = head_oid(repo, rev).await?;
    let (mut ds, wrapper) = open_capturing(dataset_uri, storage_options).await?;

    let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema);
    ds.append(reader, None)
        .await
        .context("appending to the remote dataset")?;

    // Snapshot the captured writes before reading index stats: `index_statistics` can write a stats
    // migration through the same wrapper, and that must not leak into the data commit.
    let mut files = captured_files(&wrapper);
    let unindexed = max_unindexed_rows(&ds).await;
    for (path, body) in extra_files {
        files.insert(path.clone(), body.clone());
    }

    let (ops, _dir) = write_ops(&files)?;
    match send_commit(repo, ops, parent, rev, message).await {
        Ok(info) => Ok(Appended::Committed {
            oid: info.commit_oid.unwrap_or_else(|| "?".to_string()),
            unindexed,
        }),
        Err(e) if head_moved(&e) => Ok(Appended::Conflict),
        Err(e) => Err(anyhow::Error::new(e).context("data commit failed")),
    }
}

/// Fold an index's delta sub-indexes back into one once this many pile up. Queries fan out across
/// every delta (and per-segment BM25 stats drift), so the pile must stay bounded. Only the deltas
/// are merged — the base is never re-read, which would be the full-index rewrite [`reindex`]
/// exists to avoid.
const COMPACT_DELTAS: usize = 8;

/// Refresh the remote dataset's indexes and land the delta in one `create_commit` on branch `rev`,
/// guarded by the current head. The backlog is appended as a delta sub-index — merging it into the
/// existing index would re-read the whole index over the network — until [`COMPACT_DELTAS`] pile
/// up and the deltas are folded back into one. [`Reindexed::AlreadyCurrent`] if there was nothing
/// to optimize, [`Reindexed::Conflict`] if the head moved first (retry against the new head).
pub(crate) async fn reindex(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    storage_options: HashMap<String, String>,
    rev: &str,
    message: String,
) -> Result<Reindexed> {
    let parent = head_oid(repo, rev).await?;
    let (mut ds, wrapper) = open_capturing(dataset_uri, storage_options).await?;

    for (name, subs) in sub_index_counts(&ds).await? {
        // subs = base + deltas; merge(deltas) folds every delta into one, sparing the base.
        let deltas = subs - 1;
        let opts = if deltas >= COMPACT_DELTAS {
            eprintln!("  compacting {name} ({deltas} delta sub-indexes)…");
            OptimizeOptions::merge(deltas)
        } else {
            OptimizeOptions::append()
        };
        ds.optimize_indices(&opts.index_names(vec![name]))
            .await
            .context("optimizing the remote index")?;
    }

    let files = captured_files(&wrapper);
    if files.is_empty() {
        return Ok(Reindexed::AlreadyCurrent);
    }
    let (ops, _dir) = write_ops(&files)?;
    match send_commit(repo, ops, parent, rev, message).await {
        Ok(info) => Ok(Reindexed::Committed(info.commit_oid.unwrap_or_else(|| "?".to_string()))),
        Err(e) if head_moved(&e) => Ok(Reindexed::Conflict),
        Err(e) => Err(anyhow::Error::new(e).context("reindex commit failed")),
    }
}

/// Rename a column on the remote dataset in one head-guarded commit. `alter_columns` is
/// metadata-only — the captured writes are a new manifest and transaction, no data files — so the
/// commit is small whatever the store's size. Returns the new commit oid. A moved head is an
/// error (single attempt): a rename is an exclusive-writer operation, not something to retry
/// under concurrency.
pub async fn rename_column(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    storage_options: HashMap<String, String>,
    rev: &str,
    message: String,
    from: &str,
    to: &str,
) -> Result<String> {
    let parent = head_oid(repo, rev).await?;
    let (mut ds, wrapper) = open_capturing(dataset_uri, storage_options).await?;
    ds.alter_columns(&[lance::dataset::ColumnAlteration::new(from.into()).rename(to.into())])
        .await
        .context("renaming the remote column")?;
    let files = captured_files(&wrapper);
    anyhow::ensure!(!files.is_empty(), "the rename produced no files to commit");
    let (ops, _dir) = write_ops(&files)?;
    let info = send_commit(repo, ops, parent, rev, message)
        .await
        .map_err(|e| anyhow::Error::new(e).context("rename commit failed"))?;
    Ok(info.commit_oid.unwrap_or_else(|| "?".to_string()))
}

/// Add a column to the remote dataset via `add_columns`, landing the new column's files in one
/// head-guarded commit. `transform` produces the new column per batch (a UDF over `read_columns`).
/// Unlike [`rename_column`] this writes real per-fragment column data, but still ships as one
/// captured commit; data, vectors, and indexes are untouched. Returns the new oid. A moved head is
/// an error (single attempt — a backfill runs with no concurrent writers).
pub async fn add_column(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    storage_options: HashMap<String, String>,
    rev: &str,
    message: String,
    transform: NewColumnTransform,
    read_columns: Vec<String>,
) -> Result<String> {
    let parent = head_oid(repo, rev).await?;
    let (mut ds, wrapper) = open_capturing(dataset_uri, storage_options).await?;
    ds.add_columns(transform, Some(read_columns), None)
        .await
        .context("adding the remote column")?;
    let files = captured_files(&wrapper);
    ensure!(!files.is_empty(), "add_columns produced no files to commit");
    let (ops, _dir) = write_ops(&files)?;
    let info = send_commit(repo, ops, parent, rev, message)
        .await
        .map_err(|e| anyhow::Error::new(e).context("add_column commit failed"))?;
    Ok(info.commit_oid.unwrap_or_else(|| "?".to_string()))
}

/// Open the remote dataset with a [`CaptureStore`] installed, returning the wrapped dataset and the
/// wrapper that holds the shared capture map.
async fn open_capturing(
    dataset_uri: &str,
    storage_options: HashMap<String, String>,
) -> Result<(Dataset, Arc<CaptureWrapper>)> {
    let wrapper = Arc::new(CaptureWrapper {
        captured: Captured::default(),
    });
    let ds = DatasetBuilder::from_uri(dataset_uri)
        .with_storage_options(storage_options)
        .load()
        .await
        .context("opening the remote dataset")?;
    let ds = ds.with_object_store_wrappers([wrapper.clone() as Arc<dyn WrappingObjectStore>]);
    Ok((ds, wrapper))
}

/// The captured writes as repo-path → bytes — the files Lance wrote, ready to commit.
fn captured_files(wrapper: &CaptureWrapper) -> BTreeMap<String, Bytes> {
    wrapper
        .captured
        .lock()
        .unwrap()
        .iter()
        .map(|(p, b)| (p.to_string(), b.clone()))
        .collect()
}

/// The largest `num_unindexed_rows` across the dataset's indexes — how many rows aren't yet folded
/// into an index (and so are answered by a brute-force scan at query time). 0 when there are no
/// indexes. Best-effort: a stats read that errors is skipped rather than failing the caller.
pub(crate) async fn max_unindexed_rows(ds: &Dataset) -> u64 {
    let Ok(indices) = ds.load_indices().await else {
        return 0;
    };
    let mut max = 0u64;
    for idx in indices.iter() {
        if let Ok(json) = ds.index_statistics(&idx.name).await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(n) = v.get("num_unindexed_rows").and_then(|x| x.as_u64()) {
                    max = max.max(n);
                }
            }
        }
    }
    max
}

/// Sub-index count per index name (the base plus its deltas, which share the index's name), from
/// the index metadata — not `index_statistics`, which can write a stats migration through the
/// capture wrapper.
async fn sub_index_counts(ds: &Dataset) -> Result<Vec<(String, usize)>> {
    let indices = ds.load_indices().await.context("listing the remote indexes")?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for idx in indices.iter() {
        *counts.entry(idx.name.clone()).or_default() += 1;
    }
    Ok(counts.into_iter().collect())
}

/// Read the commit at the tip of branch `rev` — the parent-commit guard for the next commit.
async fn head_oid(repo: &HFRepository<RepoTypeDataset>, rev: &str) -> Result<String> {
    let refs = repo.list_refs().send().await.context("listing remote refs")?;
    refs.branches
        .iter()
        .find(|b| b.name == rev)
        .map(|b| b.target_commit.clone())
        .context("target branch not found on the remote")
}

/// Write captured files (path → bytes) to a scratch dir and turn them into add-file commit
/// operations — hf-hub uploads from local paths. The returned `TempDir` must outlive the commit.
fn write_ops(files: &BTreeMap<String, Bytes>) -> Result<(Vec<CommitOperation>, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let mut ops = Vec::with_capacity(files.len());
    for (i, (repo_path, body)) in files.iter().enumerate() {
        let local = dir.path().join(format!("f{i}"));
        std::fs::write(&local, body)?;
        ops.push(CommitOperation::add_file(repo_path.clone(), local));
    }
    Ok((ops, dir))
}

/// The repo's `README.md` at `rev`, or `None` when it has none — fetched straight to bytes,
/// never the shared cache, so a push always classifies the dataset card against the branch
/// head it targets.
pub(crate) async fn fetch_readme(repo: &HFRepository<RepoTypeDataset>, rev: &str) -> Result<Option<String>> {
    let fetched = repo
        .download_file_to_bytes()
        .filename("README.md")
        .revision(rev.to_string())
        .send()
        .await;
    match fetched {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(HFError::EntryNotFound { .. }) => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("reading the remote dataset card")),
    }
}

/// One `create_commit` of `ops` on branch `rev`, guarded by `parent`. Returns the raw hf-hub
/// result so callers can tell a head-moved [`HFError::Conflict`] from other failures.
async fn send_commit(
    repo: &HFRepository<RepoTypeDataset>,
    ops: Vec<CommitOperation>,
    parent: String,
    rev: &str,
    message: String,
) -> std::result::Result<CommitInfo, HFError> {
    repo.create_commit()
        .operations(ops)
        .commit_message(message)
        .parent_commit(parent)
        .revision(rev.to_string())
        .progress(upload_progress())
        .send()
        .await
}

/// Whether a [`send_commit`] failure is the Hub rejecting a stale `parent_commit`: the commit API
/// answers a moved head with 412 Precondition Failed, which hf-hub leaves as a generic
/// [`HFError::Http`] (only 409 is typed as [`HFError::Conflict`]).
fn head_moved(e: &HFError) -> bool {
    match e {
        HFError::Conflict { .. } => true,
        HFError::Http { context } => context.status.as_u16() == 412,
        _ => false,
    }
}

/// A live stderr byte-bar for an upload `create_commit`, redrawn in place (`\r`) as xet streams the
/// data. Small commits skip the byte phase (no `Progress` events) — then nothing is drawn and the
/// caller's "uploading…" line is the only trace. `Send + Sync`: hf-hub calls it off the main thread.
struct UploadBar;

impl ProgressHandler for UploadBar {
    fn on_progress(&self, event: &ProgressEvent) {
        let ProgressEvent::Upload(e) = event else {
            return;
        };
        match e {
            UploadEvent::Progress {
                bytes_completed,
                total_bytes,
                bytes_per_sec,
                ..
            } => {
                let pct = if *total_bytes > 0 {
                    100.0 * *bytes_completed as f64 / *total_bytes as f64
                } else {
                    0.0
                };
                let rate = bytes_per_sec
                    .map(|r| format!(" ({}/s)", human_bytes(r as u64)))
                    .unwrap_or_default();
                eprint!(
                    "\r    uploaded {} / {}  {pct:.0}%{rate}   ",
                    human_bytes(*bytes_completed),
                    human_bytes(*total_bytes),
                );
                let _ = std::io::stderr().flush();
            }
            UploadEvent::Committing => {
                eprint!("\r    committing…                                        ");
                let _ = std::io::stderr().flush();
            }
            UploadEvent::Complete => {
                eprintln!("\r    upload complete                                     ");
            }
            UploadEvent::Start { .. } => {}
        }
    }
}

/// The upload progress handler for `create_commit`, shared by [`send_commit`] and the first-publish
/// commit in [`crate::push`]. See [`UploadBar`].
pub(crate) fn upload_progress() -> Progress {
    Progress::new(UploadBar)
}

/// Human-readable byte count (binary units), for the upload bar.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Installs a [`CaptureStore`] in front of the store Lance built for the dataset URI, and holds the
/// shared capture map so the operation that created it can read the files back once Lance is done.
#[derive(Debug)]
struct CaptureWrapper {
    captured: Captured,
}

impl WrappingObjectStore for CaptureWrapper {
    fn wrap(&self, _prefix: &str, original: Arc<dyn OSObjectStore>) -> Arc<dyn OSObjectStore> {
        Arc::new(CaptureStore::new(original, self.captured.clone()))
    }
}

/// Fetches a repo file whole at the pinned revision, returning its path in hf-hub's local cache.
/// Pinning to a commit SHA (not a branch) is what makes a warm read zero-network — hf-hub serves the
/// cached blob without a request — and fixes every read at one immutable revision.
#[derive(Debug)]
struct HubFetcher {
    repo: Arc<HFRepository<RepoTypeDataset>>,
    revision: String,
}

#[async_trait]
impl FileFetcher for HubFetcher {
    async fn fetch(&self, filename: &str) -> Result<PathBuf> {
        self.repo
            .download_file()
            .filename(filename)
            .revision(self.revision.clone())
            .send()
            .await
            .with_context(|| format!("caching {filename}@{}", self.revision))
    }
}

/// Installs a [`FetchStore`] backed by a [`HubFetcher`] in front of the store Lance built for a
/// remote read. The read mirror of [`CaptureWrapper`]; built by the caller, where the repo handle
/// and head SHA are known, because `wrap` is handed only the built store and an opendal-internal
/// prefix.
#[derive(Debug)]
pub(crate) struct FetchWrapper {
    fetcher: Arc<dyn FileFetcher>,
}

impl FetchWrapper {
    /// A wrapper that serves reads from `repo` at the pinned `revision` (a commit SHA).
    pub(crate) fn new(repo: Arc<HFRepository<RepoTypeDataset>>, revision: String) -> Self {
        Self {
            fetcher: Arc::new(HubFetcher { repo, revision }),
        }
    }
}

impl WrappingObjectStore for FetchWrapper {
    fn wrap(&self, _prefix: &str, original: Arc<dyn OSObjectStore>) -> Arc<dyn OSObjectStore> {
        Arc::new(FetchStore::new(original, self.fetcher.clone()))
    }
}

/// Resolve the head commit of `branch` for `owner/name` and build a [`FetchWrapper`] pinned to it,
/// returning the wrapper and that SHA. The SHA is the read pin: the caller puts it in the dataset's
/// `hf_revision` so Lance reads the exact commit the wrapper serves, and a commit SHA is what makes
/// warm reads zero-network. A fresh repo handle is built from `token` and shared into the wrapper.
pub(crate) async fn fetch_wrapper(
    owner: &str,
    name: &str,
    token: Option<&str>,
    branch: &str,
) -> Result<(Arc<FetchWrapper>, String)> {
    let mut builder = HFClient::builder();
    if let Some(t) = token {
        builder = builder.token(t.to_string());
    }
    let client = builder.build().context("building hf-hub client")?;
    let repo = Arc::new(client.dataset(owner, name));
    let sha = head_oid(&repo, branch).await?;
    Ok((Arc::new(FetchWrapper::new(repo, sha.clone())), sha))
}

/// A resolved Hub trace dataset's parquet: the `refs/convert/parquet` commit and its `*.parquet`
/// shard paths, plus a repo handle to download them. Built by [`resolve_parquet`].
pub(crate) struct RemoteParquet {
    /// The `refs/convert/parquet` commit SHA — pins every shard read, and is the incremental
    /// signature (an unchanged repo skips without re-downloading).
    pub(crate) revision: String,
    pub(crate) shards: Vec<String>,
    pub(crate) repo: Arc<HFRepository<RepoTypeDataset>>,
}

/// The `refs/convert/parquet` commit SHA from a repo's refs, if that auto-converted branch exists
/// (agent-traces datasets have it; a plain dataset does not).
pub(crate) fn pick_convert_oid(refs: &GitRefs) -> Option<String> {
    refs.converts
        .iter()
        .find(|r| r.name == "parquet" || r.git_ref.ends_with("convert/parquet"))
        .map(|r| r.target_commit.clone())
}

/// The `*.parquet` paths among tree `entries`, sorted for a deterministic shard order.
pub(crate) fn parquet_paths(entries: &[RepoTreeEntry]) -> Vec<String> {
    let mut paths: Vec<String> = entries
        .iter()
        .filter_map(|e| match e {
            RepoTreeEntry::File { path, .. } if path.ends_with(".parquet") => Some(path.clone()),
            _ => None,
        })
        .collect();
    paths.sort();
    paths
}

/// Resolve a Hub trace dataset's auto-converted parquet: the `refs/convert/parquet` commit and its
/// `*.parquet` shards, via the public `list_refs` + `list_tree` API (no datasets-server HTTP dep).
pub(crate) async fn resolve_parquet(owner: &str, name: &str, token: Option<&str>) -> Result<RemoteParquet> {
    let mut builder = HFClient::builder();
    if let Some(t) = token {
        builder = builder.token(t.to_string());
    }
    let client = builder.build().context("building hf-hub client")?;
    let repo = Arc::new(client.dataset(owner, name));

    let refs = repo.list_refs().send().await.context("listing dataset refs")?;
    let revision = pick_convert_oid(&refs)
        .with_context(|| format!("{owner}/{name} has no refs/convert/parquet branch (not an agent-traces dataset?)"))?;

    // Scope the stream: it borrows `repo` (`impl Stream + '_`), so it must drop before `repo` moves
    // into the returned struct.
    let shards = {
        let stream = repo
            .list_tree()
            .revision(revision.clone())
            .recursive(true)
            .send()
            .context("listing the parquet branch")?;
        futures::pin_mut!(stream);
        let mut entries = Vec::new();
        while let Some(entry) = stream.try_next().await.context("reading a tree entry")? {
            entries.push(entry);
        }
        parquet_paths(&entries)
    };
    ensure!(
        !shards.is_empty(),
        "{owner}/{name}: no *.parquet on refs/convert/parquet"
    );

    Ok(RemoteParquet { revision, shards, repo })
}

/// Download one shard whole-file at the pinned `revision` into hf-hub's cache; returns its local
/// path. Mirrors [`HubFetcher::fetch`] — a warm re-download is zero-network.
pub(crate) async fn download_shard(
    repo: &HFRepository<RepoTypeDataset>,
    filename: &str,
    revision: &str,
) -> Result<PathBuf> {
    repo.download_file()
        .filename(filename.to_string())
        .revision(revision.to_string())
        .send()
        .await
        .with_context(|| format!("downloading {filename}@{revision}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::StringArray;
    use arrow_schema::{DataType, Field, Schema};
    use lance_index::scalar::InvertedIndexParams;
    use lance_index::IndexType;

    /// Pins the Lance behavior [`reindex`] relies on: `append()` adds one delta sub-index per
    /// backlog, and `merge(deltas)` folds the deltas back into one without touching the base.
    #[tokio::test]
    async fn append_optimize_stacks_deltas_and_merge_spares_the_base() {
        let batch = |texts: &[&str]| {
            let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
            let rows = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(texts.to_vec()))]);
            RecordBatchIterator::new([rows], schema)
        };
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().join("t.lance");
        let mut ds = Dataset::write(batch(&["alpha bravo"]), uri.to_str().unwrap(), None)
            .await
            .unwrap();
        ds.create_index(
            &["text"],
            IndexType::Inverted,
            None,
            &InvertedIndexParams::default(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(sub_index_counts(&ds).await.unwrap(), vec![("text_idx".to_string(), 1)]);
        let base_uuid = ds.load_indices().await.unwrap()[0].uuid;

        for i in 0..3 {
            ds.append(batch(&[&format!("charlie delta {i}")]), None).await.unwrap();
            ds.optimize_indices(&OptimizeOptions::append()).await.unwrap();
        }
        assert_eq!(sub_index_counts(&ds).await.unwrap(), vec![("text_idx".to_string(), 4)]);

        ds.optimize_indices(&OptimizeOptions::merge(3)).await.unwrap();
        assert_eq!(sub_index_counts(&ds).await.unwrap(), vec![("text_idx".to_string(), 2)]);
        let after = ds.load_indices().await.unwrap();
        assert!(
            after.iter().any(|i| i.uuid == base_uuid),
            "the base index must survive untouched"
        );
    }

    #[test]
    fn pick_convert_oid_finds_the_parquet_convert() {
        let refs: GitRefs = serde_json::from_value(serde_json::json!({
            "branches": [{"name": "main", "ref": "refs/heads/main", "targetCommit": "main-oid"}],
            "tags": [],
            "converts": [{"name": "parquet", "ref": "refs/convert/parquet", "targetCommit": "conv-oid"}],
        }))
        .unwrap();
        assert_eq!(pick_convert_oid(&refs).as_deref(), Some("conv-oid"));

        let no_convert: GitRefs = serde_json::from_value(serde_json::json!({"branches": [], "tags": []})).unwrap();
        assert!(pick_convert_oid(&no_convert).is_none());
    }

    #[test]
    fn parquet_paths_keeps_only_parquet_files_sorted() {
        let file = |p: &str| RepoTreeEntry::File {
            oid: "o".into(),
            size: 0,
            path: p.into(),
            lfs: None,
            last_commit: None,
            xet_hash: None,
            security: None,
        };
        let entries = vec![
            file("default/train/0001.parquet"),
            RepoTreeEntry::Directory {
                oid: "o".into(),
                path: "default".into(),
                last_commit: None,
            },
            file(".gitattributes"),
            file("default/train/0000.parquet"),
        ];
        assert_eq!(
            parquet_paths(&entries),
            vec![
                "default/train/0000.parquet".to_string(),
                "default/train/0001.parquet".to_string()
            ]
        );
    }

    #[test]
    fn human_bytes_scales_to_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
