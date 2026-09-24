//! Trace sources: where the indexer reads agent sessions from. A [`TraceSource`] enumerates the
//! discrete artifacts it indexes ([`Unit`]s — typically files) and parses one on demand into turns.
//! The indexer ([`crate::commands::index`]) drives any source through one generic loop, so adding a new
//! transcript format is: implement [`TraceSource`] and add a branch to [`open`].
//!
//! A unit is both the incremental-tracking granule (skipped when its [`Unit::signature`] still
//! matches `state.json`) and the single-append granule (all of a unit's turns are written in one
//! commit). A turns file is one session; a parquet dataset is many sessions in one file.

use super::funes_jsonl;
use super::harness::{self, Harness};
use super::jsonl;
use super::parquet;
use super::Turn;
use crate::hub;

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// One artifact a source indexes as a unit. `key` identifies and locates it: a transcript path, an
/// `hf://` shard uri, a `state.db` and the session inside it. A
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

    /// Whether `key` names one of this store's units.
    fn owns(&self, _key: &str) -> bool {
        false
    }

    /// Keys of every unit this store holds, uncapped by the `limit` that trims `units`.
    fn unit_keys(&self) -> Result<Vec<String>>;
}

/// Pick the source for `path`: a `*.parquet` file is a parquet trace dataset, a `.funes.jsonl` file
/// (or a directory holding them) is a turns file, and anything else is an agent's own session tree,
/// which is refused by naming the integration that converts it. `limit` caps how many sessions are
/// read (`None` = all) — used to bound a benchmark's build time.
pub fn open(path: &Path, limit: Option<usize>) -> Result<Box<dyn TraceSource>> {
    open_with_harness(path, limit, None)
}

/// Like [`open`], but a `Some` `harness` forces the JSONL tree's harness (the CLI's `--harness`)
/// instead of detecting it. A `*.parquet` path is a parquet dataset regardless; a turns file
/// carries its harness in each turn and refuses the override.
pub fn open_with_harness(path: &Path, limit: Option<usize>, harness: Option<Harness>) -> Result<Box<dyn TraceSource>> {
    let is_parquet = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("parquet"));
    if is_parquet {
        return Ok(Box::new(ParquetDataset {
            path: path.to_path_buf(),
            limit,
        }));
    }
    // A path named by a known session dir is refused on its name alone; anything else is listed
    // once, here, and the source that wins keeps the listing.
    let listing = Harness::from_known_dir(path)
        .is_none()
        .then(|| jsonl::iter_jsonl_files(path));
    // Nothing to read is an empty turns store rather than an agent's: a spool no integration has
    // written into yet indexes as a no-op instead of being refused.
    let holds_turns = listing
        .as_ref()
        .is_some_and(|l| l.is_empty() || l.iter().any(|p| funes_jsonl::is_turns_file(p)));
    if holds_turns {
        // A spool is the root funes resolved *from* the harness, so the override it carries is its
        // own and redundant. Anywhere else the flag is a mistake worth naming.
        if harness.is_some() && !harness::is_spool(path) {
            bail!(
                "`--harness` does not apply to {}: a funes JSONL turn names its own harness",
                path.display()
            );
        }
        return Ok(Box::new(funes_jsonl::FunesJsonl::new(
            path,
            listing.unwrap_or_default(),
            limit,
        )));
    }
    let harness = harness.unwrap_or_else(|| detect_harness(path, listing.as_deref()));
    bail!(
        "{0} sessions are converted by its integration — run `funes add {0}`, which indexes them",
        harness.cli_name()
    )
}

/// Detect a JSONL tree's harness: a known session dir wins (a cheap tail match), else sniff the
/// first record of the `listing`'s first transcript (see [`Harness::detect`]).
fn detect_harness(root: &Path, listing: Option<&[PathBuf]>) -> Harness {
    if let Some(h) = Harness::from_known_dir(root) {
        return h;
    }
    let first = listing.and_then(|l| l.first()).and_then(|p| jsonl::first_record(p));
    Harness::detect(root, first.as_ref())
}

/// "size:mtime_secs" for a file's incremental change-stamp, or `None` if it can't be stat'd.
pub(crate) fn file_sig(p: &Path) -> Option<String> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}:{}", md.len(), mtime))
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

    fn unit_keys(&self) -> Result<Vec<String>> {
        Ok(vec![self.path.to_string_lossy().into_owned()])
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let p = Path::new(&unit.key);
        // Fallback workdir for rows without a recorded cwd: the dataset's file stem.
        let fallback = p.file_stem().and_then(|s| s.to_str()).unwrap_or("parquet").to_string();
        parquet::turns_from_parquet(p, &fallback, self.limit)
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
    let remote = parquet::resolve_parquet(owner, name, token.as_deref()).await?;
    let mut paths = remote.shards;
    if let Some(n) = max_shards {
        paths.truncate(n);
    }
    let mut shards = Vec::with_capacity(paths.len());
    for shard in &paths {
        let local = parquet::download_shard(&remote.repo, shard, &remote.revision).await?;
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

    fn unit_keys(&self) -> Result<Vec<String>> {
        Ok(self.shards.iter().map(|s| s.key.clone()).collect())
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let shard = self
            .shards
            .iter()
            .find(|s| s.key == unit.key)
            .context("unknown remote shard")?;
        parquet::turns_from_parquet(&shard.local, &shard.workdir, None)
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
        assert!(open(Path::new("/x/data.parquet"), None)
            .unwrap()
            .describe()
            .contains("parquet"));
        assert!(open(Path::new("/x/DATA.PARQUET"), None)
            .unwrap()
            .describe()
            .contains("parquet"));
        let err = open(Path::new("/x/.claude/projects"), None).err().expect("refused");
        assert!(err.to_string().contains("funes add claude"), "{err}");
    }

    #[test]
    fn open_routes_turns_files_and_refuses_a_harness_override() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("thread.funes.jsonl");
        std::fs::write(&file, b"").unwrap();
        assert!(open(&file, None).unwrap().describe().contains("funes JSONL"));
        assert!(open(dir.path(), None).unwrap().describe().contains("funes JSONL"));
        let nested = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(nested.path().join("a/b")).unwrap();
        std::fs::write(nested.path().join("a/b/t.funes.jsonl"), b"").unwrap();
        assert!(open(nested.path(), None).unwrap().describe().contains("funes JSONL"));
        let err = open_with_harness(&file, None, Some(Harness::Claude))
            .err()
            .expect("refused");
        assert!(err.to_string().contains("--harness"), "{err}");
    }

    #[test]
    fn a_transcript_names_the_integration_that_converts_it_and_an_empty_root_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.jsonl"), b"{\"type\":\"session_meta\"}\n").unwrap();
        let err = open(dir.path(), None).err().expect("refused");
        assert!(err.to_string().contains("funes add codex"), "{err}");
        let empty = tempfile::tempdir().unwrap();
        assert!(open(empty.path(), None).unwrap().units().unwrap().is_empty());
    }

    #[test]
    fn a_parquet_dataset_is_one_unsigned_unit_owning_nothing() {
        // Never skipped and never recorded: a re-run re-reads and dedups by chunk id.
        let pq = open(Path::new("/x/data.parquet"), None).unwrap();
        let units = pq.units().unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].signature.is_none());
        assert!(!pq.owns("/x/projects/-a-project/s.funes.jsonl"));
    }
}
