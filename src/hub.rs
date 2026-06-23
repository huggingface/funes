//! Remote, shared memory tiers.
//!
//! A [`Store`] is either the local index or a remote Lance dataset on the HF Hub. Remote
//! reads go over `hf://` and are lazy by construction: the IVF_PQ index bounds which
//! fragments a query touches, and lance's HF object store fetches only those byte ranges.
//! Caching across CLI runs is handled by the **Xet chunk cache** (`~/.cache/huggingface/xet`,
//! on by default), so funes adds no cache layer of its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use lance::dataset::Dataset;

use crate::dataset;
use crate::index::{DIM, MODEL};

/// A store to recall from: a local Lance directory or a remote dataset on the HF Hub.
#[derive(Debug, Clone)]
pub enum Store {
    /// A local Lance store directory (e.g. `~/.funes/lancedb`).
    Local { path: PathBuf },
    /// A remote Lance dataset on the HF Hub, e.g. `hf://datasets/<org>/<repo>`.
    Remote { uri: String, revision: Option<String> },
}

impl Store {
    /// The default local store (`$FUNES_DB` / `~/.funes` → `…/lancedb`).
    pub fn local() -> Self {
        Store::Local {
            path: PathBuf::from(dataset::local_store_dir()),
        }
    }

    /// Parse a store spec: `"local"` → the default local store; an `hf://…` URI → a remote
    /// dataset; anything else → a local store at that path.
    pub fn parse(spec: &str, revision: Option<String>) -> Self {
        if spec == "local" {
            Store::local()
        } else if spec.starts_with("hf://") {
            Store::Remote {
                uri: spec.to_string(),
                revision,
            }
        } else {
            Store::Local {
                path: PathBuf::from(spec),
            }
        }
    }

    /// Resolve the store the read commands should use: an explicit `spec` (a CLI `--store`)
    /// wins, else `$FUNES_STORE`, else the default local store. `revision` is taken from the
    /// flag, else `$FUNES_REVISION` (only meaningful for a remote `hf://` store).
    pub fn resolve(spec: Option<String>, revision: Option<String>) -> Self {
        resolve_from(spec, revision, |k| std::env::var(k).ok())
    }

    /// True only for the *default* local store (`$FUNES_DB`/`~/.funes`), the one a fresh install
    /// uses. An explicit `--store <path>` or `hf://…` is not the default, even when it can't open —
    /// so the hello-world fallback never masks a real "your store is missing" error.
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
            Store::Remote { uri, revision } => {
                let mut opts = HashMap::new();
                if let Some(rev) = revision {
                    opts.insert("revision".to_string(), rev.clone());
                }
                if let Some(token) = hf_token() {
                    opts.insert("hf_token".to_string(), token);
                }
                dataset::open(&dataset::table_uri(uri), opts).await?
            }
        };
        check_compat(&ds)?;
        Ok(ds)
    }
}

/// Pure core of [`Store::resolve`]: explicit `spec` wins over `$FUNES_STORE`, explicit
/// `revision` over `$FUNES_REVISION`; blank env values are ignored. Split out so it's testable
/// without mutating process env.
fn resolve_from(spec: Option<String>, revision: Option<String>, env: impl Fn(&str) -> Option<String>) -> Store {
    let nonempty = |k: &str| env(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    match spec.or_else(|| nonempty("FUNES_STORE")) {
        Some(s) => Store::parse(&s, revision.or_else(|| nonempty("FUNES_REVISION"))),
        None => Store::local(),
    }
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
    fn store_parse_local_path_remote() {
        assert!(matches!(Store::parse("local", None), Store::Local { .. }));
        match Store::parse("/tmp/store", None) {
            Store::Local { path } => assert_eq!(path, std::path::PathBuf::from("/tmp/store")),
            _ => panic!("expected a local path"),
        }
        match Store::parse("hf://datasets/org/kb", Some("abc".into())) {
            Store::Remote { uri, revision } => {
                assert_eq!(uri, "hf://datasets/org/kb");
                assert_eq!(revision.as_deref(), Some("abc"));
            }
            _ => panic!("expected remote"),
        }
    }

    #[test]
    fn store_label() {
        assert_eq!(Store::Local { path: "/tmp/x".into() }.label(), "/tmp/x");
        assert_eq!(
            Store::parse("hf://datasets/org/kb", None).label(),
            "hf://datasets/org/kb"
        );
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
    fn resolve_prefers_explicit_then_env_then_local() {
        let none = |_: &str| None;
        // Explicit spec + revision win outright.
        match resolve_from(Some("hf://datasets/o/r".into()), Some("v1".into()), none) {
            Store::Remote { uri, revision } => {
                assert_eq!(uri, "hf://datasets/o/r");
                assert_eq!(revision.as_deref(), Some("v1"));
            }
            _ => panic!("expected remote"),
        }
        // No spec, no env -> default local store.
        assert!(matches!(resolve_from(None, None, none), Store::Local { .. }));
        // No explicit spec -> $FUNES_STORE used, $FUNES_REVISION fills the revision.
        let env = |k: &str| match k {
            "FUNES_STORE" => Some("hf://datasets/e/r".to_string()),
            "FUNES_REVISION" => Some("envrev".to_string()),
            _ => None,
        };
        match resolve_from(None, None, env) {
            Store::Remote { uri, revision } => {
                assert_eq!(uri, "hf://datasets/e/r");
                assert_eq!(revision.as_deref(), Some("envrev"));
            }
            _ => panic!("expected remote from env"),
        }
        // An explicit (local) spec beats a remote $FUNES_STORE.
        let env2 = |k: &str| (k == "FUNES_STORE").then(|| "hf://datasets/env/wins".to_string());
        match resolve_from(Some("/local/path".into()), None, env2) {
            Store::Local { path } => assert_eq!(path, std::path::PathBuf::from("/local/path")),
            _ => panic!("explicit local path should beat env remote"),
        }
        // A blank $FUNES_STORE is ignored -> local.
        let blank = |k: &str| (k == "FUNES_STORE").then(|| "   ".to_string());
        assert!(matches!(resolve_from(None, None, blank), Store::Local { .. }));
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
