//! A fetch-through read object-store decorator.
//!
//! [`FetchStore`] wraps an inner [`ObjectStore`](object_store::ObjectStore): it serves every `get*`
//! from a whole local file supplied by a [`FileFetcher`] — fetched once, then sliced to the requested
//! range — and delegates every other method (listing, writes) to the inner store. It is the read
//! mirror of [`CaptureStore`](super::capture_store::CaptureStore), the write decorator.
//!
//! It owns no cache. The [`FileFetcher`] decides where the file comes from and whether it persists: a
//! fetcher backed by a disk cache turns this into a read-through cache; a fetcher that re-downloads
//! every time does not. The decorator only fetches-and-slices — the concrete fetcher is the caller's
//! to provide, so this module stays backend-agnostic.
//!
//! ```text
//!   get → fetcher.fetch(path) → whole local file → serve the requested range
//!   list / put / copy / delete → delegated to the inner store
//! ```
//!
//! **A decorator, because Rust has no inheritance** — the inner store is held and every method
//! forwarded by hand, intercepting only the reads.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::{self, BoxStream, StreamExt};
use object_store::path::Path as OPath;
use object_store::{
    Attributes, CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore as OSObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as OSResult,
};

/// Supplies a whole local file for an object key: given the object path, return a local filesystem
/// path holding its bytes, fetching it on first touch. The seam that keeps [`FetchStore`]
/// backend-agnostic and unit-testable — the caller provides the concrete fetcher.
#[async_trait]
pub(crate) trait FileFetcher: std::fmt::Debug + Send + Sync {
    /// Resolve `filename` (the object's path) to a local path holding its bytes.
    async fn fetch(&self, filename: &str) -> anyhow::Result<PathBuf>;

    /// Drop the local copy at `path`; a later [`fetch`](Self::fetch) of that key must not yield it.
    async fn discard(&self, path: &Path) -> anyhow::Result<()>;
}

/// Reads are served from the whole file `fetcher` supplies for the key; every other method delegates
/// to `inner` (also the live fallback when a fetch fails).
#[derive(Debug)]
pub(crate) struct FetchStore {
    inner: Arc<dyn OSObjectStore>,
    fetcher: Arc<dyn FileFetcher>,
}

impl FetchStore {
    /// Decorate `inner`, serving reads from files `fetcher` resolves.
    pub(crate) fn new(inner: Arc<dyn OSObjectStore>, fetcher: Arc<dyn FileFetcher>) -> Self {
        Self { inner, fetcher }
    }

    /// Serve `location` from a copy obtained after discarding `corrupt`, at most one discard per read.
    async fn repair(&self, corrupt: &Path, location: &OPath, options: GetOptions) -> OSResult<GetResult> {
        if self.fetcher.discard(corrupt).await.is_ok() {
            if let Ok(fresh) = self.fetcher.fetch(location.as_ref()).await {
                if let Ok(result) = serve_from_file(&fresh, location, &options) {
                    return Ok(result);
                }
            }
        }
        self.inner.get_opts(location, options).await
    }
}

impl std::fmt::Display for FetchStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FetchStore({})", self.inner)
    }
}

#[async_trait]
impl OSObjectStore for FetchStore {
    async fn get_opts(&self, location: &OPath, options: GetOptions) -> OSResult<GetResult> {
        // A transient fetch or read failure falls back to the inner store, so it never breaks a read.
        let Ok(path) = self.fetcher.fetch(location.as_ref()).await else {
            return self.inner.get_opts(location, options).await;
        };
        match serve_from_file(&path, location, &options) {
            Ok(result) => Ok(result),
            Err(Unservable::Corrupt) => self.repair(&path, location, options).await,
            Err(Unservable::Unreadable) => self.inner.get_opts(location, options).await,
        }
    }

    async fn put_opts(&self, location: &OPath, payload: PutPayload, opts: PutOptions) -> OSResult<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &OPath,
        opts: PutMultipartOptions,
    ) -> OSResult<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    fn list(&self, prefix: Option<&OPath>) -> BoxStream<'static, OSResult<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&OPath>) -> OSResult<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    fn delete_stream(&self, locations: BoxStream<'static, OSResult<OPath>>) -> BoxStream<'static, OSResult<OPath>> {
        self.inner.delete_stream(locations)
    }

    async fn copy_opts(&self, from: &OPath, to: &OPath, opts: CopyOptions) -> OSResult<()> {
        self.inner.copy_opts(from, to, opts).await
    }
}

/// Why a fetched file cannot answer a read.
enum Unservable {
    /// It does not span the requested range, so its bytes are not the object's.
    Corrupt,
    Unreadable,
}

/// Build a [`GetResult`] over the fetched file at `path` for the request `options`, reporting the
/// object's `location` and whole-file size. A `head` returns metadata only; a body request hands the
/// open file to `object_store`, which reads the resolved range off the executor (so a large file is
/// never buffered here).
fn serve_from_file(path: &Path, location: &OPath, options: &GetOptions) -> Result<GetResult, Unservable> {
    let total = std::fs::metadata(path).map_err(|_| Unservable::Unreadable)?.len();
    let meta = meta(location.clone(), total);
    if options.head {
        return Ok(GetResult {
            payload: GetResultPayload::Stream(stream::empty().boxed()),
            meta,
            range: 0..0,
            attributes: Attributes::default(),
        });
    }
    let range = match &options.range {
        None => 0..total,
        // Not clamped: an out-of-file range underflows where its length is computed.
        Some(requested) => requested.as_range(total).map_err(|_| Unservable::Corrupt)?,
    };
    let file = std::fs::File::open(path).map_err(|_| Unservable::Unreadable)?;
    Ok(GetResult {
        payload: GetResultPayload::File(file, path.to_path_buf()),
        meta,
        range,
        attributes: Attributes::default(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use object_store::memory::InMemory;
    use object_store::GetRange;
    use tempfile::TempDir;

    /// A fake fetcher: writes each requested file to a temp dir from a fixed body and returns its
    /// path, counting fetches so a test can assert how often the fetch path was taken. A filename not
    /// in `bodies` yields an error, exercising the live-fallback path. After a discard, `repaired`
    /// supplies the body for a key it holds.
    #[derive(Debug)]
    struct FakeFetcher {
        dir: TempDir,
        bodies: std::collections::HashMap<String, Bytes>,
        repaired: std::collections::HashMap<String, Bytes>,
        calls: AtomicUsize,
        discards: AtomicUsize,
    }

    fn bodies_of(files: &[(&str, &[u8])]) -> std::collections::HashMap<String, Bytes> {
        files
            .iter()
            .map(|(name, body)| (name.to_string(), Bytes::copy_from_slice(body)))
            .collect()
    }

    impl FakeFetcher {
        fn new(files: &[(&str, &[u8])], repaired: &[(&str, &[u8])]) -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                bodies: bodies_of(files),
                repaired: bodies_of(repaired),
                calls: AtomicUsize::new(0),
                discards: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl FileFetcher for FakeFetcher {
        async fn fetch(&self, filename: &str) -> anyhow::Result<PathBuf> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let repaired = self.discards.load(Ordering::SeqCst) > 0;
            let body = repaired
                .then(|| self.repaired.get(filename))
                .flatten()
                .or_else(|| self.bodies.get(filename))
                .ok_or_else(|| anyhow::anyhow!("no such file: {filename}"))?;
            let path = self.dir.path().join(filename.replace('/', "_"));
            std::fs::write(&path, body)?;
            Ok(path)
        }

        async fn discard(&self, path: &Path) -> anyhow::Result<()> {
            self.discards.fetch_add(1, Ordering::SeqCst);
            std::fs::remove_file(path)?;
            Ok(())
        }
    }

    fn store_with(files: &[(&str, &[u8])]) -> (FetchStore, Arc<FakeFetcher>, Arc<InMemory>) {
        store_repairing(files, &[])
    }

    fn store_repairing(
        files: &[(&str, &[u8])],
        repaired: &[(&str, &[u8])],
    ) -> (FetchStore, Arc<FakeFetcher>, Arc<InMemory>) {
        let fetcher = Arc::new(FakeFetcher::new(files, repaired));
        let inner = Arc::new(InMemory::new());
        let store = FetchStore::new(inner.clone(), fetcher.clone());
        (store, fetcher, inner)
    }

    /// A `get` serves the fetched file's bytes; the requested range is honored and `meta.size` is the
    /// whole-file size (not the slice).
    #[tokio::test]
    async fn get_serves_fetched_file_and_reports_whole_size() {
        let (store, fetcher, _inner) = store_with(&[("chunks.lance/data/0.lance", b"0123456789")]);
        let loc = OPath::from("chunks.lance/data/0.lance");

        let whole = store.get_opts(&loc, GetOptions::default()).await.unwrap();
        assert_eq!(whole.meta.size, 10, "meta.size is the whole-file size");
        assert_eq!(whole.range, 0..10);
        assert_eq!(whole.bytes().await.unwrap(), Bytes::from_static(b"0123456789"));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1, "one fetch per get");
    }

    /// Every `GetRange` variant resolves to the right slice, while `meta.size` stays the whole file.
    #[tokio::test]
    async fn range_variants_resolve_against_whole_file() {
        let (store, _f, _inner) = store_with(&[("f", b"0123456789")]);
        let loc = OPath::from("f");
        let cases: &[(GetRange, &[u8])] = &[
            (GetRange::Bounded(2..5), b"234"),
            (GetRange::Offset(7), b"789"),
            (GetRange::Suffix(3), b"789"),
        ];
        for (range, want) in cases {
            let opts = GetOptions {
                range: Some(range.clone()),
                ..Default::default()
            };
            let got = store.get_opts(&loc, opts).await.unwrap();
            assert_eq!(got.meta.size, 10, "{range:?}: whole-file size");
            assert_eq!(got.bytes().await.unwrap(), Bytes::copy_from_slice(want), "{range:?}");
        }
    }

    /// A `head` returns the whole-file size from the fetched file without reading a body.
    #[tokio::test]
    async fn head_returns_whole_size_no_body() {
        let (store, _f, _inner) = store_with(&[("idx", b"abcdef")]);
        let opts = GetOptions {
            head: true,
            ..Default::default()
        };
        let got = store.get_opts(&OPath::from("idx"), opts).await.unwrap();
        assert_eq!(got.meta.size, 6);
        assert!(got.bytes().await.unwrap().is_empty(), "head carries no body");
    }

    /// A failed fetch (unknown file) falls back to the inner store, which still serves the bytes — a
    /// transient miss never breaks a read.
    #[tokio::test]
    async fn fetch_failure_falls_back_to_inner() {
        let (store, fetcher, inner) = store_with(&[]);
        let loc = OPath::from("data/live.lance");
        inner
            .put_opts(&loc, PutPayload::from("live"), PutOptions::default())
            .await
            .unwrap();
        let got = store.get_opts(&loc, GetOptions::default()).await.unwrap();
        assert_eq!(got.bytes().await.unwrap(), Bytes::from_static(b"live"));
        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            1,
            "fetch was attempted before falling back"
        );
    }

    /// A copy that cannot span the request is discarded, and the read reflects the object's bytes.
    #[tokio::test]
    async fn short_local_copy_is_repaired_and_served() {
        let (store, fetcher, _inner) = store_repairing(&[("f", b"<Error/>")], &[("f", b"0123456789abcdefghij")]);
        let opts = GetOptions {
            range: Some(GetRange::Bounded(12..16)),
            ..Default::default()
        };

        let got = store.get_opts(&OPath::from("f"), opts).await.unwrap();

        assert_eq!(got.meta.size, 20, "size is the repaired copy's");
        assert_eq!(got.bytes().await.unwrap(), Bytes::from_static(b"cdef"));
        assert_eq!(
            fetcher.discards.load(Ordering::SeqCst),
            1,
            "the short copy was discarded"
        );
        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            2,
            "fetched again after the discard"
        );
    }

    /// A corrupt copy no refetch repairs leaves the inner store to answer, after one discard.
    #[tokio::test]
    async fn unrepairable_copy_falls_back_to_inner() {
        let (store, fetcher, inner) = store_with(&[("f", b"<Error/>")]);
        let loc = OPath::from("f");
        inner
            .put_opts(&loc, PutPayload::from("0123456789abcdefghij"), PutOptions::default())
            .await
            .unwrap();
        let opts = GetOptions {
            range: Some(GetRange::Bounded(12..16)),
            ..Default::default()
        };

        let got = store.get_opts(&loc, opts).await.unwrap();

        assert_eq!(got.bytes().await.unwrap(), Bytes::from_static(b"cdef"));
        assert_eq!(fetcher.discards.load(Ordering::SeqCst), 1, "one repair attempt only");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
    }

    /// `list` is delegated to the inner store (version resolution must stay live) — the fetcher is
    /// never consulted for a listing.
    #[tokio::test]
    async fn list_delegates_to_inner() {
        let (store, fetcher, inner) = store_with(&[]);
        inner
            .put_opts(
                &OPath::from("chunks.lance/_versions/1.manifest"),
                PutPayload::from("m"),
                PutOptions::default(),
            )
            .await
            .unwrap();
        let names: Vec<String> = store
            .list(None)
            .map(|r| r.unwrap().location.to_string())
            .collect()
            .await;
        assert_eq!(names, vec!["chunks.lance/_versions/1.manifest".to_string()]);
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0, "listing never fetches");
    }
}
