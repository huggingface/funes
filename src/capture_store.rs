//! A write-capturing object-store decorator.
//!
//! [`CaptureStore`] wraps an inner [`ObjectStore`](object_store::ObjectStore): it records every
//! write in memory instead of forwarding it, and delegates reads to the inner store — except for a
//! path it has already captured, which it serves back (read-your-writes). A caller can run a
//! sequence of writes against it and then recover exactly the files that would have been written,
//! keyed by path, with nothing reaching the backend.
//!
//! ```text
//!   put → captured in memory      (never reaches the inner store)
//!   get → captured if present, else delegated to the inner store
//! ```
//!
//! It is generic: the inner store is any `ObjectStore`, so the tests exercise it over an in-memory
//! backend, and what to do with the captured files is left to the caller.
//!
//! **A decorator, because Rust has no inheritance.** You can't subclass a store and override
//! `put`; the idiomatic stand-in is to hold the `inner` store, forward every method unchanged, and
//! intercept the ones that matter. The cost is the hand-written delegation of each `ObjectStore`
//! method below — Rust has no auto-delegation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::stream::{self, BoxStream, StreamExt};
use object_store::path::Path as OPath;
use object_store::{
    Attributes, CopyOptions, GetOptions, GetRange, GetResult, GetResultPayload, ListResult, MultipartUpload,
    ObjectMeta, ObjectStore as OSObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    Result as OSResult, UploadPart,
};

/// In-memory capture of object-store writes: path → bytes. Shared between a [`CaptureStore`] and
/// whatever holds it, so the writes can be read back after the fact.
pub(crate) type Captured = Arc<Mutex<BTreeMap<OPath, Bytes>>>;

/// Reads delegate to `inner` unless the path was already captured (read-your-writes, so a caller
/// can read back what it just wrote); writes are captured and never forwarded.
#[derive(Debug)]
pub(crate) struct CaptureStore {
    inner: Arc<dyn OSObjectStore>,
    captured: Captured,
}

impl CaptureStore {
    /// Decorate `inner`, recording writes into the shared `captured` map.
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
        // The decorator never writes to the underlying store — a copy lands in the capture. The
        // source comes from the capture if present, else a read of the underlying store.
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
        let store = CaptureStore::new(inner.clone(), Captured::default());
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
            store.captured.lock().unwrap().contains_key(&p),
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
        assert!(store.captured.lock().unwrap().contains_key(&to));
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
}
