//! Trace sources: where the indexer reads agent sessions from. A [`TraceSource`] enumerates the
//! discrete artifacts it indexes ([`Unit`]s — typically files) and parses one on demand into turns.
//! The indexer ([`crate::index`]) drives any source through one generic loop, so adding a new
//! transcript format is: implement [`TraceSource`] and add a branch to [`open`].
//!
//! A unit is both the incremental-tracking granule (skipped when its [`Unit::signature`] still
//! matches `state.json`) and the single-append granule (all of a unit's turns are written in one
//! commit). JSONL is one session per file; a parquet dataset is many sessions in one file.

use crate::claude_traces;
use crate::hf_traces;
use crate::jsonl;
use crate::trace::Turn;

use anyhow::Result;
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
/// tree of Claude Code JSONL transcripts. `limit` caps how many sessions are read (`None` = all) —
/// used to bound a benchmark's build time.
pub fn open(path: &Path, limit: Option<usize>) -> Box<dyn TraceSource> {
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
        Box::new(JsonlTree {
            root: path.to_path_buf(),
            limit,
        })
    }
}

/// "size:mtime_secs" for a file's incremental change-stamp, or `None` if it can't be stat'd.
pub(crate) fn file_sig(p: &Path) -> Option<String> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}:{}", md.len(), mtime))
}

/// A directory tree of Claude Code `*.jsonl` transcripts — one session per file, indexed
/// incrementally (each file skipped while unchanged). `limit` caps the number of files (sessions).
struct JsonlTree {
    root: PathBuf,
    limit: Option<usize>,
}

impl TraceSource for JsonlTree {
    fn describe(&self) -> String {
        format!("scanning transcripts under {}", self.root.display())
    }

    fn units(&self) -> Result<Vec<Unit>> {
        let mut files = jsonl::iter_jsonl_files(&self.root);
        if let Some(n) = self.limit {
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
        let sid = jsonl::session_id_of(p);
        let project = claude_traces::project_of(p);
        Ok(claude_traces::turns_from_jsonl_file(p, &sid, &project)?)
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
        // The project facet for a parquet dataset is its file stem.
        let project = p.file_stem().and_then(|s| s.to_str()).unwrap_or("parquet").to_string();
        hf_traces::turns_from_parquet(p, &project, self.limit)
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
