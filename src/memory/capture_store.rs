//! A write-capturing object-store decorator.
//!
//! Writes land in immutable temporary files and never reach the inner store. Reads serve the
//! capture first, then delegate. Snapshots retain the temporary directory and the exact files
//! selected at snapshot time, even if a later write replaces the same logical path.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt, TryStreamExt};
use object_store::local::LocalFileSystem;
use object_store::path::Path as OPath;
use object_store::ObjectStoreExt;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore as OSObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as OSResult,
    UploadPart,
};

/// One immutable capture. Clones keep its backing directory alive through an upload or read.
#[derive(Clone, Debug)]
pub(crate) struct CapturedFile {
    pub(crate) path: PathBuf,
    physical: OPath,
    meta: ObjectMeta,
    _dir: Arc<tempfile::TempDir>,
}

#[derive(Debug)]
struct CaptureState {
    files: Mutex<BTreeMap<OPath, CapturedFile>>,
    local: LocalFileSystem,
    dir: Arc<tempfile::TempDir>,
    next: AtomicU64,
}

/// Shared disk-backed writes, with snapshots independent of subsequent logical overwrites.
#[derive(Clone, Debug)]
pub(crate) struct Captured(Arc<CaptureState>);

impl Captured {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let dir = Arc::new(tempfile::tempdir()?);
        let local = LocalFileSystem::new_with_prefix(dir.path())?;
        Ok(Self(Arc::new(CaptureState {
            files: Mutex::new(BTreeMap::new()),
            local,
            dir,
            next: AtomicU64::new(0),
        })))
    }

    pub(crate) fn snapshot(&self) -> BTreeMap<OPath, CapturedFile> {
        self.0.files.lock().unwrap().clone()
    }

    fn next_path(&self) -> OPath {
        // Physical names never derive from logical paths, which may have characters or suffixes
        // reserved by the local filesystem store. Nor can a logical overwrite reuse a file.
        OPath::from(format!("f{}", self.0.next.fetch_add(1, Ordering::Relaxed)))
    }

    async fn record(&self, location: OPath, physical: OPath) -> OSResult<()> {
        let mut meta = self.0.local.head(&physical).await?;
        meta.location = location.clone();
        let file = CapturedFile {
            path: self.0.local.path_to_filesystem(&physical)?,
            physical,
            meta,
            _dir: self.0.dir.clone(),
        };
        self.0.files.lock().unwrap().insert(location, file);
        Ok(())
    }
}

/// Reads delegate to `inner` unless captured; writes remain local until the caller commits them.
#[derive(Debug)]
pub(crate) struct CaptureStore {
    inner: Arc<dyn OSObjectStore>,
    captured: Captured,
}

impl CaptureStore {
    pub(crate) fn new(inner: Arc<dyn OSObjectStore>, captured: Captured) -> Self {
        Self { inner, captured }
    }
}

impl std::fmt::Display for CaptureStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CaptureStore({})", self.inner)
    }
}

#[async_trait]
impl OSObjectStore for CaptureStore {
    async fn put_opts(&self, location: &OPath, payload: PutPayload, _opts: PutOptions) -> OSResult<PutResult> {
        let physical = self.captured.next_path();
        let result = self.captured.0.local.put(&physical, payload).await?;
        self.captured.record(location.clone(), physical).await?;
        Ok(result)
    }

    async fn put_multipart_opts(
        &self,
        location: &OPath,
        _opts: PutMultipartOptions,
    ) -> OSResult<Box<dyn MultipartUpload>> {
        let physical = self.captured.next_path();
        let upload = self.captured.0.local.put_multipart(&physical).await?;
        Ok(Box::new(CaptureMultipart {
            location: location.clone(),
            physical,
            upload,
            captured: self.captured.clone(),
        }))
    }

    async fn get_opts(&self, location: &OPath, options: GetOptions) -> OSResult<GetResult> {
        let hit = self.captured.0.files.lock().unwrap().get(location).cloned();
        match hit {
            Some(file) => {
                let result = self.captured.0.local.get_opts(&file.physical, options).await?;
                let range = result.range.clone();
                let attributes = result.attributes.clone();
                let mut meta = result.meta.clone();
                meta.location = location.clone();
                // The returned stream owns the capture, so dropping the store before consuming
                // a read cannot remove its directory. LocalFileSystem streams bounded chunks.
                let body = result.into_stream().map(move |chunk| {
                    let _keep_alive = &file;
                    chunk
                });
                Ok(GetResult {
                    payload: GetResultPayload::Stream(body.boxed()),
                    meta,
                    range,
                    attributes,
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
            .0
            .files
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| prefix.as_ref().is_none_or(|pre| p.as_ref().starts_with(pre.as_ref())))
            .map(|(p, file)| {
                let mut meta = file.meta.clone();
                meta.location = p.clone();
                Ok(meta)
            })
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
                    captured.0.files.lock().unwrap().remove(p);
                }
                loc
            })
            .boxed()
    }

    async fn copy_opts(&self, from: &OPath, to: &OPath, _opts: CopyOptions) -> OSResult<()> {
        let hit = self.captured.0.files.lock().unwrap().get(from).cloned();
        if let Some(file) = hit {
            // Immutable files can be shared safely, including across later overwrites of `from`.
            self.captured.0.files.lock().unwrap().insert(to.clone(), file);
            return Ok(());
        }
        let mut input = self.inner.get(from).await?.into_stream();
        let mut upload = self.put_multipart(to).await?;
        let result = async {
            while let Some(chunk) = input.try_next().await? {
                upload.put_part(chunk.into()).await?;
            }
            upload.complete().await?;
            OSResult::Ok(())
        }
        .await;
        if result.is_err() {
            let _ = upload.abort().await;
        }
        result
    }
}

/// Parts go straight to a local multipart writer; only completion exposes the logical path.
#[derive(Debug)]
struct CaptureMultipart {
    location: OPath,
    physical: OPath,
    upload: Box<dyn MultipartUpload>,
    captured: Captured,
}

#[async_trait]
impl MultipartUpload for CaptureMultipart {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        self.upload.put_part(data)
    }

    async fn complete(&mut self) -> OSResult<PutResult> {
        let result = self.upload.complete().await?;
        self.captured
            .record(self.location.clone(), self.physical.clone())
            .await?;
        Ok(result)
    }

    async fn abort(&mut self) -> OSResult<()> {
        self.upload.abort().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use object_store::memory::InMemory;
    use object_store::GetRange;

    fn capture_over_memory() -> (CaptureStore, Arc<InMemory>) {
        let inner = Arc::new(InMemory::new());
        let store = CaptureStore::new(inner.clone(), Captured::new().unwrap());
        (store, inner)
    }

    /// The load-bearing invariant: a write is captured and never reaches the underlying store —
    /// that is what stops the backend from issuing a per-file commit.
    #[tokio::test]
    async fn put_is_captured_not_forwarded() {
        let (store, inner) = capture_over_memory();
        let p = OPath::from("data/x.lance");
        store
            .put_opts(&p, PutPayload::from("frag"), PutOptions::default())
            .await
            .unwrap();
        assert!(
            store.captured.0.files.lock().unwrap().contains_key(&p),
            "write must be captured"
        );
        assert!(
            inner.get_opts(&p, GetOptions::default()).await.is_err(),
            "write must NOT reach the underlying store"
        );
    }

    /// Reads serve the capture first (so a caller can read back what it just wrote), then fall
    /// through to the underlying store.
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

    /// `copy` must land in the capture, never in the underlying store (a fallback that delegated to
    /// `inner` would trigger a stray backend write).
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
        assert!(store.captured.0.files.lock().unwrap().contains_key(&to));
        assert!(
            inner.get_opts(&to, GetOptions::default()).await.is_err(),
            "copy must not write to the underlying store"
        );
    }

    /// `list` shows both the underlying files and the captured ones, so a caller sees the version
    /// it just wrote alongside the existing ones.
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

    #[tokio::test]
    async fn snapshot_survives_overwrite_delete_and_store_drop() {
        let (store, inner) = capture_over_memory();
        let p = OPath::from("_latest.manifest");
        store.put(&p, "old".into()).await.unwrap();
        let snapshot = store.captured.snapshot();
        let original = snapshot.get(&p).unwrap().path.clone();
        store.put(&p, "new".into()).await.unwrap();
        assert_eq!(std::fs::read(&original).unwrap(), b"old");
        assert_eq!(store.get(&p).await.unwrap().bytes().await.unwrap(), b"new"[..]);
        store.delete(&p).await.unwrap();
        drop(store);
        assert_eq!(std::fs::read(&original).unwrap(), b"old");
        assert!(inner.get(&p).await.is_err());
        drop(snapshot);
        assert!(!original.exists(), "last snapshot releases the scratch directory");
    }

    #[tokio::test]
    async fn multipart_is_visible_only_after_completion_and_abort_preserves_old_value() {
        let (store, inner) = capture_over_memory();
        let p = OPath::from("data/fragment.lance");
        let mut upload = store.put_multipart(&p).await.unwrap();
        let part1 = upload.put_part("first".into());
        let part2 = upload.put_part("second".into());
        // Local multipart offsets follow invocation order, even when parts finish out of order.
        part2.await.unwrap();
        part1.await.unwrap();
        assert!(store.get(&p).await.is_err());
        upload.complete().await.unwrap();
        assert_eq!(store.get(&p).await.unwrap().bytes().await.unwrap(), b"firstsecond"[..]);
        assert!(inner.get(&p).await.is_err());

        let mut aborted = store.put_multipart(&p).await.unwrap();
        aborted.put_part("replacement".into()).await.unwrap();
        aborted.abort().await.unwrap();
        assert!(aborted.complete().await.is_err());
        assert_eq!(store.get(&p).await.unwrap().bytes().await.unwrap(), b"firstsecond"[..]);
    }

    #[tokio::test]
    async fn captured_reads_preserve_ranges_and_logical_metadata() {
        let (store, _) = capture_over_memory();
        let p = OPath::from("_versions/1.manifest");
        store.put(&p, "0123456789".into()).await.unwrap();
        for (range, expected, bytes) in [
            (GetRange::Bounded(2..5), 2..5, "234"),
            (GetRange::Bounded(8..20), 8..10, "89"),
            (GetRange::Offset(6), 6..10, "6789"),
            (GetRange::Suffix(3), 7..10, "789"),
        ] {
            let result = store
                .get_opts(
                    &p,
                    GetOptions {
                        range: Some(range),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(result.meta.location, p);
            assert_eq!(result.meta.size, 10);
            assert_eq!(result.range, expected);
            assert_eq!(result.bytes().await.unwrap(), bytes.as_bytes());
        }
        let result = store.get(&p).await.unwrap();
        drop(store);
        assert_eq!(
            result.bytes().await.unwrap(),
            b"0123456789"[..],
            "read owns its backing file"
        );
    }

    #[tokio::test]
    async fn remote_copy_and_captured_alias_survive_source_overwrite() {
        let (store, inner) = capture_over_memory();
        let source = OPath::from("remote");
        let copy = OPath::from("copied");
        let alias = OPath::from("alias");
        inner.put(&source, "base".into()).await.unwrap();
        store.copy(&source, &copy).await.unwrap();
        store.copy(&copy, &alias).await.unwrap();
        store.put(&copy, "replacement".into()).await.unwrap();
        assert_eq!(store.get(&alias).await.unwrap().bytes().await.unwrap(), b"base"[..]);
        assert_eq!(store.head(&alias).await.unwrap().location, alias);
        assert_eq!(inner.get(&source).await.unwrap().bytes().await.unwrap(), b"base"[..]);
        assert!(inner.get(&copy).await.is_err());
        assert!(inner.get(&alias).await.is_err());
    }

    #[tokio::test]
    async fn disk_write_failure_exposes_no_capture() {
        let (store, inner) = capture_over_memory();
        let p = OPath::from("data/fragment.lance");
        // Make the scratch root a regular file. This fails independent of privileges, unlike
        // permission-bit tests (which can succeed as root).
        let root = store.captured.0.dir.path();
        std::fs::remove_dir(root).unwrap();
        std::fs::write(root, b"not a directory").unwrap();
        assert!(store.put(&p, "payload".into()).await.is_err());
        assert!(store.captured.snapshot().is_empty());
        assert!(inner.get(&p).await.is_err());
        std::fs::remove_file(root).unwrap();
        std::fs::create_dir(root).unwrap();
    }
}
