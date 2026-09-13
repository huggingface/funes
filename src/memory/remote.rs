//! The remote side of a memory: how its Lance dataset is read from and written to a Hub repo.
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
//! [`CaptureStore`](super::capture_store::CaptureStore) installed via Lance's
//! [`WrappingObjectStore`] seam: Lance's writes are captured in temporary files instead of hitting the Hub,
//! and we ship the whole set as one guarded `create_commit`. The Hub client streams Xet files
//! from these paths; its regular-file/non-Xet fallback still buffers inline commit content.
//!
//! # Why this shape
//!
//! **Intercept at the object-store layer.** Every file an append or optimize produces — data
//! fragment, manifest, transaction, index — is written through `object_store`, so it is the one
//! hook that captures the *whole* write set with no knowledge of Lance's on-disk layout. A
//! narrower seam can't do it: a custom `CommitHandler` only governs the final manifest commit and
//! never sees the data fragments, which are written earlier.
//!
//! **Decorate Lance's object store rather than inject our own.** Lance does support dependency injection
//! (`DatasetBuilder::with_object_store`, now deprecated, or an `ObjectStoreProvider`), but both
//! make *us* construct the HF object store — reproducing Lance's OpenDAL-hf setup, XET wiring, and
//! token/revision plumbing, and keeping it in lockstep. [`WrappingObjectStore`] instead hands us
//! the object store Lance already built (`wrap`'s `original`), so we decorate it and never reconstruct
//! anything. It is also the non-deprecated seam.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use arrow_array::RecordBatchReader;
use async_trait::async_trait;
use bytes::Bytes;
use hf_hub::progress::{Progress, ProgressEvent, ProgressHandler, UploadEvent};
use hf_hub::repository::{CommitInfo, CommitOperation};
use hf_hub::{HFError, HFRepository, RepoTypeDataset};
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::{Dataset, NewColumnTransform, WriteParams};
use lance::index::DatasetIndexExt;
use lance_index::optimize::OptimizeOptions;
use lance_io::object_store::WrappingObjectStore;
use object_store::ObjectStore as OSObjectStore;

use super::capture_store::{CaptureStore, Captured, CapturedFile};
use super::dataset;
use super::fetch_store::{FetchStore, FileFetcher};
use crate::hub;

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

/// Append `reader` to the remote Lance dataset at `dataset_uri` (an `hf://…/<table>.lance` URI)
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
    reader: impl RecordBatchReader + Send + 'static,
    extra_files: &BTreeMap<String, Bytes>,
) -> Result<Appended> {
    let parent = head_oid(repo, rev).await?;
    let (mut ds, wrapper) = open_capturing(dataset_uri, storage_options).await?;

    ds.append(reader, None)
        .await
        .context("appending to the remote dataset")?;

    // Snapshot the captured writes before reading index stats: `index_statistics` can write a stats
    // migration through the same wrapper, and that must not leak into the data commit.
    let files = captured_files(&wrapper);
    let unindexed = max_unindexed_rows(&ds).await;
    let mut ops = capture_ops(&files);
    let (extra_ops, _extra_dir) = write_ops(extra_files)?;
    ops.extend(extra_ops);
    match send_commit(repo, ops, parent, rev, message).await {
        Ok(info) => Ok(Appended::Committed {
            oid: info.commit_oid.unwrap_or_else(|| "?".to_string()),
            unindexed,
        }),
        Err(e) if head_moved(&e) => Ok(Appended::Conflict),
        Err(e) => Err(anyhow::Error::new(e).context("data commit failed")),
    }
}

/// Exercise native append and disk capture without a network commit or index construction.
#[cfg(test)]
pub(crate) async fn benchmark_append(mut reader: impl RecordBatchReader + Send + 'static) -> Result<(usize, u64)> {
    use arrow_array::RecordBatchIterator;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let schema = reader.schema();
    let (reader, first) = tokio::task::spawn_blocking(move || {
        let first = reader.next();
        (reader, first)
    })
    .await?;
    let first = first.context("benchmark input is empty")??;
    ensure!(first.num_rows() > 0, "benchmark first batch is empty");
    let seed = RecordBatchIterator::new([Ok(first.slice(0, 1))], schema.clone());
    // Native local paths bypass ObjectStore when writing data. A live memory:// base exercises
    // ObjectWriter, the same route hf:// takes, and keeps only the one-row seed in memory.
    let base = Dataset::write(seed, "memory://capture-benchmark", None).await?;
    let store = base.object_store(None).await?;
    ensure!(!store.is_local(), "capture benchmark must use ObjectWriter");
    let before = store_inventory(store.inner.as_ref()).await?;
    let seed_bytes: u64 = before.values().map(|bytes| bytes.len() as u64).sum();
    let wrapper = Arc::new(CaptureWrapper {
        captured: Captured::new()?,
    });
    let mut ds = base.with_object_store_wrappers([wrapper.clone() as Arc<dyn WrappingObjectStore>]);

    let rows = Arc::new(AtomicUsize::new(0));
    let counted = rows.clone();
    let batches = std::iter::once(Ok(first)).chain(reader).inspect(move |batch| {
        if let Ok(batch) = batch {
            counted.fetch_add(batch.num_rows(), Ordering::Relaxed);
        }
    });
    ds.append(RecordBatchIterator::new(batches, schema), None).await?;
    let files = captured_files(&wrapper);
    let appended = rows.load(Ordering::Relaxed);
    ensure!(ds.count_rows(None).await? == appended + 1, "captured row count differs");
    ensure!(
        store_inventory(store.inner.as_ref()).await? == before,
        "benchmark modified the base store"
    );
    ensure!(
        files.keys().any(|path| path.contains("/data/")),
        "capture contains no data files"
    );
    let bytes = files.values().try_fold(0u64, |total, file| {
        std::fs::metadata(&file.path).map(|meta| total + meta.len())
    })?;
    ensure!(bytes > seed_bytes, "capture is smaller than its one-row seed");
    Ok((appended, bytes))
}

/// Exact inventory of the tiny seed backend, detecting stray data writes as well as commits.
#[cfg(test)]
async fn store_inventory(store: &dyn OSObjectStore) -> Result<BTreeMap<String, Bytes>> {
    use futures::TryStreamExt;

    let mut listed = store.list(None);
    let mut files = BTreeMap::new();
    while let Some(meta) = listed.try_next().await? {
        let bytes = store
            .get_opts(&meta.location, Default::default())
            .await?
            .bytes()
            .await?;
        files.insert(meta.location.to_string(), bytes);
    }
    Ok(files)
}

/// Build the whole dataset locally (data + indexes) and upload it in one `create_commit` — unlike
/// [`append`]/[`reindex`], no head to guard against, since the dataset doesn't exist yet. `None` if
/// the build produced no files.
#[allow(clippy::too_many_arguments)] // internal orchestration, one call site (`push`)
pub(crate) async fn first_publish(
    repo: &HFRepository<RepoTypeDataset>,
    prefix: &str,
    reader: impl RecordBatchReader + Send + 'static,
    rev: &str,
    message: String,
    extra_files: &BTreeMap<String, Bytes>,
    on_phase: impl Fn(&str),
) -> Result<Option<String>> {
    let staging = tempfile::tempdir()?;
    // Empty prefix = dataset at the repo root; joining "" would leave a stray trailing separator.
    let db_dir = if prefix.is_empty() {
        staging.path().to_path_buf()
    } else {
        staging.path().join(prefix)
    };
    std::fs::create_dir_all(&db_dir)?;
    let table_uri = dataset::table_uri(&db_dir.to_string_lossy());
    let mut ds = Dataset::write(reader, &table_uri, Some(WriteParams::default()))
        .await
        .context("building the dataset for first publish")?;
    dataset::build_indexes(&mut ds, on_phase).await;

    let mut ops = Vec::new();
    for entry in walkdir::WalkDir::new(&db_dir) {
        let entry = entry.context("enumerating files for first publish")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(staging.path()).unwrap_or(entry.path());
        ops.push(CommitOperation::add_file(
            rel.to_string_lossy().into_owned(),
            entry.path().to_path_buf(),
        ));
    }
    if ops.is_empty() {
        return Ok(None);
    }
    let (extra_ops, _extra_dir) = write_ops(extra_files)?;
    ops.extend(extra_ops);

    let info = repo
        .create_commit()
        .operations(ops)
        .commit_message(message)
        .revision(rev.to_string())
        .progress(upload_progress())
        .send()
        .await
        .map_err(|e| anyhow::Error::new(e).context("create_commit failed"))?;
    Ok(Some(info.commit_oid.unwrap_or_else(|| "?".to_string())))
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
    let ops = capture_ops(&files);
    match send_commit(repo, ops, parent, rev, message).await {
        Ok(info) => Ok(Reindexed::Committed(info.commit_oid.unwrap_or_else(|| "?".to_string()))),
        Err(e) if head_moved(&e) => Ok(Reindexed::Conflict),
        Err(e) => Err(anyhow::Error::new(e).context("reindex commit failed")),
    }
}

/// Add a column to the remote dataset via `add_columns`, landing the new column's files in one
/// head-guarded commit. `transform` produces the new column per batch (a UDF over `read_columns`).
/// Writes real per-fragment column data, shipped as one captured commit; data, vectors, and
/// indexes are untouched. Returns the new oid. A moved head is an error — a single guarded
/// attempt, not retried.
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
    let ops = capture_ops(&files);
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
        captured: Captured::new()?,
    });
    let ds = DatasetBuilder::from_uri(dataset_uri)
        .with_storage_options(storage_options)
        .load()
        .await
        .context("opening the remote dataset")?;
    let ds = ds.with_object_store_wrappers([wrapper.clone() as Arc<dyn WrappingObjectStore>]);
    Ok((ds, wrapper))
}

/// The captured writes as repo-path → immutable local file, retaining their backing directory.
fn captured_files(wrapper: &CaptureWrapper) -> BTreeMap<String, CapturedFile> {
    wrapper
        .captured
        .snapshot()
        .into_iter()
        .map(|(p, file)| (p.to_string(), file))
        .collect()
}

/// Upload directly from captured files. `files` must outlive the commit.
fn capture_ops(files: &BTreeMap<String, CapturedFile>) -> Vec<CommitOperation> {
    files
        .iter()
        .map(|(path, file)| CommitOperation::add_file(path.clone(), file.path.clone()))
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

/// Stage small extra files (such as the dataset card) for path-based uploads. Captured Lance
/// data already lives on disk. The returned `TempDir` must outlive the commit.
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
/// commit in [`crate::commands::push`]. See [`UploadBar`].
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

    /// A cache entry links to a blob named by the object's expected hash rather than by the bytes
    /// on disk, so dropping the entry alone would resolve to those same bytes again.
    async fn discard(&self, path: &Path) -> Result<()> {
        let blob = std::fs::read_link(path).ok().map(|target| match path.parent() {
            Some(dir) if target.is_relative() => dir.join(target),
            _ => target,
        });
        std::fs::remove_file(path).with_context(|| format!("discarding {}", path.display()))?;
        if let Some(blob) = blob {
            let _ = std::fs::remove_file(blob);
        }
        Ok(())
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
    let repo = Arc::new(hub::client(token, true)?.dataset(owner, name));
    let sha = head_oid(&repo, branch).await?;
    Ok((Arc::new(FetchWrapper::new(repo, sha.clone())), sha))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{RecordBatch, RecordBatchIterator, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use lance_index::scalar::InvertedIndexParams;
    use lance_index::IndexType;

    /// Exercises native Lance writes through the capture wrapper. A failed/conflicted commit
    /// leaves the base unchanged, and replaying the same reader produces the same logical rows.
    #[tokio::test]
    async fn captured_append_is_readable_and_replay_leaves_base_unchanged() {
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let reader = |texts: &[&str]| {
            let rows = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(texts.to_vec()))]);
            RecordBatchIterator::new([rows], schema.clone())
        };
        let base = Dataset::write(reader(&["original"]), "memory://capture-replay", None)
            .await
            .unwrap();
        let store = base.object_store(None).await.unwrap();
        assert!(!store.is_local());
        let before = store_inventory(store.inner.as_ref()).await.unwrap();
        let mut prior_snapshot = None;
        for _ in 0..2 {
            let wrapper = Arc::new(CaptureWrapper {
                captured: Captured::new().unwrap(),
            });
            let mut ds = base.with_object_store_wrappers([wrapper.clone() as Arc<dyn WrappingObjectStore>]);
            ds.append(reader(&["new", "new"]), None).await.unwrap();
            let files = captured_files(&wrapper);
            assert!(
                files.keys().any(|path| path.contains("/data/")),
                "data must be captured"
            );
            assert_eq!(
                store_inventory(store.inner.as_ref()).await.unwrap(),
                before,
                "no stray writes"
            );
            let batches = dataset::scan_rows(&ds, &["text"], None, None).await.unwrap();
            let texts: Vec<_> = batches
                .iter()
                .flat_map(|b| {
                    b.column(0)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .unwrap()
                        .iter()
                        .map(|s| s.unwrap().to_owned())
                })
                .collect();
            assert_eq!(
                texts,
                ["original", "new", "new"],
                "physical duplicate rows are preserved"
            );
            drop(ds);
            drop(wrapper);
            assert!(
                files.values().all(|file| file.path.is_file()),
                "snapshot owns its files"
            );
            if let Some(previous) = prior_snapshot.replace(files) {
                assert!(
                    previous.values().all(|file| file.path.is_file()),
                    "retry cannot replace earlier capture files"
                );
            }
            assert_eq!(store_inventory(store.inner.as_ref()).await.unwrap(), before);
        }
    }

    /// Independently measures native capture with a bounded scan of a frozen local fixture.
    /// No secret scan, receipt, index, or network operation is involved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "explicit frozen fixture and process memory guard required"]
    async fn benchmark_capture_only() {
        use arrow_schema::ArrowError;
        use futures::TryStreamExt;

        let uri = std::env::var("FUNES_BENCH_DATASET").expect("FUNES_BENCH_DATASET");
        let start = std::time::Instant::now();
        let fixture = Dataset::open(&uri).await.unwrap();
        let expected = fixture.count_rows(None).await.unwrap();
        let schema = Arc::new(Schema::from(fixture.schema()));
        let version = fixture.version_id();
        let mut scan = fixture.scan();
        scan.batch_size(8192)
            .batch_readahead(2)
            .fragment_readahead(2)
            .scan_in_order(true);
        let mut stream = scan.try_into_stream().await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let feeder = tokio::spawn(async move {
            while let Some(batch) = stream
                .try_next()
                .await
                .map_err(|error| ArrowError::ExternalError(Box::new(error)))
                .transpose()
            {
                let failed = batch.is_err();
                if tx.send(batch).await.is_err() || failed {
                    break;
                }
            }
        });
        let reader = RecordBatchIterator::new(std::iter::from_fn(move || rx.blocking_recv()), schema);
        eprintln!(
            "BENCH capture_append rows={expected} version={version} seconds={:.3}",
            start.elapsed().as_secs_f64()
        );
        let result = benchmark_append(reader).await;
        feeder.await.unwrap();
        let (rows, bytes) = result.unwrap();
        assert_eq!(rows, expected);
        assert!(
            bytes > rows as u64,
            "full capture must contain substantial data, not just a manifest"
        );
        eprintln!(
            "BENCH capture_done seconds={:.3} rows={rows} captured_bytes={bytes}",
            start.elapsed().as_secs_f64()
        );
    }

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
