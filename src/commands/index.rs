//! The `index` command: read a [`crate::traces::source::TraceSource`] → parse → chunk → write to a
//! local Lance dataset → embed. One generic loop drives every source — a JSONL tree today, new
//! formats by implementing the trait — writing each of its units in a single append, unembedded;
//! the vectors are filled afterwards from what the memory says is still pending.
//!
//! Incremental on two levels: skip a unit whose stamp (size:mtime) is unchanged *and* whose rows
//! state.json records as all written; and within a re-read unit add only chunks whose id is new — a
//! grown session (the same memory) contributes just its new turns, nothing is re-embedded or
//! deleted.

use crate::chunk::{self, Tier};
use crate::hub;
use crate::inference::{self, embed_batched, Embedder};
use crate::memory::dataset::{self, build_batch, schema, MODEL};
use crate::memory::lock;
use crate::scan;
use crate::traces::harness::Harness;
use crate::traces::{self, repo, source};
use anyhow::{anyhow, Context, Result};
use arrow_array::{Array, RecordBatchIterator, StringArray, UInt64Array};
use futures::TryStreamExt;
use lance::dataset::{Dataset, WriteParams, ROW_ID};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Take the memory lock. An interactive caller (a human at `funes index`/`funes add`) waits out a
/// brief contention — up to 3 retries, 5s apart — since a memory operation rarely runs long; an
/// automated run (a hook) bails at once and re-sweeps next turn.
async fn acquire_lock(interactive: bool) -> Result<lock::MemoryLock> {
    let retries = if interactive { 3 } else { 0 };
    for attempt in 0..=retries {
        if let Some(l) = lock::MemoryLock::try_acquire()? {
            return Ok(l);
        }
        if attempt < retries {
            eprintln!(
                "funes: another memory operation is in progress; retrying in 5s… ({}/{retries})",
                attempt + 1
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    Err(anyhow!(
        "another funes memory operation is in progress; retry in a moment"
    ))
}

/// Every chunk id already stored. Re-indexing keeps only the chunks whose id isn't here, so a grown
/// session (the same memory) contributes just its new turns — nothing is re-embedded or deleted. (A
/// rewritten turn arrives under new ids, i.e. as another memory.) Chunk ids are global, so one
/// unfiltered scan dedups any unit, whether it holds one session or thousands.
async fn stored_ids(ds: &Dataset) -> Result<HashSet<String>> {
    let batches = dataset::scan_rows(ds, &["id"], None, None).await?;
    let mut ids = HashSet::new();
    for batch in batches {
        ids.extend(str_column(&batch, "id")?.into_iter().map(str::to_string));
    }
    Ok(ids)
}

/// A batch's string column, row by row.
fn str_column<'a>(batch: &'a arrow_array::RecordBatch, name: &str) -> Result<Vec<&'a str>> {
    let col = batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .with_context(|| format!("missing or non-string column {name}"))?;
    Ok((0..batch.num_rows()).map(|i| col.value(i)).collect())
}

/// Elide every block's inline base64 `data:` URI payloads, before [`redact_turns`] scans them: a
/// screenshot's entropy trips detectors on values that are not secrets, and excising one of those
/// plants a marker mid-payload that strands the rest of it in the store. Runs whether or not a
/// scanner is installed — the payload is unrecallable either way.
fn elide_turns(turns: &mut [traces::Turn]) {
    for b in turns.iter_mut().flat_map(|t| t.blocks.iter_mut()) {
        if let Cow::Owned(elided) = chunk::elide_data_uris(&b.text) {
            b.text = elided;
        }
    }
}

/// Redact secrets from `units`' turns *before* chunking — so a long key that chunking would split
/// across pieces is whole when scanned, and never reaches the embedding, the local memory, or (via
/// push) the Hub. One scanner run covers every unit given: the spawn costs ~1 s, the scan itself
/// milliseconds. Scans exactly the blocks the run will store ([`chunk::block_selected`]).
/// Best-effort: removes a secret whose value byte-matches the stored text (the common case, real
/// newlines); anything that resists is caught downstream by the fail-closed push gate. Reports to
/// stderr what it removed, per unit.
fn redact_units(
    units: &mut [&mut [traces::Turn]],
    scanner: &dyn scan::SecretScanner,
    include_thinking: bool,
) -> Result<()> {
    let n_units = units.len();
    let mut blocks: Vec<(usize, &mut traces::Block)> = Vec::new();
    for (u, turns) in units.iter_mut().enumerate() {
        for b in turns.iter_mut().flat_map(|t| t.blocks.iter_mut()) {
            if chunk::block_selected(&b.block_type, &Tier::ALL, include_thinking) {
                blocks.push((u, b));
            }
        }
    }
    if blocks.is_empty() {
        return Ok(());
    }
    let per_block = {
        let texts: Vec<&str> = blocks.iter().map(|(_, b)| b.text.as_str()).collect();
        scan::scan_blocks(&texts, scanner)?
    };
    let mut removed: Vec<Vec<String>> = (0..n_units).map(|_| Vec::new()).collect();
    for ((u, b), findings) in blocks.into_iter().zip(&per_block) {
        let r = scan::excise(&b.text, findings);
        removed[u].extend(r.removed_detectors);
        b.text = r.text;
    }
    for (turns, removed) in units.iter().zip(removed) {
        if removed.is_empty() {
            continue;
        }
        let sid = turns.first().map(|t| t.session_id.as_str()).unwrap_or("?");
        eprintln!(
            "    redacted {} secret(s) in {sid}: {}",
            removed.len(),
            scan::summary(removed.iter().map(String::as_str))
        );
    }
    Ok(())
}

/// One unit's turns chunked as the memory stores them — data URIs elided and, with a `scanner`,
/// secrets redacted first, since both change the text a block splits into.
fn chunks_of(
    turns: &mut [traces::Turn],
    include_thinking: bool,
    scanner: Option<&scan::Trufflehog>,
) -> Result<Vec<chunk::Chunk>> {
    elide_turns(turns);
    if let Some(scanner) = scanner {
        redact_units(&mut [turns], scanner, include_thinking)?;
    }
    Ok(chunk::chunks_from_turns(turns, &Tier::ALL, include_thinking))
}

/// The secret scanner, if installed. Best-effort: without one, indexing continues unredacted — the
/// push gate still scans, fail-closed, before any upload, so a secret can't reach the Hub.
fn find_scanner() -> Option<scan::Trufflehog> {
    match scan::Trufflehog::find() {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("note: secret redaction disabled — {e}");
            None
        }
    }
}

/// A unit's distinct-session count and a log label: `"<sid> (<workdir>)"` for a single session (a
/// JSONL file), `"<n> sessions"` for a bulk unit (many sessions in one artifact), and the unit's `key` (its
/// path) when it has no turns at all. The borrow of `turns` is confined here so callers keep it mutable.
fn unit_summary(turns: &[traces::Turn], key: &str) -> (u64, String) {
    let mut sids: Vec<&str> = turns.iter().map(|t| t.session_id.as_str()).collect();
    sids.sort_unstable();
    sids.dedup();
    let label = match (sids.len(), turns.first()) {
        (0, _) => key.to_string(),
        (1, Some(t)) => format!("{} ({})", t.session_id, t.workdir),
        (n, _) => format!("{n} sessions"),
    };
    (sids.len() as u64, label)
}

/// What `state.json` records per unit: the change-stamp last seen and how far it was indexed.
#[derive(Serialize, Deserialize, Clone)]
struct UnitState {
    sig: String,
    level: Level,
}

/// How far a unit got. `Shallow`: every row is in the memory, and what still owes a vector is the
/// memory's `vector IS NULL`, not the unit's. The tier levels are legacy stamps: that tier and
/// below written and embedded, deeper tiers not written.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    Text,
    ToolUse,
    ToolResult,
    Shallow,
}

impl Level {
    /// Whether every row of the unit is in the memory.
    fn rows_written(self) -> bool {
        matches!(self, Level::Shallow | Level::ToolResult)
    }
}

/// Whether a recorded unit is current: its stamp still matches and every row of it is written. A
/// legacy stamp below the top tier still owes its deeper rows.
fn unit_current(entry: Option<&UnitState>, sig: &str) -> bool {
    entry.is_some_and(|e| e.sig == sig && e.level.rows_written())
}

/// Lightweight coverage snapshot written by indexing runs for `status` to read without walking
/// the transcript trees again.
#[derive(Serialize, Deserialize, Default)]
struct IndexCoverageSnapshot {
    pending: HashSet<String>,
}

pub(crate) struct IndexCoverage {
    pub pending: usize,
}

/// Drop the pending keys a store claims and no longer holds. A store that can't list its units
/// says nothing about any of them.
fn retire_vanished_units(path: &Path, sources: &[Box<dyn source::TraceSource>]) -> Result<()> {
    let mut snapshot = read_index_coverage(path);
    for src in sources {
        let Ok(keys) = src.unit_keys() else {
            continue;
        };
        let held: HashSet<String> = keys.into_iter().collect();
        snapshot.pending.retain(|key| !src.owns(key) || held.contains(key));
    }
    write_snapshot(path, &snapshot)
}

fn update_index_coverage<'a>(
    mut snapshot: IndexCoverageSnapshot,
    units: impl IntoIterator<Item = &'a source::Unit>,
    state: &HashMap<String, UnitState>,
) -> IndexCoverageSnapshot {
    for unit in units {
        // A unit with no signature can never be known up to date.
        let Some(sig) = &unit.signature else {
            continue;
        };
        if unit_current(state.get(&unit.key), sig) {
            snapshot.pending.remove(&unit.key);
        } else {
            snapshot.pending.insert(unit.key.clone());
        }
    }
    snapshot
}

fn read_index_coverage(path: &Path) -> IndexCoverageSnapshot {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_snapshot(path: &Path, snapshot: &IndexCoverageSnapshot) -> Result<()> {
    std::fs::write(path, serde_json::to_string(snapshot)?)
        .with_context(|| format!("writing index coverage at {}", path.display()))
}

fn write_index_coverage(
    path: &Path,
    sources: &[Box<dyn source::TraceSource>],
    units: &[(usize, source::Unit)],
    state: &HashMap<String, UnitState>,
) -> Result<()> {
    let owned = units
        .iter()
        .filter(|(si, unit)| sources[*si].owns(&unit.key))
        .map(|(_, unit)| unit);
    let snapshot = update_index_coverage(read_index_coverage(path), owned, state);
    write_snapshot(path, &snapshot)
}

/// Native sessions that the most recent indexing sweep found short of a complete index. `None`
/// means no sweep has written a readable snapshot yet; status omits the line rather than doing an
/// unbounded recursive transcript scan.
pub(crate) fn local_index_coverage() -> Option<IndexCoverage> {
    std::fs::read_to_string(dataset::funes_dir().join("index-coverage.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<IndexCoverageSnapshot>(&text).ok())
        .map(|snapshot| IndexCoverage {
            pending: snapshot.pending.len(),
        })
}

/// A set-up indexer: it holds the memory lock, embedder, dataset, redaction scanner, and incremental
/// state, so a caller can index units in whatever batches it likes — one at a time to check the
/// clock between them, or all at once — without reloading the model. Build the indexes once at the
/// end with [`Indexer::finalize`].
struct Indexer {
    uri: String,
    ds: Option<Dataset>,
    /// [`stored_ids`] at open plus everything written this run — the dedup baseline for new chunks.
    existing: HashSet<String>,
    embedder: Box<dyn Embedder>,
    scanner: Option<scan::Trufflehog>,
    include_thinking: bool,
    /// cwd → resolved `repo` value, so each distinct checkout runs `git` once across the run.
    repo_cache: HashMap<String, String>,
    state: HashMap<String, UnitState>,
    state_path: PathBuf,
    coverage_path: PathBuf,
    /// The memory didn't exist when this run opened it — the first index.
    first_index: bool,
    /// A human is watching (stdin is a terminal) — probed once here, so every prompt-or-proceed
    /// choice in a run agrees.
    interactive: bool,
    /// The caller stopped early with passes still owed, so the summary must not claim the memory is
    /// up to date.
    work_remaining: bool,
    /// Units (by index) already counted in `n_sessions` this run, so a tier-major caller's repeat
    /// passes over one unit don't recount its sessions.
    counted: HashSet<usize>,
    _lock: lock::MemoryLock,
    /// The sources and their units, enumerated once at open so the change-stamps are a stable
    /// snapshot; a caller drives them by index via [`Indexer::index_unit`].
    sources: Vec<Box<dyn source::TraceSource>>,
    units: Vec<(usize, source::Unit)>,
    n_sessions: u64,
    n_skipped: u64,
    n_chunks: u64,
    n_embedded: u64,
    /// Units (by index) whose read failed this run — reported once, and not retried by a later
    /// tier pass; no state is recorded, so the next run retries them.
    rejected: HashSet<usize>,
}

/// Enumerate every source's units (each source orders its own — recency-desc, subagents last),
/// tagged with the source index. When several sources are indexed at once (e.g. every known
/// harness), one that fails to enumerate — say a hermes state.db this build can't read — is warned
/// and skipped instead of aborting the rest; a lone source stays fatal, since its failure is then
/// the whole result. But if the skips left nothing to index and at least one source errored, that's
/// a failure, not a silent success — whereas an all-empty run with no errors is a legitimate no-op.
fn collect_units(sources: &[Box<dyn source::TraceSource>]) -> Result<Vec<(usize, source::Unit)>> {
    let isolate = sources.len() > 1;
    let mut units = Vec::new();
    let mut errors = 0usize;
    for (i, src) in sources.iter().enumerate() {
        match src.units() {
            Ok(src_units) => units.extend(src_units.into_iter().map(|u| (i, u))),
            Err(e) if isolate => {
                errors += 1;
                eprintln!("{}: enumeration failed, skipping — {:#}", src.describe(), e);
            }
            Err(e) => return Err(e.context(src.describe())),
        }
    }
    // `errors > 0` implies the isolate path (a lone failure returned above), so this fires only
    // when every collected source was empty and at least one was skipped for erroring.
    if units.is_empty() && errors > 0 {
        anyhow::bail!(
            "no sessions to index — {errors} of {} sources failed to enumerate (see the warnings above)",
            sources.len()
        );
    }
    Ok(units)
}

impl Indexer {
    /// Acquire the memory lock, open (or plan to create) the dataset, load incremental state, bring
    /// up the embedder and secret scanner, and enumerate `sources`' units.
    async fn open(sources: Vec<Box<dyn source::TraceSource>>, no_thinking: bool) -> Result<Indexer> {
        let dir = dataset::funes_dir();
        std::fs::create_dir_all(&dir)?;
        let interactive = std::io::stdin().is_terminal();
        // Held for the whole run so the stored-id read and the appends see one stable version.
        let _lock = acquire_lock(interactive).await?;

        let uri = dataset::table_uri(&dataset::local_memory_dir());
        let ds = dataset::open(&uri, HashMap::new()).await.ok();

        // Model-pin: refuse to add to a memory built with a different embedding model. The id rides
        // in the dataset's schema metadata; a pre-metadata memory (no id) is tolerated and guarded
        // only by the dimension check until it is reindexed.
        if let Some(ds) = &ds {
            let schema = arrow_schema::Schema::from(ds.schema());
            if let Some(em) = schema.metadata().get("embedding_model") {
                if em != MODEL {
                    return Err(anyhow!("index built with model {em:?}, refusing to mix with {MODEL:?}"));
                }
            }
        }

        let first_index = ds.is_none();

        // Incremental state: path -> {size:mtime stamp, tier}; an unreadable or old-schema file →
        // empty. A first index (memory missing) owes everything, whatever an old state.json says — a
        // stale one would silently skip every unit against the empty memory.
        let state_path = dir.join("state.json");
        let coverage_path = dir.join("index-coverage.json");
        let state = if first_index {
            HashMap::new()
        } else {
            std::fs::read_to_string(&state_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        };

        let embedder: Box<dyn Embedder> = inference::embedder()?;
        let scanner = find_scanner();

        let existing = match &ds {
            Some(d) => stored_ids(d).await?,
            None => HashSet::new(),
        };

        let units = collect_units(&sources)?;
        // A unit can be deleted between sweeps.
        retire_vanished_units(&coverage_path, &sources)?;
        write_index_coverage(&coverage_path, &sources, &units, &state)?;

        Ok(Indexer {
            uri,
            ds,
            existing,
            embedder,
            scanner,
            include_thinking: !no_thinking,
            repo_cache: HashMap::new(),
            state,
            state_path,
            coverage_path,
            first_index,
            interactive,
            work_remaining: false,
            counted: HashSet::new(),
            _lock,
            sources,
            units,
            n_sessions: 0,
            n_skipped: 0,
            n_chunks: 0,
            n_embedded: 0,
            rejected: HashSet::new(),
        })
    }

    /// Number of units this run will consider.
    fn unit_count(&self) -> usize {
        self.units.len()
    }

    /// Units still owing their rows — a pure state + signature check, no session read, so a caller
    /// can plan a run before touching anything. A signature-less (bulk) unit always counts as
    /// pending, as [`Indexer::write_units`] never skips it.
    fn pending(&self) -> Vec<usize> {
        (0..self.units.len())
            .filter(|&i| {
                let unit = &self.units[i].1;
                match &unit.signature {
                    Some(sig) => !unit_current(self.state.get(&unit.key), sig),
                    None => true,
                }
            })
            .collect()
    }

    /// Write the rows of one batch of `units`, taken from its front — the shallow rung, the
    /// primitive a caller loops over until every unit is consumed. A batch fills at
    /// [`SHALLOW_BATCH`] units read or [`SHALLOW_BATCH_BYTES`] of their text, whichever comes
    /// first. Skips a unit already written at its current signature; a signature-less (bulk) unit
    /// is never skipped — it is re-read every run, and its chunk-id dedup makes that a no-op.
    /// Otherwise reads each unit, redacts them all in one scanner run, chunks every tier, appends
    /// the chunks the memory lacks — unembedded — and stamps each unit. Returns how many units it
    /// consumed. `done` is how many units the caller drove before this batch, for the progress
    /// label.
    ///
    /// Add-only-new: a grown session is the same memory — add only its new turns, never rewriting or
    /// deleting what's unchanged. (A rewritten turn lands under new ids.) A unit's turns are written
    /// in one append, so a bulk source (many sessions in one unit) stays a single Lance fragment
    /// rather than one per session.
    async fn write_units(&mut self, units: &[usize], done: usize, total: usize) -> Result<usize> {
        let mut read: Vec<(usize, String, Vec<traces::Turn>)> = Vec::new();
        let mut bytes = 0usize;
        let mut consumed = 0;
        for (n, &i) in units.iter().enumerate() {
            consumed = n + 1;
            let progress = format!("[{}/{total}]", done + n + 1);
            let (src_i, unit) = &self.units[i];
            if let Some(sig) = &unit.signature {
                if unit_current(self.state.get(&unit.key), sig) {
                    self.n_skipped += 1;
                    continue;
                }
            }
            if self.rejected.contains(&i) {
                continue;
            }
            // Best-effort sources retry a failed read next run (no state recorded); a fatal source
            // aborts rather than silently dropping data.
            let src = &self.sources[*src_i];
            match src.read(unit) {
                Ok(turns) => {
                    bytes += turns
                        .iter()
                        .flat_map(|t| &t.blocks)
                        .map(|b| b.text.len())
                        .sum::<usize>();
                    read.push((i, progress, turns));
                }
                Err(e) if !src.fatal_on_read_error() => {
                    eprintln!("{progress} {} — rejected: {e}", unit.key);
                    self.rejected.insert(i);
                }
                Err(e) => return Err(e),
            }
            if read.len() >= SHALLOW_BATCH || bytes >= SHALLOW_BATCH_BYTES {
                break;
            }
        }
        for (_, _, turns) in &mut read {
            elide_turns(turns);
        }
        if let Some(scanner) = &self.scanner {
            let mut all: Vec<&mut [traces::Turn]> = read.iter_mut().map(|(_, _, t)| t.as_mut_slice()).collect();
            redact_units(&mut all, scanner, self.include_thinking)?;
        }

        for (i, progress, turns) in read {
            let (key, sig) = {
                let unit = &self.units[i].1;
                (unit.key.clone(), unit.signature.clone())
            };
            let (sessions, label) = unit_summary(&turns, &key);
            let mut chunks = chunk::chunks_from_turns(&turns, &Tier::ALL, self.include_thinking);
            let mut repo_by_turn: HashMap<(&str, &str), String> = HashMap::new();
            for t in &turns {
                if let Some(cwd) = &t.cwd {
                    repo_by_turn
                        .entry((t.session_id.as_str(), t.turn_uuid.as_str()))
                        .or_insert_with(|| self.repo_for(cwd));
                }
            }
            for c in &mut chunks {
                if let Some(repo) = repo_by_turn.get(&(c.session_id.as_str(), c.turn_uuid.as_str())) {
                    c.repo.clone_from(repo);
                }
            }
            let total_chunks = chunks.len();
            let added = if total_chunks == 0 {
                eprintln!("{progress} {label} — no indexable content");
                0
            } else {
                // A unit can carry one id twice (a turn re-emitted under its `turn_uuid`); the first wins,
                // as it would have had the two arrived in separate runs.
                let mut in_batch = HashSet::new();
                let new_chunks: Vec<chunk::Chunk> = chunks
                    .into_iter()
                    .filter(|c| !self.existing.contains(&c.id) && in_batch.insert(c.id.clone()))
                    .collect();
                if new_chunks.is_empty() {
                    eprintln!("{progress} {label} — {total_chunks} chunks, all already indexed");
                    0
                } else {
                    eprintln!("{progress} {label} — {} new of {total_chunks} chunks", new_chunks.len());
                    self.write_rows(&new_chunks).await?
                }
            };

            // Record state only for signed units, even when they produced no chunks ("remembered when
            // empty"), and persist after each so an interrupted run is resumable.
            if let Some(sig) = sig {
                self.state.insert(
                    key,
                    UnitState {
                        sig,
                        level: Level::Shallow,
                    },
                );
                std::fs::write(&self.state_path, serde_json::to_string_pretty(&self.state)?)?;
                write_index_coverage(&self.coverage_path, &self.sources, &self.units, &self.state)?;
            }
            if self.counted.insert(i) {
                self.n_sessions += sessions;
            }
            self.n_chunks += added;
        }
        Ok(consumed)
    }

    /// [`repo::of_cwd`], cached so each distinct checkout runs `git` once across the run.
    fn repo_for(&mut self, cwd: &str) -> String {
        self.repo_cache
            .entry(cwd.to_string())
            .or_insert_with(|| repo::of_cwd(cwd))
            .clone()
    }

    /// Append `chunks` unembedded, creating the dataset on the first write.
    async fn write_rows(&mut self, chunks: &[chunk::Chunk]) -> Result<u64> {
        if chunks.is_empty() {
            return Ok(0);
        }
        let batch = build_batch(chunks, None)?;
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema());
        let uri = self.uri.clone();
        match &mut self.ds {
            Some(d) => {
                d.append(reader, None).await?;
            }
            None => {
                self.ds = Some(Dataset::write(reader, &uri, Some(WriteParams::default())).await?);
            }
        }
        self.existing.extend(chunks.iter().map(|c| c.id.clone()));
        Ok(chunks.len() as u64)
    }

    /// [`pending_embeddings`] of the memory; nothing before its first write.
    async fn pending_embeddings(&self) -> Result<PendingEmbeddings> {
        match &self.ds {
            Some(ds) => pending_embeddings(ds).await,
            None => Ok(PendingEmbeddings::default()),
        }
    }

    /// Embed `tier`'s pending rows session by session, filling their vectors in place in fills of
    /// about [`EMBED_BATCH`] rows cut at session boundaries. `keep_going(embedded)` is asked after
    /// each fill that leaves rows; `false` stops. Returns the rows embedded.
    async fn embed_tier(
        &mut self,
        tier: Tier,
        sessions: &[(String, Vec<u64>)],
        mut keep_going: impl FnMut(usize) -> bool,
    ) -> Result<usize> {
        let total: usize = sessions.iter().map(|(_, rows)| rows.len()).sum();
        let mut embedded = 0;
        let mut fill = Vec::new();
        let t0 = Instant::now();
        for (n, (_, rows)) in sessions.iter().enumerate() {
            fill.extend(rows);
            if fill.len() < EMBED_BATCH && n + 1 < sessions.len() {
                continue;
            }
            self.fill(&fill).await?;
            embedded += fill.len();
            fill.clear();
            eprintln!(
                "\r    {}: embedded {embedded}/{total}  ({:.0}/s)        ",
                tier.label(),
                embedded as f64 / t0.elapsed().as_secs_f64().max(0.001)
            );
            if embedded < total && !keep_going(embedded) {
                break;
            }
        }
        Ok(embedded)
    }

    /// Embed the rows at `row_ids` from their stored text and fill their vectors in place.
    async fn fill(&mut self, row_ids: &[u64]) -> Result<()> {
        let ds = self.ds.as_ref().context("embedding rows before any was written")?;
        let rows = ds.take_rows(row_ids, ds.schema().project(&["id", "text"])?).await?;
        let ids = str_column(&rows, "id")?;
        let texts = str_column(&rows, "text")?;
        let n = texts.len();
        let vectors = embed_batched(self.embedder.as_mut(), &texts, |done| {
            eprint!("\r    embedding {done}/{n}   ");
            let _ = std::io::stderr().flush();
        })?;
        let filled = dataset::fill_vectors(ds, &ids, &vectors).await?;
        self.ds = Some(filled);
        self.n_embedded += n as u64;
        Ok(())
    }

    /// Build the FTS + IVF_PQ indexes (best-effort), reap superseded versions, and print the run
    /// summary. Consumes the indexer, releasing the memory lock. The vector index bounds how much a
    /// query reads — what makes recall over a remote (hf://) tier lazy rather than a full scan; lance
    /// enforces its own training minimum (256 rows) and skips below it, falling back to brute force.
    async fn finalize(mut self) -> Result<()> {
        // Nothing written → the memory is unchanged since it opened; skip the rebuild and its
        // version churn.
        if self.n_chunks > 0 {
            if let Some(d) = &mut self.ds {
                dataset::build_indexes(d, |phase| eprintln!("building {phase}…")).await;

                // Reap superseded versions — best-effort; on failure the reap waits for next run.
                match d.cleanup_old_versions(chrono::Duration::minutes(10), None, None).await {
                    Ok(stats) if stats.bytes_removed > 0 => eprintln!(
                        "reclaimed {:.1} MB from {} old version(s)",
                        stats.bytes_removed as f64 / 1e6,
                        stats.old_versions
                    ),
                    Ok(_) => {}
                    Err(e) => eprintln!("note: version cleanup skipped — {e}"),
                }
            }
        }
        println!(
            "{}",
            run_summary(
                self.interactive && !self.work_remaining,
                self.n_sessions,
                self.n_skipped,
                self.n_chunks,
                self.n_embedded,
                self.rejected.len() as u64,
                self.units.len(),
            )
        );
        if !self.rejected.is_empty() {
            anyhow::bail!("{} unit(s) rejected", self.rejected.len());
        }
        Ok(())
    }
}

/// The run summary line. An interactive rerun that added nothing — and left nothing owed or
/// rejected — gets a friendly "up to date" instead of a zero-count tally; an automated run (no
/// reader) or any run that wrote, embedded, rejected or still owes something gets the tally.
fn run_summary(
    done: bool,
    sessions: u64,
    skipped: u64,
    chunks: u64,
    embedded: u64,
    rejected: u64,
    units: usize,
) -> String {
    if done && chunks == 0 && embedded == 0 && rejected == 0 {
        format!("up to date ({units} sessions, all embedded)")
    } else if rejected == 0 {
        format!("indexed sessions={sessions} skipped={skipped} chunks={chunks} embedded={embedded}")
    } else {
        format!("indexed sessions={sessions} skipped={skipped} chunks={chunks} embedded={embedded} rejected={rejected}")
    }
}

/// Rows still owing a vector: per tier, the sessions holding them in storage order, each with its
/// row ids. Costs the vector column whole (`vector IS NULL`) — ~0.25 s at 170k rows.
#[derive(Default)]
struct PendingEmbeddings {
    by_tier: BTreeMap<Tier, Vec<(String, Vec<u64>)>>,
    total: usize,
}

impl PendingEmbeddings {
    fn tier_total(&self, tier: Tier) -> usize {
        self.by_tier
            .get(&tier)
            .map_or(0, |sessions| sessions.iter().map(|(_, rows)| rows.len()).sum())
    }
}

async fn pending_embeddings(ds: &Dataset) -> Result<PendingEmbeddings> {
    let mut scan = ds.scan();
    scan.project(&["session_id", "block_type"])?;
    scan.with_row_id();
    scan.filter("vector IS NULL")?;
    let mut stream = scan.try_into_stream().await?;
    let mut pending = PendingEmbeddings::default();
    while let Some(batch) = stream.try_next().await? {
        let sessions = str_column(&batch, "session_id")?;
        let block_types = str_column(&batch, "block_type")?;
        let row_ids = batch
            .column_by_name(ROW_ID)
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            .context("pending rows: missing or non-u64 row ids")?;
        for i in 0..batch.num_rows() {
            let sessions_of_tier = pending.by_tier.entry(Tier::of_block(block_types[i])).or_default();
            match sessions_of_tier.last_mut() {
                Some((session, rows)) if session == sessions[i] => rows.push(row_ids.value(i)),
                _ => sessions_of_tier.push((sessions[i].to_string(), vec![row_ids.value(i)])),
            }
            pending.total += 1;
        }
    }
    Ok(pending)
}

/// Build/update the local index from one or more source roots — each `(path, harness override)`
/// where `None` auto-detects. All roots share one memory, embedder, and `state.json` (keyed by
/// absolute file path, so cross-root incremental works). Writes only locally — publishing is the
/// separate `push`. `max_sessions` caps sessions *per root* to the most recent N (`None` = all).
/// `yes` skips the first-index confirmation (`--yes`).
pub async fn run_index_roots(
    roots: &[(PathBuf, Option<Harness>)],
    no_thinking: bool,
    max_sessions: Option<usize>,
    yes: bool,
) -> Result<()> {
    let sources = roots
        .iter()
        .map(|(path, harness)| source::open_with_harness(path, max_sessions, *harness))
        .collect::<Result<Vec<_>>>()?;
    index_sources(sources, no_thinking, yes).await
}

/// Index a Hub trace dataset (`funes index <org/repo>`): resolve its `refs/convert/parquet` shards,
/// download them, and index — through the same pipeline as the local sources. `uri` is the
/// `hf://datasets/<owner>/<name>` form (the CLI resolves a shorthand to it).
pub async fn run_index_remote(uri: &str, no_thinking: bool) -> Result<()> {
    let (owner, name, _prefix) = hub::parse_hf(uri)?;
    let src = source::open_remote(&owner, &name, None).await?;
    // A Hub import is an explicit, deliberate command — skip the first-index confirmation.
    index_sources(vec![src], no_thinking, true).await
}

/// The wall-clock budget a budgeted run gives itself: it stops at the first unit-batch or embed-fill
/// boundary past this. Deeper tiers and older sessions backfill on later runs.
const INDEX_BUDGET_SECS: u64 = 60;

/// Units read and scanned for secrets together: one scanner spawn (~1 s) per batch rather than per
/// unit — over a first index of hundreds of sessions, seconds instead of ten minutes before a row
/// lands.
const SHALLOW_BATCH: usize = 32;

/// Block text a batch holds before it is scanned and written: bounds the memory a batch of bulk
/// units (a Hub shard holds thousands of sessions) takes, where the unit count alone would not.
const SHALLOW_BATCH_BYTES: usize = 64 << 20;

/// Rows embedded per fill. A fill rewrites the vector column of every fragment it touches, so fills
/// span whole sessions and are not cut small.
const EMBED_BATCH: usize = 512;

/// What a budgeted run does when the budget expires with work still owed.
#[derive(Clone, Copy)]
enum Finish {
    /// Stop at the boundary — later runs catch up.
    Stop,
    /// Offer to finish the rest now (interactive only; otherwise stop).
    Ask,
    /// Finish everything without asking (`--yes`).
    All,
}

/// Whether a run past its budget goes on: only if the caller said so, or a human agrees.
fn go_on(finish: Finish, interactive: bool, remaining: Duration) -> bool {
    match finish {
        Finish::All => true,
        Finish::Ask if interactive => confirm_continue(remaining),
        _ => false,
    }
}

/// Build/update the local index from harness session roots, budgeted and rung-major: every owed
/// session's rows first, then the pending embeddings text → tool_use → tool_result, stopping at
/// the first boundary past the budget. The no-path `funes index` — the per-turn hook advances the
/// backfill one bounded step per run; an interactive run offers to finish the rest; `yes` finishes
/// it without asking.
pub async fn run_index_budgeted(
    roots: &[(PathBuf, Option<Harness>)],
    no_thinking: bool,
    max_sessions: Option<usize>,
    yes: bool,
) -> Result<()> {
    let sources = roots
        .iter()
        .map(|(path, harness)| source::open_with_harness(path, max_sessions, *harness))
        .collect::<Result<Vec<_>>>()?;
    let finish = if yes { Finish::All } else { Finish::Ask };
    run_budgeted(sources, no_thinking, finish).await
}

/// The `funes add` first index: the budgeted drain with no finish prompt — the add flow already
/// asked, and the per-turn drip owns whatever the budget defers. Rows land first, so recall answers
/// from full-text search within the minute; embeddings follow, text (decisions, rationale) first.
pub async fn run_index_seed(root: &Path, harness: Harness) -> Result<()> {
    let sources = vec![source::open_with_harness(root, None, Some(harness))?];
    run_budgeted(sources, false, Finish::Stop).await
}

/// Drive `sources` rung-major — every owed unit's rows, then the pending embeddings tier by tier —
/// checking the budget after each unit batch and each fill; `finish` says what to do when it
/// expires with work left. The owed units come from state alone (no reading) and the pending rows
/// from the memory, so the plan reflects what this run actually owes.
async fn run_budgeted(sources: Vec<Box<dyn source::TraceSource>>, no_thinking: bool, finish: Finish) -> Result<()> {
    let mut idx = Indexer::open(sources, no_thinking).await?;
    let interactive = idx.interactive;
    let owed = idx.pending();
    let mut pending = idx.pending_embeddings().await?;
    if owed.is_empty() && pending.total == 0 {
        return idx.finalize().await;
    }
    eprintln!(
        "to index — {} session(s) to write, {} chunk(s) to embed",
        owed.len(),
        pending.total
    );

    let start = Instant::now();
    let budget = Duration::from_secs(INDEX_BUDGET_SECS);
    let mut capped = true;
    let mut done = 0usize;
    while done < owed.len() {
        done += idx.write_units(&owed[done..], done, owed.len()).await?;
        if capped && start.elapsed() >= budget {
            if !go_on(
                finish,
                interactive,
                estimate_remaining(start.elapsed(), done, owed.len()),
            ) {
                eprintln!(
                    "{} session(s) left to write — per-turn indexing (or a `funes index` rerun) picks them up",
                    owed.len() - done
                );
                idx.work_remaining = true;
                return idx.finalize().await;
            }
            capped = false; // finish the rest now
        }
    }

    // The rows just written are pending too.
    if done > 0 {
        pending = idx.pending_embeddings().await?;
    }
    let embed_start = Instant::now();
    let mut before = 0usize;
    for tier in Tier::ALL {
        let Some(sessions) = pending.by_tier.get(&tier) else {
            continue;
        };
        let embedded = idx
            .embed_tier(tier, sessions, |embedded| {
                if !capped || start.elapsed() < budget {
                    return true;
                }
                let remaining = estimate_remaining(embed_start.elapsed(), before + embedded, pending.total);
                capped = !go_on(finish, interactive, remaining);
                !capped
            })
            .await?;
        if embedded < pending.tier_total(tier) {
            eprintln!(
                "{} chunk(s) left to embed — per-turn indexing (or a `funes index` rerun) picks them up",
                pending.total - before - embedded
            );
            idx.work_remaining = true;
            break;
        }
        before += embedded;
    }
    idx.finalize().await
}

/// Index a set of already-opened sources fully — every unit's rows, then every pending embedding —
/// sharing one embedder, `state.json`, and dataset handle across them (state keyed by absolute
/// path / `hf://…` shard, so incremental works cross-source). On a first interactive index it
/// estimates the embedding run after the first fill and asks before the long haul.
async fn index_sources(sources: Vec<Box<dyn source::TraceSource>>, no_thinking: bool, yes: bool) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let mut indexer = Indexer::open(sources, no_thinking).await?;
    let total = indexer.unit_count();

    // Per-source tally of what this run owes.
    for (si, src) in indexer.sources.iter().enumerate() {
        let units = indexer.units.iter().filter(|(i, _)| *i == si);
        let (mut n, mut cached) = (0usize, 0usize);
        for (_, u) in units {
            n += 1;
            if u.signature
                .as_ref()
                .is_some_and(|s| unit_current(indexer.state.get(&u.key), s))
            {
                cached += 1;
            }
        }
        eprintln!("{} — {} to index, {cached} cached", src.describe(), n - cached);
    }

    let all: Vec<usize> = (0..total).collect();
    let mut done = 0;
    while done < total {
        done += indexer.write_units(&all[done..], done, total).await?;
    }

    // First interactive index: the rows are in and searchable; estimate the embedding run from the
    // first fill and — if it looks long — ask whether to continue or stop here (a rerun resumes).
    let pending = indexer.pending_embeddings().await?;
    let mut probe = indexer.first_index && !yes && interactive;
    let t0 = Instant::now();
    let mut before = 0usize;
    for tier in Tier::ALL {
        let Some(sessions) = pending.by_tier.get(&tier) else {
            continue;
        };
        let embedded = indexer
            .embed_tier(tier, sessions, |embedded| {
                if !probe {
                    return true;
                }
                probe = false;
                let est = t0.elapsed().mul_f64(pending.total as f64 / (before + embedded) as f64);
                est < Duration::from_secs(FIRST_INDEX_PROMPT_SECS) || confirm_full_index(pending.total, est)
            })
            .await?;
        if embedded < pending.tier_total(tier) {
            eprintln!(
                "stopped with {} chunk(s) unembedded (kept — the index is searchable and resumable). \
                 Re-run `funes index` to embed the rest.",
                pending.total - before - embedded
            );
            indexer.work_remaining = true;
            break;
        }
        before += embedded;
    }

    indexer.finalize().await
}

/// What a dry run found.
pub struct CheckReport {
    pub text: String,
    pub rejected: usize,
    pub duplicate_ids: usize,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.rejected == 0 && self.duplicate_ids == 0
    }
}

/// Dry-run `path`: read and chunk every unit exactly as an index would, count turns and chunks,
/// find the ids a unit produces twice (a turn re-emitted under its `turn_uuid` would be deduped
/// away, never indexed), and write nothing — no lock, no memory, no model.
pub fn check(path: &Path, no_thinking: bool, limit: Option<usize>, harness: Option<Harness>) -> Result<CheckReport> {
    if !path.exists() {
        anyhow::bail!("no such path: {}", path.display());
    }
    let src = source::open_with_harness(path, limit, harness)?;
    let units = src.units()?;
    let scanner = find_scanner();
    let mut text = format!("checking {}\n", path.display());
    let (mut turns, mut chunks, mut rejected, mut duplicate_ids) = (0usize, 0usize, 0usize, 0usize);
    for unit in &units {
        let mut unit_turns = match src.read(unit) {
            Ok(t) => t,
            Err(e) => {
                rejected += 1;
                text.push_str(&format!("  {} — rejected: {e}\n", unit.key));
                continue;
            }
        };
        let unit_chunks = chunks_of(&mut unit_turns, !no_thinking, scanner.as_ref())?;
        text.push_str(&format!(
            "  {} — {} turns, {} chunks\n",
            unit.key,
            unit_turns.len(),
            unit_chunks.len()
        ));
        let mut seen = HashSet::new();
        for c in unit_chunks.iter().filter(|c| !seen.insert(c.id.as_str())) {
            duplicate_ids += 1;
            text.push_str(&format!(
                "    duplicate id {}: session {} turn {} block {} split {}\n",
                c.id, c.session_id, c.turn_uuid, c.block_idx, c.split_idx
            ));
        }
        turns += unit_turns.len();
        chunks += unit_chunks.len();
    }
    text.push_str(&format!(
        "checked {} unit(s): {turns} turns, {chunks} chunks, {rejected} rejected, {duplicate_ids} duplicate id(s)\n",
        units.len()
    ));
    Ok(CheckReport {
        text,
        rejected,
        duplicate_ids,
    })
}

/// Build/update the local index from a single source root, auto-detecting its harness — a thin
/// convenience over [`run_index_roots`] for a single path (tests, benchmarks, one explicit path).
/// Passes `yes = true`: these callers are non-interactive and must not gate on the first-index prompt.
pub async fn run_index(path: &Path, no_thinking: bool, max_sessions: Option<usize>) -> Result<()> {
    run_index_roots(&[(path.to_path_buf(), None)], no_thinking, max_sessions, true).await
}

/// A first interactive index estimated at ≥ this many seconds prompts before continuing.
const FIRST_INDEX_PROMPT_SECS: u64 = 120;

/// Ask `prompt` on stderr and read one stdin line. Enter takes the default; anything but `y`/`yes`
/// is a no, and EOF or a read error declines — never start long work off a wedged stdin.
fn confirm(prompt: &str, default_yes: bool) -> bool {
    eprint!("{prompt} ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    match std::io::stdin().read_line(&mut answer) {
        Ok(n) if n > 0 => match answer.trim().to_ascii_lowercase().as_str() {
            "" => default_yes,
            "y" | "yes" => true,
            _ => false,
        },
        _ => false,
    }
}

/// Prompt before a long first embedding run (interactive only): continue, or stop with the rows in
/// and resume later. Returns whether to proceed.
fn confirm_full_index(chunks: usize, est: Duration) -> bool {
    confirm(
        &format!(
            "embedding {chunks} chunks is estimated at ~{} (rough, from the first batch). Continue? [y/N]  \
             (the rows are in and searchable; a later `funes index` embeds the rest)",
            fmt_eta(est)
        ),
        false,
    )
}

/// After a budgeted pass leaves work unfinished, ask whether to finish the rest now; default yes.
/// `remaining` is a rough estimate of the time left.
fn confirm_continue(remaining: Duration) -> bool {
    confirm(
        &format!(
            "more to index (~{} left, rough). Finish it now? [Y/n]  (or let per-turn indexing catch up)",
            fmt_eta(remaining)
        ),
        true,
    )
}

/// Extrapolate the time left from the average cost of the passes done so far — cached and cheap
/// passes count, so the estimate tracks the run's real mix rather than its slowest pass.
fn estimate_remaining(elapsed: Duration, processed: usize, total: usize) -> Duration {
    if processed == 0 || processed >= total {
        return Duration::ZERO;
    }
    elapsed / processed as u32 * (total - processed) as u32
}

/// Rough human ETA: "45s", "12 min", "2.3 h".
fn fmt_eta(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 90.0 {
        format!("{s:.0}s")
    } else if s < 5400.0 {
        format!("{:.0} min", s / 60.0)
    } else {
        format!("{:.1} h", s / 3600.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source whose enumeration yields fixed unit keys, or fails — for `collect_units` tests.
    struct MockSource {
        name: &'static str,
        keys: Vec<&'static str>,
        fail: bool,
    }

    impl source::TraceSource for MockSource {
        fn describe(&self) -> String {
            self.name.to_string()
        }
        fn units(&self) -> Result<Vec<source::Unit>> {
            if self.fail {
                anyhow::bail!("enumerate failed: {}", self.name);
            }
            Ok(self
                .keys
                .iter()
                .map(|k| source::Unit {
                    key: k.to_string(),
                    signature: Some("sig".to_string()),
                    is_subagent: false,
                })
                .collect())
        }
        fn unit_keys(&self) -> Result<Vec<String>> {
            Ok(self.keys.iter().map(|k| k.to_string()).collect())
        }
        fn read(&self, _: &source::Unit) -> Result<Vec<traces::Turn>> {
            Ok(vec![])
        }
    }

    #[test]
    fn collect_units_skips_a_failing_source_among_several() {
        let srcs: Vec<Box<dyn source::TraceSource>> = vec![
            Box::new(MockSource {
                name: "good-a",
                keys: vec!["a1"],
                fail: false,
            }),
            Box::new(MockSource {
                name: "bad",
                keys: vec![],
                fail: true,
            }),
            Box::new(MockSource {
                name: "good-b",
                keys: vec!["b1", "b2"],
                fail: false,
            }),
        ];
        let units = collect_units(&srcs).unwrap();
        // The failing source (index 1) is skipped; the others keep their source-tagged units.
        let got: Vec<(usize, &str)> = units.iter().map(|(i, u)| (*i, u.key.as_str())).collect();
        assert_eq!(got, vec![(0, "a1"), (2, "b1"), (2, "b2")]);
    }

    #[test]
    fn collect_units_is_fatal_for_a_lone_failing_source() {
        let srcs: Vec<Box<dyn source::TraceSource>> = vec![Box::new(MockSource {
            name: "only",
            keys: vec![],
            fail: true,
        })];
        assert!(collect_units(&srcs).is_err());
    }

    #[test]
    fn collect_units_fails_when_every_source_errors_and_nothing_is_collected() {
        let srcs: Vec<Box<dyn source::TraceSource>> = vec![
            Box::new(MockSource {
                name: "bad-a",
                keys: vec![],
                fail: true,
            }),
            Box::new(MockSource {
                name: "bad-b",
                keys: vec![],
                fail: true,
            }),
        ];
        // 0 units + a skipped error must not look like a successful no-op.
        assert!(collect_units(&srcs).is_err());
    }

    #[test]
    fn collect_units_is_a_noop_for_empty_sources_without_errors() {
        let srcs: Vec<Box<dyn source::TraceSource>> = vec![
            Box::new(MockSource {
                name: "empty-a",
                keys: vec![],
                fail: false,
            }),
            Box::new(MockSource {
                name: "empty-b",
                keys: vec![],
                fail: false,
            }),
        ];
        // Nothing to index but nothing errored → a legitimate empty result, not a failure.
        assert!(collect_units(&srcs).unwrap().is_empty());
    }

    #[test]
    fn unit_current_needs_matching_sig_and_every_row_written() {
        let shallow = UnitState {
            sig: "10:20".into(),
            level: Level::Shallow,
        };
        assert!(unit_current(Some(&shallow), "10:20"));
        assert!(
            !unit_current(Some(&shallow), "99:99"),
            "a changed stamp is never current"
        );
        assert!(!unit_current(None, "10:20"));
        // Legacy tier-major stamps: only the top tier had every row written.
        for (level, current) in [(Level::Text, false), (Level::ToolUse, false), (Level::ToolResult, true)] {
            let legacy = UnitState {
                sig: "10:20".into(),
                level,
            };
            assert_eq!(unit_current(Some(&legacy), "10:20"), current, "{level:?}");
        }
    }

    #[test]
    fn legacy_state_levels_still_deserialize() {
        let state: HashMap<String, UnitState> =
            serde_json::from_str(r#"{"a":{"sig":"1:2","level":"ToolResult"},"b":{"sig":"3:4","level":"Text"}}"#)
                .unwrap();
        assert_eq!(state["a"].level, Level::ToolResult);
        assert_eq!(state["b"].level, Level::Text);
        assert_eq!(serde_json::to_string(&Level::Shallow).unwrap(), r#""Shallow""#);
    }

    #[test]
    fn index_coverage_merges_native_pending_units_across_sweeps() {
        let unit = |key: &str, sig: Option<&str>| source::Unit {
            key: key.to_string(),
            signature: sig.map(str::to_string),
            is_subagent: false,
        };
        let first = vec![
            unit("current", Some("1")),
            unit("partial", Some("2")),
            unit("stale", Some("new")),
            unit("new", Some("4")),
            unit("unsigned", None),
        ];
        let state = HashMap::from([
            (
                "current".to_string(),
                UnitState {
                    sig: "1".into(),
                    level: Level::ToolResult,
                },
            ),
            (
                "partial".to_string(),
                UnitState {
                    sig: "2".into(),
                    level: Level::Text,
                },
            ),
            (
                "stale".to_string(),
                UnitState {
                    sig: "old".into(),
                    level: Level::ToolResult,
                },
            ),
        ]);
        let snapshot = update_index_coverage(IndexCoverageSnapshot::default(), &first, &state);
        assert_eq!(
            snapshot.pending,
            ["partial", "stale", "new"].into_iter().map(str::to_string).collect()
        );

        let second = [unit("other-harness", Some("5"))];
        let snapshot = update_index_coverage(snapshot, &second, &state);
        assert!(snapshot.pending.contains("partial"));
        assert!(snapshot.pending.contains("other-harness"));
    }

    fn pending_after_a_sweep(coverage: &Path, sources: &[Box<dyn source::TraceSource>]) -> HashSet<String> {
        let units = collect_units(sources).unwrap();
        retire_vanished_units(coverage, sources).unwrap();
        write_index_coverage(coverage, sources, &units, &HashMap::new()).unwrap();
        let snapshot: IndexCoverageSnapshot =
            serde_json::from_str(&std::fs::read_to_string(coverage).unwrap()).unwrap();
        snapshot.pending
    }

    #[test]
    fn index_coverage_retires_a_transcript_the_tree_no_longer_lists() {
        let dir = tempfile::tempdir().unwrap();
        let coverage = dir.path().join("index-coverage.json");
        let root = dir.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        for f in ["a.jsonl", "b.jsonl"] {
            std::fs::write(root.join(f), b"{}\n").unwrap();
        }
        let key = |f: &str| root.join(f).to_string_lossy().into_owned();
        let sweep = || -> Vec<Box<dyn source::TraceSource>> {
            vec![
                source::open_with_harness(&root, None, Some(Harness::Claude)).unwrap(),
                // A store that claims no key contributes none, however its units are signed.
                Box::new(MockSource {
                    name: "remote",
                    keys: vec!["hf://datasets/acme/traces/shard.parquet"],
                    fail: false,
                }),
            ]
        };
        assert_eq!(
            pending_after_a_sweep(&coverage, &sweep()),
            HashSet::from([key("a.jsonl"), key("b.jsonl")])
        );

        std::fs::remove_file(root.join("b.jsonl")).unwrap();
        assert_eq!(
            pending_after_a_sweep(&coverage, &sweep()),
            HashSet::from([key("a.jsonl")])
        );
    }

    #[test]
    fn index_coverage_retires_a_session_the_hermes_db_no_longer_holds() {
        let dir = tempfile::tempdir().unwrap();
        let coverage = dir.path().join("index-coverage.json");
        let db = dir.path().join("state.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT, \
                content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL NOT NULL, \
                reasoning TEXT, reasoning_content TEXT);
             INSERT INTO sessions (id, cwd) VALUES ('s1','/w'),('s2','/w');
             INSERT INTO messages (session_id, role, content, timestamp) VALUES
                ('s1','user','a',1.0),('s2','user','b',2.0);",
        )
        .unwrap();
        let key = |sid: &str| format!("{}#{sid}", db.display());
        let sweep = || -> Vec<Box<dyn source::TraceSource>> {
            vec![source::open_with_harness(&db, None, Some(Harness::Hermes)).unwrap()]
        };
        assert_eq!(
            pending_after_a_sweep(&coverage, &sweep()),
            HashSet::from([key("s1"), key("s2")])
        );

        // The db outlives the session it dropped, and the key is no path to probe for.
        conn.execute_batch("DELETE FROM messages WHERE session_id = 's2'")
            .unwrap();
        assert_eq!(pending_after_a_sweep(&coverage, &sweep()), HashSet::from([key("s1")]));
    }

    #[test]
    fn run_summary_says_up_to_date_only_on_a_done_no_op() {
        // Interactive rerun that added nothing and owes nothing → the friendly no-op.
        assert_eq!(
            run_summary(true, 0, 30, 0, 0, 0, 30),
            "up to date (30 sessions, all embedded)"
        );
        // A run that wrote or embedded something → the tally, not "up to date".
        assert_eq!(
            run_summary(true, 2, 28, 57, 57, 0, 30),
            "indexed sessions=2 skipped=28 chunks=57 embedded=57"
        );
        assert_eq!(
            run_summary(true, 0, 30, 0, 120, 0, 30),
            "indexed sessions=0 skipped=30 chunks=0 embedded=120"
        );
        // Stopped early (or no reader at all) → the tally, even with nothing added: work is owed.
        assert_eq!(
            run_summary(false, 0, 30, 0, 0, 0, 30),
            "indexed sessions=0 skipped=30 chunks=0 embedded=0"
        );
        // A rejected unit is never "up to date", and shows in the tally.
        assert_eq!(
            run_summary(true, 0, 29, 0, 0, 1, 30),
            "indexed sessions=0 skipped=29 chunks=0 embedded=0 rejected=1"
        );
    }

    #[test]
    fn estimate_remaining_extrapolates_average_pass_cost() {
        // 10 of 40 passes done in 20s → 2s/pass, 30 left → 60s.
        assert_eq!(
            estimate_remaining(Duration::from_secs(20), 10, 40),
            Duration::from_secs(60)
        );
        // Nothing processed yet, or already done → no estimate.
        assert_eq!(estimate_remaining(Duration::from_secs(5), 0, 40), Duration::ZERO);
        assert_eq!(estimate_remaining(Duration::from_secs(5), 40, 40), Duration::ZERO);
    }

    #[test]
    fn fmt_eta_uses_the_right_unit_at_each_boundary() {
        // < 90s → whole seconds.
        assert_eq!(fmt_eta(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_eta(Duration::from_secs(89)), "89s");
        // The 90s cutoff crosses into minutes; below 90 min it stays there.
        assert!(fmt_eta(Duration::from_secs(90)).contains("min"));
        assert_eq!(fmt_eta(Duration::from_secs(120)), "2 min");
        assert_eq!(fmt_eta(Duration::from_secs(5340)), "89 min");
        // >= 90 min → hours with one decimal.
        assert_eq!(fmt_eta(Duration::from_secs(5400)), "1.5 h");
        assert_eq!(fmt_eta(Duration::from_secs(9000)), "2.5 h");
    }

    #[test]
    fn redact_turns_replaces_secrets_in_block_text() {
        struct Fake(Vec<scan::Finding>);
        impl scan::SecretScanner for Fake {
            fn scan(&self, texts: &[&str]) -> Result<Vec<Vec<scan::Finding>>> {
                Ok(texts.iter().map(|_| self.0.clone()).collect())
            }
        }
        let scanner = Fake(vec![
            scan::Finding {
                detector: "PrivateKey".into(),
                raw: "TOPSECRET".into(),
                decoder: "PLAIN".into(),
            },
            scan::Finding {
                detector: "VirusTotal".into(),
                raw: "cafef00d".into(),
                decoder: "PLAIN".into(),
            },
        ]);
        let mut turns = vec![traces::Turn {
            format: traces::FORMAT_VERSION,
            session_id: "sess".into(),
            cwd: None,
            workdir: "proj".into(),
            turn_uuid: "turn".into(),
            parent_uuid: None,
            seq: 0,
            ts: String::new(),
            role: "user".into(),
            blocks: vec![traces::Block {
                block_type: "text".into(),
                text: "key=TOPSECRET hash=cafef00d".into(),
                tool_name: None,
                tool_use_id: None,
            }],
            source_path: String::new(),
            harness: "claude_code".into(),
        }];
        redact_units(&mut [&mut turns], &scanner, true).unwrap();
        assert_eq!(
            turns[0].blocks[0].text,
            "key=[REDACTED:PrivateKey] hash=[REDACTED:VirusTotal]"
        );
    }

    #[test]
    fn redact_scans_every_unit_in_one_pass_and_only_the_blocks_stored() {
        struct Counting(std::cell::Cell<usize>);
        impl scan::SecretScanner for Counting {
            fn scan(&self, texts: &[&str]) -> Result<Vec<Vec<scan::Finding>>> {
                self.0.set(self.0.get() + 1);
                let hit = scan::Finding {
                    detector: "PrivateKey".into(),
                    raw: "SECRET".into(),
                    decoder: "PLAIN".into(),
                };
                Ok(texts.iter().map(|_| vec![hit.clone()]).collect())
            }
        }
        let block = |bt: &str, text: &str| traces::Block {
            block_type: bt.into(),
            text: text.into(),
            tool_name: None,
            tool_use_id: None,
        };
        let turn = |blocks: Vec<traces::Block>| traces::Turn {
            format: traces::FORMAT_VERSION,
            session_id: "sess".into(),
            cwd: None,
            workdir: "proj".into(),
            turn_uuid: "turn".into(),
            parent_uuid: None,
            seq: 0,
            ts: String::new(),
            role: "user".into(),
            blocks,
            source_path: String::new(),
            harness: "claude_code".into(),
        };
        let mut a = vec![turn(vec![
            block("text", "note SECRET here"),
            block("thinking", "SECRET thought"),
        ])];
        let mut b = vec![turn(vec![block("tool_result", "output SECRET dump")])];
        let scanner = Counting(std::cell::Cell::new(0));
        redact_units(&mut [&mut a, &mut b], &scanner, false).unwrap();
        assert_eq!(scanner.0.get(), 1, "one scanner run for both units");
        assert!(a[0].blocks[0].text.contains("[REDACTED:PrivateKey]"));
        assert_eq!(
            a[0].blocks[1].text, "SECRET thought",
            "a thinking block --no-thinking won't store is not scanned"
        );
        assert!(
            b[0].blocks[0].text.contains("[REDACTED:PrivateKey]"),
            "every tier is stored, so every tier is scanned"
        );
    }

    #[test]
    fn elide_turns_strips_payloads_before_anything_scans_them() {
        struct Recorder(std::cell::RefCell<String>);
        impl scan::SecretScanner for Recorder {
            fn scan(&self, texts: &[&str]) -> Result<Vec<Vec<scan::Finding>>> {
                self.0.borrow_mut().push_str(&texts.join("\n"));
                Ok(texts.iter().map(|_| Vec::new()).collect())
            }
        }
        let mut turns = vec![traces::Turn {
            format: traces::FORMAT_VERSION,
            session_id: "sess".into(),
            cwd: None,
            workdir: "proj".into(),
            turn_uuid: "turn".into(),
            parent_uuid: None,
            seq: 0,
            ts: String::new(),
            role: "tool".into(),
            blocks: vec![traces::Block {
                block_type: "tool_result".into(),
                text: r#"{"image_url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="}"#.into(),
                tool_name: None,
                tool_use_id: None,
            }],
            source_path: String::new(),
            harness: "codex".into(),
        }];
        let scanner = Recorder(std::cell::RefCell::new(String::new()));
        elide_turns(&mut turns);
        redact_units(&mut [&mut turns], &scanner, true).unwrap();
        assert_eq!(
            turns[0].blocks[0].text,
            r#"{"image_url":"data:image/png;base64,[elided]"}"#
        );
        assert!(
            !scanner.0.borrow().contains("iVBORw0KGgo"),
            "the scanner must never see the payload: excising a match inside it would strand the rest"
        );
    }
}
