//! Capture-commit publishing.
//!
//! Lance never speaks to the HF Hub directly. A Lance `Dataset` does all of its IO through the
//! `object_store` trait, and for an `hf://` URI that store is backed by OpenDAL's HuggingFace
//! service: a `get` is a range read, and every `put` uploads the file over XET and finalizes it
//! as its own git commit.
//!
//! ```text
//!   Lance Dataset → object_store → OpenDAL hf service → HF Hub
//!       put = XET upload + one git commit, per file
//! ```
//!
//! Letting Lance's `put`s through would make several separate commits — not atomic, and with no
//! compare-and-swap against the branch head. So both entry points here slip a
//! [`WrappingObjectStore`] between Lance and OpenDAL: writes are captured in memory and never
//! forwarded, reads pass through.
//!
//! ```text
//!   Lance Dataset → CaptureStore:
//!       put → captured in memory             (never reaches the Hub)
//!       get → OpenDAL hf service → HF Hub
//! ```
//!
//! What comes back is exactly the files Lance wrote, keyed by the repo paths it wrote them to, so
//! `sync` uploads them in a single `create_commit` with a parent-commit guard.
//!
//! Two operations, kept separate so each commits on its own:
//! - [`capture_append`] runs a native append — a new data fragment, a new manifest, a transaction.
//!   The appended rows are left *unindexed*; a query still finds them, by brute force over the
//!   delta. It also reports how many rows are now unindexed, so `sync` can decide when to reindex.
//! - [`capture_reindex`] runs `optimize_indices` — folding the unindexed rows into the FTS/IVF
//!   indexes (new `_indices/*`, a manifest, a transaction). Bigger and not urgent, so `sync` runs
//!   it as its own commit, only past a threshold or when forced.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use arrow_array::{RecordBatch, RecordBatchIterator};
use arrow_schema::SchemaRef;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::stream::{self, BoxStream, StreamExt};
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::Dataset;
use lance::index::DatasetIndexExt;
use lance_index::optimize::OptimizeOptions;
use lance_io::object_store::WrappingObjectStore;
use object_store::path::Path as OPath;
use object_store::{
    Attributes, CopyOptions, GetOptions, GetRange, GetResult, GetResultPayload, ListResult, MultipartUpload,
    ObjectMeta, ObjectStore as OSObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    Result as OSResult, UploadPart,
};

/// In-memory capture of object-store writes: repo path → bytes.
type Captured = Arc<Mutex<BTreeMap<OPath, Bytes>>>;

/// Append `batches` to the remote Lance dataset at `dataset_uri` (an `hf://…/<table>.lance` URI),
/// capturing the files Lance writes without committing them. `storage_options` carries the hf
/// token and revision. The append writes only data — a new immutable fragment, a version-numbered
/// manifest, a transaction — and leaves the new rows unindexed (refresh the index separately with
/// [`capture_reindex`]). Returns `(repo_path → bytes, num_unindexed_rows)`: every captured entry is
/// a new file, so the caller can add them all in one commit without clobbering anything, and the
/// count is the largest unindexed-row backlog across the dataset's indexes — what `sync` thresholds
/// on to decide whether a reindex is due.
pub async fn capture_append(
    dataset_uri: &str,
    storage_options: HashMap<String, String>,
    batches: Vec<RecordBatch>,
    schema: SchemaRef,
) -> Result<(BTreeMap<String, Bytes>, u64)> {
    let wrapper = Arc::new(CaptureWrapper {
        captured: Captured::default(),
    });
    let ds = DatasetBuilder::from_uri(dataset_uri)
        .with_storage_options(storage_options)
        .load()
        .await
        .context("opening the remote dataset for append")?;
    let mut ds = ds.with_object_store_wrappers([wrapper.clone() as Arc<dyn WrappingObjectStore>]);

    let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema);
    ds.append(reader, None)
        .await
        .context("appending to the remote dataset")?;

    // Snapshot the append's writes (fragment, manifest, transaction) before reading index stats:
    // `index_statistics` can write a stats migration through the same wrapper, and that must not
    // leak into the data commit.
    let data_files: BTreeMap<String, Bytes> = {
        let captured = wrapper.captured.lock().unwrap();
        captured.iter().map(|(p, b)| (p.to_string(), b.clone())).collect()
    };
    let unindexed = max_unindexed_rows(&ds).await;
    Ok((data_files, unindexed))
}

/// Refresh the remote dataset's indexes (`optimize_indices`) and capture the files Lance writes —
/// new `_indices/*`, a manifest, a transaction — without committing. No data is appended. Returns
/// `repo_path → bytes`, empty when the indexes are already current.
pub async fn capture_reindex(
    dataset_uri: &str,
    storage_options: HashMap<String, String>,
) -> Result<BTreeMap<String, Bytes>> {
    let wrapper = Arc::new(CaptureWrapper {
        captured: Captured::default(),
    });
    let ds = DatasetBuilder::from_uri(dataset_uri)
        .with_storage_options(storage_options)
        .load()
        .await
        .context("opening the remote dataset for reindex")?;
    let mut ds = ds.with_object_store_wrappers([wrapper.clone() as Arc<dyn WrappingObjectStore>]);

    ds.optimize_indices(&OptimizeOptions::default())
        .await
        .context("optimizing the remote index")?;

    let captured = wrapper.captured.lock().unwrap();
    Ok(captured.iter().map(|(p, b)| (p.to_string(), b.clone())).collect())
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

/// Wraps the remote dataset's object store so writes are captured, not forwarded.
#[derive(Debug)]
struct CaptureWrapper {
    captured: Captured,
}

impl WrappingObjectStore for CaptureWrapper {
    fn wrap(&self, _prefix: &str, original: Arc<dyn OSObjectStore>) -> Arc<dyn OSObjectStore> {
        Arc::new(CaptureStore {
            inner: original,
            captured: self.captured.clone(),
        })
    }
}

/// Reads delegate to `inner` (the hf store) unless already captured (read-your-writes, needed so
/// Lance's commit can read back the manifest it just wrote); writes are captured and never
/// forwarded, so OpenDAL never issues a per-file commit.
#[derive(Debug)]
struct CaptureStore {
    inner: Arc<dyn OSObjectStore>,
    captured: Captured,
}

impl std::fmt::Display for CaptureStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CaptureStore({})", self.inner)
    }
}

#[async_trait]
impl OSObjectStore for CaptureStore {
    async fn put_opts(&self, location: &OPath, payload: PutPayload, _opts: PutOptions) -> OSResult<PutResult> {
        let mut buf = Vec::new();
        for b in payload {
            buf.extend_from_slice(&b);
        }
        self.captured.lock().unwrap().insert(location.clone(), Bytes::from(buf));
        Ok(PutResult {
            e_tag: None,
            version: None,
        })
    }

    async fn put_multipart_opts(
        &self,
        location: &OPath,
        _opts: PutMultipartOptions,
    ) -> OSResult<Box<dyn MultipartUpload>> {
        Ok(Box::new(CaptureMultipart {
            location: location.clone(),
            buf: Vec::new(),
            captured: self.captured.clone(),
        }))
    }

    async fn get_opts(&self, location: &OPath, options: GetOptions) -> OSResult<GetResult> {
        let hit = self.captured.lock().unwrap().get(location).cloned();
        match hit {
            Some(full) => {
                let total = full.len() as u64;
                let range = match &options.range {
                    None => 0..total,
                    Some(GetRange::Bounded(r)) => r.start..r.end.min(total),
                    Some(GetRange::Offset(o)) => (*o).min(total)..total,
                    Some(GetRange::Suffix(n)) => total.saturating_sub(*n)..total,
                };
                let body = full.slice(range.start as usize..range.end as usize);
                Ok(GetResult {
                    payload: GetResultPayload::Stream(stream::once(async move { Ok(body) }).boxed()),
                    meta: meta(location.clone(), total),
                    range,
                    attributes: Attributes::default(),
                })
            }
            None => self.inner.get_opts(location, options).await,
        }
    }

    fn list(&self, prefix: Option<&OPath>) -> BoxStream<'static, OSResult<ObjectMeta>> {
        let inner = self.inner.list(prefix);
        let prefix = prefix.cloned();
        let extra: Vec<OSResult<ObjectMeta>> = self
            .captured
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| prefix.as_ref().is_none_or(|pre| p.as_ref().starts_with(pre.as_ref())))
            .map(|(p, b)| Ok(meta(p.clone(), b.len() as u64)))
            .collect();
        inner.chain(stream::iter(extra)).boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&OPath>) -> OSResult<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    fn delete_stream(&self, locations: BoxStream<'static, OSResult<OPath>>) -> BoxStream<'static, OSResult<OPath>> {
        let captured = self.captured.clone();
        locations
            .map(move |loc| {
                if let Ok(p) = &loc {
                    captured.lock().unwrap().remove(p);
                }
                loc
            })
            .boxed()
    }

    async fn copy_opts(&self, from: &OPath, to: &OPath, _opts: CopyOptions) -> OSResult<()> {
        // The wrapper never writes to the underlying (hf) store — a copy lands in the capture.
        // The source comes from the capture if present, else a read of the underlying store.
        let hit = self.captured.lock().unwrap().get(from).cloned();
        let body = match hit {
            Some(b) => b,
            None => self.inner.get_opts(from, GetOptions::default()).await?.bytes().await?,
        };
        self.captured.lock().unwrap().insert(to.clone(), body);
        Ok(())
    }
}

fn meta(location: OPath, size: u64) -> ObjectMeta {
    ObjectMeta {
        location,
        last_modified: Utc::now(),
        size,
        e_tag: None,
        version: None,
    }
}

/// A captured multipart upload: parts are buffered and stored under `location` on `complete`.
struct CaptureMultipart {
    location: OPath,
    buf: Vec<u8>,
    captured: Captured,
}

#[async_trait]
impl MultipartUpload for CaptureMultipart {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        for b in data {
            self.buf.extend_from_slice(&b);
        }
        Box::pin(async { Ok(()) })
    }

    async fn complete(&mut self) -> OSResult<PutResult> {
        let bytes = Bytes::from(std::mem::take(&mut self.buf));
        self.captured.lock().unwrap().insert(self.location.clone(), bytes);
        Ok(PutResult {
            e_tag: None,
            version: None,
        })
    }

    async fn abort(&mut self) -> OSResult<()> {
        Ok(())
    }
}

impl std::fmt::Debug for CaptureMultipart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CaptureMultipart({})", self.location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn capture_over_memory() -> (CaptureStore, Arc<InMemory>) {
        let inner = Arc::new(InMemory::new());
        let store = CaptureStore {
            inner: inner.clone(),
            captured: Captured::default(),
        };
        (store, inner)
    }

    /// The load-bearing invariant: a write is captured and never reaches the underlying (hf)
    /// store — that is what stops OpenDAL from issuing a per-file commit.
    #[tokio::test]
    async fn put_is_captured_not_forwarded() {
        let (store, inner) = capture_over_memory();
        let p = OPath::from("data/x.lance");
        store
            .put_opts(&p, PutPayload::from("frag"), PutOptions::default())
            .await
            .unwrap();
        assert!(
            store.captured.lock().unwrap().contains_key(&p),
            "write must be captured"
        );
        assert!(
            inner.get_opts(&p, GetOptions::default()).await.is_err(),
            "write must NOT reach the underlying store"
        );
    }

    /// Reads serve the capture first (so Lance's commit can read back the manifest it just wrote),
    /// then fall through to the underlying store.
    #[tokio::test]
    async fn reads_capture_first_then_delegates() {
        let (store, inner) = capture_over_memory();
        let m = OPath::from("_versions/2.manifest");
        store
            .put_opts(&m, PutPayload::from("new"), PutOptions::default())
            .await
            .unwrap();
        let got = store
            .get_opts(&m, GetOptions::default())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(got, Bytes::from_static(b"new"), "read-your-writes");

        let base = OPath::from("data/base.lance");
        inner
            .put_opts(&base, PutPayload::from("base"), PutOptions::default())
            .await
            .unwrap();
        let got = store
            .get_opts(&base, GetOptions::default())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(got, Bytes::from_static(b"base"), "delegates to the underlying store");
    }

    /// `copy` must land in the capture, never in the underlying store (the fallback that used to
    /// delegate to `inner` would have triggered a stray hf write).
    #[tokio::test]
    async fn copy_lands_in_capture_never_underlying() {
        let (store, inner) = capture_over_memory();
        let from = OPath::from("data/a.lance");
        let to = OPath::from("data/b.lance");
        store
            .put_opts(&from, PutPayload::from("frag"), PutOptions::default())
            .await
            .unwrap();
        store.copy_opts(&from, &to, CopyOptions::default()).await.unwrap();
        assert!(store.captured.lock().unwrap().contains_key(&to));
        assert!(
            inner.get_opts(&to, GetOptions::default()).await.is_err(),
            "copy must not write to the underlying store"
        );
    }

    /// `list` shows both the underlying files and the captured ones, so Lance's commit sees the
    /// version it just wrote alongside the existing ones.
    #[tokio::test]
    async fn list_merges_underlying_and_captured() {
        let (store, inner) = capture_over_memory();
        inner
            .put_opts(
                &OPath::from("data/base.lance"),
                PutPayload::from("b"),
                PutOptions::default(),
            )
            .await
            .unwrap();
        store
            .put_opts(
                &OPath::from("_versions/2.manifest"),
                PutPayload::from("m"),
                PutOptions::default(),
            )
            .await
            .unwrap();
        let names: std::collections::HashSet<String> = store
            .list(None)
            .map(|r| r.unwrap().location.to_string())
            .collect()
            .await;
        assert!(names.contains("data/base.lance"), "underlying file listed");
        assert!(names.contains("_versions/2.manifest"), "captured file listed");
    }
}
