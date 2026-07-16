//! Trace sources: where the indexer reads agent sessions from. A [`TraceSource`] enumerates the
//! discrete artifacts it indexes ([`Unit`]s — typically files) and parses one on demand into turns.
//! The indexer ([`crate::index`]) drives any source through one generic loop, so adding a new
//! transcript format is: implement [`TraceSource`] and add a branch to [`open`].
//!
//! A unit is both the incremental-tracking granule (skipped when its [`Unit::signature`] still
//! matches `state.json`) and the single-append granule (all of a unit's turns are written in one
//! commit). JSONL is one session per file; a parquet dataset is many sessions in one file.

use crate::claude_traces;
use crate::codex_traces;
use crate::harness::Harness;
use crate::hf_dataset;
use crate::hf_traces;
use crate::hub;
use crate::jsonl;
use crate::pi_traces;
use crate::trace::Turn;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// One artifact a source indexes as a unit. `key` is its `state.json` identity (the path). A
/// `Some` `signature` is a cheap change-stamp: the unit is skipped when it still matches what was
/// recorded, and recorded after a successful index. `None` means "always read, never recorded" —
/// for a bulk source whose idempotency comes from chunk-id dedup, not file stats.
pub struct Unit {
    pub key: String,
    pub signature: Option<String>,
}

/// A source of agent-session transcripts. `units()` is cheap (enumerate + stat, no parsing);
/// `read` parses one unit's turns and is called only for units that aren't skipped.
pub trait TraceSource {
    /// One-line description of what's being indexed (the scan banner's source-kind part).
    fn describe(&self) -> String;

    /// The units to consider, in deterministic order.
    fn units(&self) -> Result<Vec<Unit>>;

    /// Parse one unit into turns (each [`Turn`] already carries its `session_id` and `project`).
    fn read(&self, unit: &Unit) -> Result<Vec<Turn>>;

    /// Whether a `read` error aborts the whole index. Best-effort sources (a JSONL tree, where one
    /// unreadable file shouldn't sink the run) return `false`; a single-artifact source (a parquet
    /// dataset) returns `true`, so a corrupt file is a hard failure rather than a silent skip.
    fn fatal_on_read_error(&self) -> bool {
        false
    }
}

/// Pick the source for `path`: a `*.parquet` file is a parquet trace dataset; anything else is a
/// JSONL transcript tree whose harness is auto-detected. `limit` caps how many sessions are read
/// (`None` = all) — used to bound a benchmark's build time.
pub fn open(path: &Path, limit: Option<usize>) -> Box<dyn TraceSource> {
    open_with_harness(path, limit, None)
}

/// Like [`open`], but a `Some` `harness` forces the JSONL tree's harness (the CLI's `--harness`)
/// instead of detecting it. A `*.parquet` path is a parquet dataset regardless.
pub fn open_with_harness(path: &Path, limit: Option<usize>, harness: Option<Harness>) -> Box<dyn TraceSource> {
    let is_parquet = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("parquet"));
    if is_parquet {
        Box::new(ParquetDataset {
            path: path.to_path_buf(),
            limit,
        })
    } else {
        let harness = harness.unwrap_or_else(|| detect_harness(path));
        Box::new(JsonlTree {
            root: path.to_path_buf(),
            limit,
            harness,
        })
    }
}

/// Detect a JSONL tree's harness: a known session dir wins (a cheap tail match), else sniff the
/// first transcript's first record — only then is the tree walked (see [`Harness::detect`]).
fn detect_harness(root: &Path) -> Harness {
    if let Some(h) = Harness::from_known_dir(root) {
        return h;
    }
    let first = jsonl::iter_jsonl_files(root)
        .first()
        .and_then(|p| jsonl::first_record(p));
    Harness::detect(root, first.as_ref())
}

/// "size:mtime_secs" for a file's incremental change-stamp, or `None` if it can't be stat'd.
pub(crate) fn file_sig(p: &Path) -> Option<String> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}:{}", md.len(), mtime))
}

/// A directory tree of `*.jsonl` transcripts for one `harness` — one session per file, indexed
/// incrementally (each file skipped while unchanged). `limit` keeps the most recent N files
/// (sessions) by mtime.
struct JsonlTree {
    root: PathBuf,
    limit: Option<usize>,
    harness: Harness,
}

impl TraceSource for JsonlTree {
    fn describe(&self) -> String {
        format!(
            "scanning {} transcripts under {}",
            self.harness.as_str(),
            self.root.display()
        )
    }

    fn units(&self) -> Result<Vec<Unit>> {
        let mut files = jsonl::iter_jsonl_files(&self.root);
        if let Some(n) = self.limit {
            // Keep the most recent `n` by mtime: recall values recency, and the default order is by
            // filename (≈ random for UUID-named sessions), so a plain truncate would drop recent work.
            files.sort_by_cached_key(|p| {
                std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH))
            });
            files.truncate(n);
        }
        Ok(files
            .into_iter()
            .map(|p| Unit {
                signature: file_sig(&p),
                key: p.to_string_lossy().into_owned(),
            })
            .collect())
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let p = Path::new(&unit.key);
        // Each parser derives the project facet from the session's recorded cwd; the path-derived
        // value is only the fallback for transcripts that never recorded one.
        let fallback = claude_traces::project_of(p);
        let turns = match self.harness {
            Harness::Claude => claude_traces::turns_from_jsonl_file(p, &jsonl::session_id_of(p), &fallback)?,
            Harness::Codex => codex_traces::turns_from_jsonl_file(p, &fallback)?,
            Harness::Pi => pi_traces::turns_from_jsonl_file(p, &jsonl::session_id_of(p), &fallback)?,
        };
        Ok(turns)
    }
}

/// A parquet agent-trace dataset — many sessions in one file, indexed as a single bulk import.
/// `signature: None` so it's never skipped on stats and never recorded: a re-run always re-reads
/// and dedups by chunk id to a no-op, which also means a wiped store is never silently skipped.
/// `limit` caps how many of its sessions (rows) are read.
struct ParquetDataset {
    path: PathBuf,
    limit: Option<usize>,
}

impl TraceSource for ParquetDataset {
    fn describe(&self) -> String {
        format!("indexing parquet dataset {}", self.path.display())
    }

    fn units(&self) -> Result<Vec<Unit>> {
        Ok(vec![Unit {
            key: self.path.to_string_lossy().into_owned(),
            signature: None,
        }])
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let p = Path::new(&unit.key);
        // Fallback project for rows without a recorded cwd: the dataset's file stem.
        let fallback = p.file_stem().and_then(|s| s.to_str()).unwrap_or("parquet").to_string();
        hf_traces::turns_from_parquet(p, &fallback, self.limit)
    }

    fn fatal_on_read_error(&self) -> bool {
        true
    }
}

/// One pre-downloaded parquet shard of a remote trace dataset.
struct RemoteShard {
    /// `state.json` key: `hf://datasets/<owner>/<name>/<shard>` — stable and disjoint from any
    /// local path, so cross-source incremental never collides.
    key: String,
    /// The shard downloaded whole into hf-hub's cache.
    local: PathBuf,
    /// Fallback project for rows without a recorded cwd: the shard's file stem (parity with
    /// [`ParquetDataset`]).
    project: String,
}

/// A Hub trace dataset's `refs/convert/parquet` shards, resolved and pre-downloaded by
/// [`open_remote`]. Each shard is a unit signed with the convert-branch commit oid, so an unchanged
/// repo is skipped without re-reading; a changed repo re-reads and chunk-id dedup keeps rows already
/// stored a no-op.
struct RemoteParquetDataset {
    shards: Vec<RemoteShard>,
    /// The convert-branch commit — every shard's incremental signature.
    revision: String,
    label: String,
}

/// Resolve `<owner>/<name>`'s auto-converted parquet, download its shards whole-file into hf-hub's
/// cache, and return a source over them. Async (resolve + download happen here) so `read` stays
/// sync — the indexer never blocks a Tokio worker on a download. `max_shards` caps how many shards
/// are downloaded and indexed (for the gated live test); all sessions within each are read.
pub async fn open_remote(owner: &str, name: &str, max_shards: Option<usize>) -> Result<Box<dyn TraceSource>> {
    let token = hub::hf_token();
    let remote = hf_dataset::resolve_parquet(owner, name, token.as_deref()).await?;
    let mut paths = remote.shards;
    if let Some(n) = max_shards {
        paths.truncate(n);
    }
    let mut shards = Vec::with_capacity(paths.len());
    for shard in &paths {
        let local = hf_dataset::download_shard(&remote.repo, shard, &remote.revision).await?;
        let project = Path::new(shard)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("parquet")
            .to_string();
        shards.push(RemoteShard {
            key: format!("hf://datasets/{owner}/{name}/{shard}"),
            local,
            project,
        });
    }
    Ok(Box::new(RemoteParquetDataset {
        shards,
        revision: remote.revision,
        label: format!("{owner}/{name}"),
    }))
}

impl TraceSource for RemoteParquetDataset {
    fn describe(&self) -> String {
        let oid8 = &self.revision[..self.revision.len().min(8)];
        format!(
            "indexing {} — {} parquet shard(s) @ refs/convert/parquet:{oid8}",
            self.label,
            self.shards.len()
        )
    }

    fn units(&self) -> Result<Vec<Unit>> {
        Ok(self
            .shards
            .iter()
            .map(|s| Unit {
                key: s.key.clone(),
                signature: Some(self.revision.clone()),
            })
            .collect())
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let shard = self
            .shards
            .iter()
            .find(|s| s.key == unit.key)
            .context("unknown remote shard")?;
        hf_traces::turns_from_parquet(&shard.local, &shard.project, None)
    }

    fn fatal_on_read_error(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn remote_parquet_units_are_shards_signed_by_the_convert_oid() {
        let ds = RemoteParquetDataset {
            shards: vec![
                RemoteShard {
                    key: "hf://datasets/o/n/default/train/0000.parquet".into(),
                    local: "/tmp/a".into(),
                    project: "0000".into(),
                },
                RemoteShard {
                    key: "hf://datasets/o/n/default/train/0001.parquet".into(),
                    local: "/tmp/b".into(),
                    project: "0001".into(),
                },
            ],
            revision: "abc123".into(),
            label: "o/n".into(),
        };
        let units = ds.units().unwrap();
        assert_eq!(units.len(), 2);
        // Every shard is signed by the convert-branch oid, so an unchanged repo skips.
        assert!(units.iter().all(|u| u.signature.as_deref() == Some("abc123")));
        assert_eq!(units[0].key, "hf://datasets/o/n/default/train/0000.parquet");
        assert!(ds.fatal_on_read_error());
    }

    #[test]
    fn file_sig_is_len_colon_mtime() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"hello").unwrap();
        f.flush().unwrap();
        let sig = file_sig(f.path()).expect("stat-able file has a signature");
        let (len, mtime) = sig.split_once(':').expect("sig is len:mtime");
        assert_eq!(len, "5");
        assert!(mtime.parse::<u64>().is_ok());
    }

    #[test]
    fn file_sig_is_none_for_missing_file() {
        assert!(file_sig(Path::new("/no/such/file")).is_none());
    }

    #[test]
    fn open_dispatches_by_extension() {
        assert!(open(Path::new("/x/data.parquet"), None).describe().contains("parquet"));
        assert!(open(Path::new("/x/DATA.PARQUET"), None).describe().contains("parquet"));
        assert!(open(Path::new("/x/projects"), None).describe().contains("transcripts"));
    }

    #[test]
    fn jsonl_tree_units_are_files_with_signatures() {
        // A *.jsonl file under the tree becomes a unit keyed by its path, with a size:mtime stamp;
        // a parquet dataset is a single signature-less unit (never skipped/recorded).
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sess.jsonl");
        std::fs::write(&f, b"{}\n").unwrap();

        let units = open(dir.path(), None).units().unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].key, f.to_string_lossy());
        assert!(units[0].signature.is_some());

        let pq = open(Path::new("/x/data.parquet"), None).units().unwrap();
        assert_eq!(pq.len(), 1);
        assert!(pq[0].signature.is_none());
    }

    #[test]
    fn jsonl_limit_caps_units() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("s{i}.jsonl")), b"{}\n").unwrap();
        }
        assert_eq!(open(dir.path(), Some(2)).units().unwrap().len(), 2);
        assert_eq!(open(dir.path(), None).units().unwrap().len(), 5);
    }
}
