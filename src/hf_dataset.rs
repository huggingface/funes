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
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{RecordBatch, RecordBatchIterator};
use arrow_schema::SchemaRef;
use bytes::Bytes;
use hf_hub::progress::{Progress, ProgressEvent, ProgressHandler, UploadEvent};
use hf_hub::repository::{CommitInfo, CommitOperation};
use hf_hub::{HFError, HFRepository, RepoTypeDataset};
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::Dataset;
use lance::index::DatasetIndexExt;
use lance_index::optimize::OptimizeOptions;
use lance_io::object_store::WrappingObjectStore;
use object_store::ObjectStore as OSObjectStore;

use crate::capture_store::{CaptureStore, Captured};

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
/// unindexed (refresh the index separately with [`reindex`]). Returns [`Appended::Committed`] with
/// the new commit oid and the resulting unindexed-row backlog (the largest across the dataset's
/// indexes — what `push` thresholds on), or [`Appended::Conflict`] if the head moved first — a
/// single attempt against the head it read, so the caller drives the retry.
pub(crate) async fn append(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    storage_options: HashMap<String, String>,
    rev: &str,
    message: String,
    batches: Vec<RecordBatch>,
    schema: SchemaRef,
) -> Result<Appended> {
    let parent = head_oid(repo, rev).await?;
    let (mut ds, wrapper) = open_capturing(dataset_uri, storage_options).await?;

    let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema);
    ds.append(reader, None)
        .await
        .context("appending to the remote dataset")?;

    // Snapshot the captured writes before reading index stats: `index_statistics` can write a stats
    // migration through the same wrapper, and that must not leak into the data commit.
    let files = captured_files(&wrapper);
    let unindexed = max_unindexed_rows(&ds).await;

    let (ops, _dir) = write_ops(&files)?;
    match send_commit(repo, ops, parent, rev, message).await {
        Ok(info) => Ok(Appended::Committed {
            oid: info.commit_oid.unwrap_or_else(|| "?".to_string()),
            unindexed,
        }),
        Err(HFError::Conflict { .. }) => Ok(Appended::Conflict),
        Err(e) => Err(anyhow::Error::new(e).context("data commit failed")),
    }
}

/// Refresh the remote dataset's indexes (`optimize_indices`) and land the delta in one
/// `create_commit` on branch `rev`, guarded by the current head. [`Reindexed::AlreadyCurrent`] if
/// there was nothing to optimize, [`Reindexed::Conflict`] if the head moved first (retry against
/// the new head).
pub(crate) async fn reindex(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    storage_options: HashMap<String, String>,
    rev: &str,
    message: String,
) -> Result<Reindexed> {
    let parent = head_oid(repo, rev).await?;
    let (mut ds, wrapper) = open_capturing(dataset_uri, storage_options).await?;

    ds.optimize_indices(&OptimizeOptions::default())
        .await
        .context("optimizing the remote index")?;

    let files = captured_files(&wrapper);
    if files.is_empty() {
        return Ok(Reindexed::AlreadyCurrent);
    }
    let (ops, _dir) = write_ops(&files)?;
    match send_commit(repo, ops, parent, rev, message).await {
        Ok(info) => Ok(Reindexed::Committed(info.commit_oid.unwrap_or_else(|| "?".to_string()))),
        Err(HFError::Conflict { .. }) => Ok(Reindexed::Conflict),
        Err(e) => Err(anyhow::Error::new(e).context("reindex commit failed")),
    }
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
/// indexes. Best-effort: a stats read that errors is skipped rather than failing the append.
async fn max_unindexed_rows(ds: &Dataset) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::human_bytes;

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
