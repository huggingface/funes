//! Trace sources: where the indexer reads agent sessions from. A [`TraceSource`] enumerates the
//! discrete artifacts it indexes ([`Unit`]s — typically files) and parses one on demand into turns.
//! The indexer ([`crate::index`]) drives any source through one generic loop, so adding a new
//! transcript format is: implement [`TraceSource`] and add a branch to [`open`].
//!
//! A unit is both the incremental-tracking granule (skipped when its [`Unit::signature`] still
//! matches `state.json`) and the single-append granule (all of a unit's turns are written in one
//! commit). JSONL is one session per file; a parquet dataset — or a hermes `state.db` — is many
//! sessions in one file.

use crate::claude_traces;
use crate::codex_traces;
use crate::harness::Harness;
use crate::hermes_traces;
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
    pub is_subagent: bool,
}

/// A source of agent-session transcripts. `units()` is cheap (enumerate + stat, no parsing);
/// `read` parses one unit's turns and is called only for units that aren't skipped.
pub trait TraceSource {
    /// One-line description of what's being indexed (the scan banner's source-kind part).
    fn describe(&self) -> String;

    /// The units to consider, in deterministic order.
    fn units(&self) -> Result<Vec<Unit>>;

    /// Parse one unit into turns (each [`Turn`] already carries its `session_id` and `workdir`).
    fn read(&self, unit: &Unit) -> Result<Vec<Turn>>;

    /// Whether a `read` error aborts the whole index. Best-effort sources (a JSONL tree, where one
    /// unreadable file shouldn't sink the run) return `false`; a single-artifact source (a parquet
    /// dataset) returns `true`, so a corrupt file is a hard failure rather than a silent skip.
    fn fatal_on_read_error(&self) -> bool {
        false
    }
}

/// Pick the source for `path`: a `*.parquet` file is a parquet trace dataset, a hermes `state.db`
/// (or the `~/.hermes` dir holding it) is its SQLite session store, and anything else is a JSONL
/// transcript tree whose harness is auto-detected. `limit` caps how many sessions are read
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
    } else if harness == Some(Harness::Hermes) || is_hermes_path(path) {
        Box::new(HermesDb {
            path: hermes_db_path(path),
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

/// Whether `path` addresses hermes' SQLite store — the `state.db` file itself, or the `~/.hermes`
/// dir that holds it. (`--harness hermes` also forces the hermes source regardless of the path.)
fn is_hermes_path(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("state.db") | Some(".hermes")
    )
}

/// The `state.db` file for a hermes path: the file itself, or `<dir>/state.db` when handed the
/// `~/.hermes` directory.
fn hermes_db_path(path: &Path) -> PathBuf {
    if path.file_name().and_then(|n| n.to_str()) == Some("state.db") {
        path.to_path_buf()
    } else {
        path.join("state.db")
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
        // Newest-first by mtime; `--limit` then keeps the recent N. (Default filename order is
        // ≈ random for UUID-named sessions.)
        files.sort_by_cached_key(|p| {
            std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH))
        });
        if let Some(n) = self.limit {
            files.truncate(n);
        }
        let mut units: Vec<Unit> = files
            .into_iter()
            .map(|p| Unit {
                is_subagent: jsonl::is_subagent(&jsonl::session_id_of(&p)),
                signature: file_sig(&p),
                key: p.to_string_lossy().into_owned(),
            })
            .collect();
        // Subagents last (stable sort preserves recency within each group).
        units.sort_by_key(|u| u.is_subagent);
        Ok(units)
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let p = Path::new(&unit.key);
        // Each parser derives the workdir facet from the session's recorded cwd; the path-derived
        // value is only the fallback for transcripts that never recorded one.
        let fallback = claude_traces::workdir_of(p);
        let turns = match self.harness {
            Harness::Claude => claude_traces::turns_from_jsonl_file(p, &jsonl::session_id_of(p), &fallback)?,
            Harness::Codex => codex_traces::turns_from_jsonl_file(p, &fallback)?,
            Harness::Pi => pi_traces::turns_from_jsonl_file(p, &jsonl::session_id_of(p), &fallback)?,
            // hermes keeps its sessions in a SQLite state.db, not a JSONL tree, so it's read by a
            // dedicated source and never reaches here.
            Harness::Hermes => anyhow::bail!("hermes sessions are read from state.db, not a JSONL tree"),
        };
        Ok(turns)
    }
}

/// hermes' single SQLite `state.db` — many sessions in one file. Each session is a unit signed by
/// its high-water `messages.id`, so an unchanged session is skipped and a grown one is re-read
/// (chunk-id dedup keeps already-stored turns a no-op). `limit` keeps the most-recently-active N.
struct HermesDb {
    path: PathBuf,
    limit: Option<usize>,
}

impl TraceSource for HermesDb {
    fn describe(&self) -> String {
        format!("scanning hermes sessions in {}", self.path.display())
    }

    fn units(&self) -> Result<Vec<Unit>> {
        let mut sessions = hermes_traces::sessions_with_watermark(&self.path)?;
        // Most-recently-active first (highest high-water id); `--limit` then keeps the recent N.
        sessions.sort_by_key(|s| std::cmp::Reverse(s.watermark));
        if let Some(n) = self.limit {
            sessions.truncate(n);
        }
        Ok(sessions
            .into_iter()
            .map(|s| Unit {
                key: s.session_id,
                signature: Some(s.watermark.to_string()),
                is_subagent: false,
            })
            .collect())
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        // The workdir is derived from the session's recorded cwd inside the parser; "hermes" is only
        // the fallback for a session that never recorded one.
        hermes_traces::turns_from_state_db(&self.path, &unit.key, "hermes")
    }
}

/// A parquet agent-trace dataset — many sessions in one file, indexed as a single bulk import.
/// `signature: None` so it's never skipped on stats and never recorded: a re-run always re-reads
/// and dedups by chunk id to a no-op, which also means a wiped memory is never silently skipped.
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
            is_subagent: false,
        }])
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let p = Path::new(&unit.key);
        // Fallback workdir for rows without a recorded cwd: the dataset's file stem.
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
    /// Fallback workdir for rows without a recorded cwd: the shard's file stem (parity with
    /// [`ParquetDataset`]).
    workdir: String,
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
        let workdir = Path::new(shard)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("parquet")
            .to_string();
        shards.push(RemoteShard {
            key: format!("hf://datasets/{owner}/{name}/{shard}"),
            local,
            workdir,
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
                is_subagent: false,
            })
            .collect())
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let shard = self
            .shards
            .iter()
            .find(|s| s.key == unit.key)
            .context("unknown remote shard")?;
        hf_traces::turns_from_parquet(&shard.local, &shard.workdir, None)
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
                    workdir: "0000".into(),
                },
                RemoteShard {
                    key: "hf://datasets/o/n/default/train/0001.parquet".into(),
                    local: "/tmp/b".into(),
                    workdir: "0001".into(),
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
    fn jsonl_tree_orders_subagents_last() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["sess-a.jsonl", "agent-x.jsonl", "sess-b.jsonl", "agent-y.jsonl"] {
            std::fs::write(dir.path().join(name), b"{}\n").unwrap();
        }
        let units = open(dir.path(), None).units().unwrap();
        // Whatever the mtimes, every main precedes every subagent.
        let first_sub = units.iter().position(|u| u.is_subagent).expect("has a subagent unit");
        assert!(units[..first_sub].iter().all(|u| !u.is_subagent));
        assert!(units[first_sub..].iter().all(|u| u.is_subagent));
        assert_eq!(units.iter().filter(|u| u.is_subagent).count(), 2);
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

    #[test]
    fn open_routes_hermes_state_db_and_dir() {
        // The state.db file, and the ~/.hermes dir that holds it, both route to the hermes source.
        assert!(open(Path::new("/x/.hermes/state.db"), None)
            .describe()
            .contains("hermes"));
        assert!(open(Path::new("/x/.hermes"), None).describe().contains("state.db"));
        // `--harness hermes` forces the hermes source even for an unrelated-looking path.
        assert!(open_with_harness(Path::new("/x/whatever"), None, Some(Harness::Hermes))
            .describe()
            .contains("hermes"));
    }

    #[test]
    fn hermes_units_are_sessions_signed_by_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT, \
                content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL NOT NULL, \
                reasoning TEXT, reasoning_content TEXT);
             INSERT INTO sessions (id, cwd) VALUES ('s1','/w'),('s2','/w');
             INSERT INTO messages (session_id, role, content, timestamp) VALUES
                ('s1','user','a',1.0),('s2','user','b',2.0),('s1','assistant','c',3.0);",
        )
        .unwrap();

        let src = open(&db, None);
        let units = src.units().unwrap();
        assert_eq!(units.len(), 2);
        // Most-recent-activity first: s1's high-water id is 3 (ids 1,3) > s2's 2.
        assert_eq!(units[0].key, "s1");
        assert_eq!(units[0].signature.as_deref(), Some("3"));
        assert_eq!(units[1].key, "s2");
        assert_eq!(units[1].signature.as_deref(), Some("2"));
        // read wires through to the parser (s1 has two turns).
        let turns = src.read(&units[0]).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].harness, "hermes");
        // --limit keeps the recent N sessions.
        assert_eq!(open(&db, Some(1)).units().unwrap().len(), 1);
    }
}
