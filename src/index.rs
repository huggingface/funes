//! The `index` command: read a [`crate::source::TraceSource`] → parse → chunk → embed → write to a
//! local Lance dataset. One generic loop drives every source — a JSONL tree today, new formats by
//! implementing the trait — indexing each of its units in a single append.
//!
//! Incremental on two levels: skip a unit whose stamp (size:mtime) is unchanged *and* which
//! state.json records as already indexed to the run's target tier; and within a re-read unit add
//! only chunks whose id is new — a grown session (the same memory) contributes just its new turns,
//! nothing is re-embedded or deleted.

use crate::chunk::{self, Tier};
use crate::harness::Harness;
use crate::inference::{self, Embedder};
use crate::{dataset, hub, lock, repo, scan, source, trace};
use anyhow::{anyhow, Result};
use arrow_array::types::Float32Type;
use arrow_array::{Array, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema};
use lance::dataset::{Dataset, WriteParams};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const MODEL: &str = "BAAI/bge-small-en-v1.5";
pub const DIM: i32 = 384;
const EMBED_BATCH: usize = 256;

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

/// The table schema (column order is load-bearing for Lance).
pub(crate) fn schema() -> Arc<Schema> {
    let utf8 = |name: &str| Field::new(name, DataType::Utf8, true);
    let i64f = |name: &str| Field::new(name, DataType::Int64, true);
    Arc::new(Schema::new_with_metadata(
        vec![
            utf8("id"),
            utf8("text"),
            utf8("session_id"),
            utf8("workdir"),
            utf8("turn_uuid"),
            utf8("parent_uuid"),
            i64f("seq"),
            utf8("ts"),
            utf8("role"),
            utf8("block_type"),
            utf8("tool_name"),
            utf8("source_path"),
            i64f("block_idx"),
            i64f("split_idx"),
            Field::new(
                "vector",
                DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), DIM),
                true,
            ),
            // After `vector`: `add_columns` appends a migrated column at the end, so a
            // freshly-built memory must match that order (the tripwire test pins it). `harness`
            // came first, then `repo` — each appended in turn.
            utf8("harness"),
            utf8("repo"),
        ],
        HashMap::from([("embedding_model".to_string(), MODEL.to_string())]),
    ))
}

pub(crate) fn build_batch(chunks: &[chunk::Chunk], vectors: &[Vec<f32>]) -> Result<RecordBatch> {
    let s = |f: &dyn Fn(&chunk::Chunk) -> Option<String>| -> StringArray { chunks.iter().map(f).collect() };
    let i = |f: &dyn Fn(&chunk::Chunk) -> i64| -> Int64Array { chunks.iter().map(|c| Some(f(c))).collect() };
    let vector = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        vectors
            .iter()
            .map(|v| Some(v.iter().map(|&x| Some(x)).collect::<Vec<_>>())),
        DIM,
    );
    Ok(RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(s(&|c| Some(c.id.clone()))),
            Arc::new(s(&|c| Some(c.text.clone()))),
            Arc::new(s(&|c| Some(c.session_id.clone()))),
            Arc::new(s(&|c| Some(c.workdir.clone()))),
            Arc::new(s(&|c| Some(c.turn_uuid.clone()))),
            Arc::new(s(&|c| c.parent_uuid.clone())),
            Arc::new(i(&|c| c.seq)),
            Arc::new(s(&|c| Some(c.ts.clone()))),
            Arc::new(s(&|c| Some(c.role.clone()))),
            Arc::new(s(&|c| Some(c.block_type.clone()))),
            Arc::new(s(&|c| c.tool_name.clone())),
            Arc::new(s(&|c| Some(c.source_path.clone()))),
            Arc::new(i(&|c| c.block_idx)),
            Arc::new(i(&|c| c.split_idx)),
            Arc::new(vector),
            Arc::new(s(&|c| Some(c.harness.clone()))),
            Arc::new(s(&|c| Some(c.repo.clone()))),
        ],
    )?)
}

/// Embed `texts` in batches of [`EMBED_BATCH`], calling `on_batch(embedded_so_far)` after each so a
/// caller can report progress (or pass a no-op).
pub(crate) fn embed_batched(
    embedder: &mut dyn Embedder,
    texts: &[&str],
    mut on_batch: impl FnMut(usize),
) -> Result<Vec<Vec<f32>>> {
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for group in texts.chunks(EMBED_BATCH) {
        vectors.extend(embedder.embed(group)?);
        on_batch(vectors.len());
    }
    Ok(vectors)
}

/// Every chunk id already stored. Re-indexing keeps only the chunks whose id isn't here, so a grown
/// session (the same memory) contributes just its new turns — nothing is re-embedded or deleted. (A
/// rewritten turn arrives under new ids, i.e. as another memory.) Chunk ids are global, so one
/// unfiltered scan dedups any unit, whether it holds one session or thousands.
async fn stored_ids(ds: &Dataset) -> Result<HashSet<String>> {
    let batches = dataset::scan_rows(ds, &["id"], None, None).await?;
    let mut ids = HashSet::new();
    for batch in batches {
        if let Some(col) = batch
            .column_by_name("id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        {
            for i in 0..batch.num_rows() {
                ids.insert(col.value(i).to_string());
            }
        }
    }
    Ok(ids)
}

/// Redact secrets from a session's turns *before* chunking — so a long key that chunking would
/// split across pieces is whole when scanned, and never reaches the embedding, the local memory, or
/// (via push) the Hub. Scans exactly the blocks the pass will store ([`chunk::block_selected`]), so
/// deferred tiers aren't scanned now and a tier-major backfill doesn't re-scan each session per
/// tier. Best-effort: removes a secret whose value byte-matches the stored text (the common case,
/// real newlines); anything that resists is caught downstream by the fail-closed push gate.
/// Reports to stderr what it removed.
fn redact_turns(
    turns: &mut [trace::Turn],
    scanner: &dyn scan::SecretScanner,
    tiers: &[Tier],
    include_thinking: bool,
) -> Result<()> {
    let removed: Vec<String> = {
        let mut blocks: Vec<&mut trace::Block> = turns
            .iter_mut()
            .flat_map(|t| t.blocks.iter_mut())
            .filter(|b| chunk::block_selected(&b.block_type, tiers, include_thinking))
            .collect();
        if blocks.is_empty() {
            return Ok(());
        }
        let per_block = {
            let texts: Vec<&str> = blocks.iter().map(|b| b.text.as_str()).collect();
            scan::scan_blocks(&texts, scanner)?
        };
        let mut removed = Vec::new();
        for (b, findings) in blocks.iter_mut().zip(&per_block) {
            let r = scan::excise(&b.text, findings);
            removed.extend(r.removed_detectors);
            b.text = r.text;
        }
        removed
    };
    if removed.is_empty() {
        return Ok(());
    }
    let sid = turns.first().map(|t| t.session_id.as_str()).unwrap_or("?");
    eprintln!(
        "    redacted {} secret(s) in {sid}: {}",
        removed.len(),
        scan::summary(removed.iter().map(String::as_str))
    );
    Ok(())
}

/// A unit's distinct-session count and a log label: `"<sid> (<workdir>)"` for a single session (a
/// JSONL file), `"<n> sessions"` for a bulk unit (many sessions in one artifact), and the unit's `key` (its
/// path) when it has no turns at all. The borrow of `turns` is confined here so callers keep it mutable.
fn unit_summary(turns: &[trace::Turn], key: &str) -> (u64, String) {
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

/// What `state.json` records per unit: the change-stamp last seen and the highest [`Tier`] it has
/// been indexed to.
#[derive(Serialize, Deserialize, Clone)]
struct UnitState {
    sig: String,
    level: Tier,
}

/// Whether a recorded unit is current for a run targeting `target`: its stamp still matches and it
/// has already reached at least that tier. A lower recorded tier still needs a pass.
fn unit_current(entry: Option<&UnitState>, sig: &str, target: Tier) -> bool {
    entry.is_some_and(|e| e.sig == sig && e.level >= target)
}

/// A set-up indexer: it holds the memory lock, embedder, dataset, redaction scanner, and incremental
/// state, so a caller can index units in whatever batches it likes — one at a time to check the
/// clock between them, or all at once — without reloading the model. Build the indexes once at the
/// end with [`Indexer::finalize`].
struct Indexer {
    uri: String,
    ds: Option<Dataset>,
    /// [`stored_ids`] at open plus everything appended this run — the dedup baseline for new chunks.
    existing: HashSet<String>,
    embedder: Box<dyn Embedder>,
    scanner: Option<scan::Trufflehog>,
    include_thinking: bool,
    /// cwd → resolved `repo` value, so each distinct checkout runs `git` once across the run.
    repo_cache: HashMap<String, String>,
    state: HashMap<String, UnitState>,
    state_path: PathBuf,
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
        let state = if first_index {
            HashMap::new()
        } else {
            std::fs::read_to_string(&state_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        };

        let embedder: Box<dyn Embedder> = inference::embedder()?;
        // Best-effort secret redaction: if the scanner isn't installed, indexing continues
        // unredacted — the push gate still scans, fail-closed, before any upload, so a secret can't
        // reach the Hub.
        let scanner = match scan::Trufflehog::find() {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("note: secret redaction disabled — {e}");
                None
            }
        };

        let existing = match &ds {
            Some(d) => stored_ids(d).await?,
            None => HashSet::new(),
        };

        // Each source orders its own units (recency-desc, subagents last); tag each with its source.
        let mut units = Vec::new();
        for (i, src) in sources.iter().enumerate() {
            for unit in src.units()? {
                units.push((i, unit));
            }
        }

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
        })
    }

    /// Number of units this run will consider.
    fn unit_count(&self) -> usize {
        self.units.len()
    }

    /// Units still owing work at `tier` — a pure state + signature check, no session read, so a
    /// caller can plan and estimate a run before touching anything. A signature-less (bulk) unit
    /// always counts as pending, as [`Indexer::index_unit`] never skips it.
    fn pending(&self, tier: Tier) -> Vec<usize> {
        (0..self.units.len())
            .filter(|&i| {
                let unit = &self.units[i].1;
                match &unit.signature {
                    Some(sig) => !unit_current(self.state.get(&unit.key), sig, tier),
                    None => true,
                }
            })
            .collect()
    }

    /// Index unit `i` at `tiers` — the one primitive a caller loops over, choosing the tiers and
    /// when to stop. Skips a unit already indexed to the top of `tiers`; a signature-less (bulk)
    /// unit is never skipped — it is re-read every run, and its chunk-id dedup makes that a no-op.
    /// Otherwise reads the unit, redacts, chunks those tiers, keeps only chunks whose id isn't
    /// already stored, embeds them in a single append, and stamps the unit's state. Returns the
    /// new-chunk count (0 when skipped, unreadable, or already indexed).
    ///
    /// `progress` is the caller's label for the per-unit output line (e.g. `"text [1/3]"`) — the
    /// caller owns the counter because only it knows the iteration (which tier, how many owed).
    ///
    /// Add-only-new: a grown session is the same memory — embed and add only its new turns, never
    /// re-embedding or deleting what's unchanged. (A rewritten turn lands under new ids.) A unit's
    /// turns are written in one append, so a bulk source (many sessions in one unit) stays a single
    /// Lance fragment rather than one per session.
    async fn index_unit(&mut self, i: usize, tiers: &[Tier], progress: &str) -> Result<u64> {
        let target = *tiers.iter().max().expect("a pass covers at least one tier");
        let (src_i, key, sig) = {
            let (si, unit) = &self.units[i];
            (*si, unit.key.clone(), unit.signature.clone())
        };

        if let Some(sig) = &sig {
            if unit_current(self.state.get(&key), sig, target) {
                self.n_skipped += 1;
                return Ok(0);
            }
        }

        // Best-effort sources retry a failed read next run (no state recorded); a fatal source
        // aborts rather than silently dropping data.
        let mut turns = {
            let src = &self.sources[src_i];
            match src.read(&self.units[i].1) {
                Ok(t) => t,
                Err(e) if !src.fatal_on_read_error() => {
                    eprintln!("{progress} {key} — read failed, skipping: {e}");
                    return Ok(0);
                }
                Err(e) => return Err(e),
            }
        };

        let (sessions, label) = unit_summary(&turns, &key);
        if let Some(scanner) = &self.scanner {
            redact_turns(&mut turns, scanner, tiers, self.include_thinking)?;
        }
        let mut chunks = chunk::chunks_from_turns(&turns, tiers, self.include_thinking);
        let repo = self.repo_for(&key);
        if !repo.is_empty() {
            for c in &mut chunks {
                c.repo.clone_from(&repo);
            }
        }
        let total_chunks = chunks.len();
        let added = if total_chunks == 0 {
            eprintln!("{progress} {label} — no indexable content");
            0
        } else {
            let new_chunks: Vec<chunk::Chunk> = chunks.into_iter().filter(|c| !self.existing.contains(&c.id)).collect();
            if new_chunks.is_empty() {
                eprintln!("{progress} {label} — {total_chunks} chunks, all already indexed");
                0
            } else {
                eprintln!("{progress} {label} — {} new of {total_chunks} chunks", new_chunks.len());
                let n = self.embed_and_write(&new_chunks).await?;
                self.existing.extend(new_chunks.into_iter().map(|c| c.id));
                n
            }
        };

        // Record state only for signed units, even when they produced no chunks ("remembered when
        // empty"), and persist after each so an interrupted run is resumable.
        if let Some(sig) = &sig {
            self.state.insert(
                key.clone(),
                UnitState {
                    sig: sig.clone(),
                    level: target,
                },
            );
            std::fs::write(&self.state_path, serde_json::to_string_pretty(&self.state)?)?;
        }
        // Count a unit's sessions once per run — later tier passes over it only add chunks.
        if self.counted.insert(i) {
            self.n_sessions += sessions;
        }
        self.n_chunks += added;
        Ok(added)
    }

    /// The session's repo(s) for the unit at `key`, resolved from its transcript's cwd and cached
    /// per cwd so each distinct checkout runs `git` once across the run. Empty for a non-transcript
    /// unit (a parquet shard) or a checkout that can't be resolved (gone, not a git repo).
    fn repo_for(&mut self, key: &str) -> String {
        let Some(cwd) = repo::cwd_of_transcript(Path::new(key)) else {
            return String::new();
        };
        self.repo_cache
            .entry(cwd.clone())
            .or_insert_with(|| repo::of_cwd(&cwd))
            .clone()
    }

    /// Embed `new_chunks` and add them — appending to the dataset, or creating it at `uri` on the
    /// first write. Returns the count.
    async fn embed_and_write(&mut self, new_chunks: &[chunk::Chunk]) -> Result<u64> {
        let n = new_chunks.len();
        let texts: Vec<&str> = new_chunks.iter().map(|c| c.text.as_str()).collect();
        let t0 = Instant::now();
        let vectors = embed_batched(self.embedder.as_mut(), &texts, |done| {
            let secs = t0.elapsed().as_secs_f64().max(0.001);
            eprint!("\r    embedded {done}/{n}  ({:.0}/s)   ", done as f64 / secs);
            let _ = std::io::stderr().flush();
        })?;
        eprintln!(
            "\r    embedded {n} chunks in {:.1}s          ",
            t0.elapsed().as_secs_f64()
        );

        let batch = build_batch(new_chunks, &vectors)?;
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
        Ok(n as u64)
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
                self.units.len(),
            )
        );
        Ok(())
    }
}

/// The run summary line. An interactive rerun that added nothing — and left nothing owed — gets a
/// friendly "up to date" instead of a zero-count tally; an automated run (no reader) or any run
/// that indexed or still owes something gets the tally.
fn run_summary(done: bool, sessions: u64, skipped: u64, chunks: u64, units: usize) -> String {
    if done && chunks == 0 {
        format!("up to date ({units} sessions, all tiers)")
    } else {
        format!("indexed sessions={sessions} skipped={skipped} chunks={chunks}")
    }
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
        .collect();
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

/// The wall-clock budget a budgeted run gives itself: it stops at the first whole-session boundary
/// past this. Deeper tiers and older sessions backfill on later runs.
const INDEX_BUDGET_SECS: u64 = 60;

/// What a budgeted run does when the budget expires with passes still owed.
#[derive(Clone, Copy)]
enum Finish {
    /// Stop at the session boundary — later runs catch up.
    Stop,
    /// Offer to finish the rest now (interactive only; otherwise stop).
    Ask,
    /// Finish everything without asking (`--yes`).
    All,
}

/// Build/update the local index from harness session roots, budgeted and tier-major: text across
/// every session first, then tool_use, then tool_result, stopping at the first whole-session
/// boundary past the budget. The no-path `funes index` — the per-turn hook advances the backfill
/// one bounded step per run; an interactive run offers to finish the rest; `yes` finishes it
/// without asking.
pub async fn run_index_budgeted(
    roots: &[(PathBuf, Option<Harness>)],
    no_thinking: bool,
    max_sessions: Option<usize>,
    yes: bool,
) -> Result<()> {
    let sources = roots
        .iter()
        .map(|(path, harness)| source::open_with_harness(path, max_sessions, *harness))
        .collect();
    let finish = if yes { Finish::All } else { Finish::Ask };
    run_budgeted(sources, no_thinking, finish).await
}

/// The `funes add` first index: the budgeted drain with no finish prompt — the add flow already
/// asked, and the per-turn drip owns whatever the budget defers. Tier-major order spends the
/// budget on text (decisions, rationale) first, so recall works in about a minute; a small history
/// simply finishes whole.
pub async fn run_index_seed(root: &Path, harness: Harness) -> Result<()> {
    let sources = vec![source::open_with_harness(root, None, Some(harness))];
    run_budgeted(sources, false, Finish::Stop).await
}

/// Drive `sources` tier-major — every owed unit at text, then at tool_use, then at tool_result —
/// checking the budget after each whole-session pass; `finish` says what to do when it expires
/// with work left. The owed passes are computed upfront from state alone (no reading), so the plan
/// and the ETA reflect what this run actually owes.
async fn run_budgeted(sources: Vec<Box<dyn source::TraceSource>>, no_thinking: bool, finish: Finish) -> Result<()> {
    let mut idx = Indexer::open(sources, no_thinking).await?;

    let owed: Vec<(Tier, Vec<usize>)> = Tier::ALL
        .iter()
        .map(|&t| (t, idx.pending(t)))
        .filter(|(_, units)| !units.is_empty())
        .collect();
    if owed.is_empty() {
        return idx.finalize().await;
    }
    let report = owed
        .iter()
        .map(|(t, u)| format!("{}: {}", t.label(), u.len()))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("to index — {report}");

    let start = Instant::now();
    let budget = Duration::from_secs(INDEX_BUDGET_SECS);
    let passes: usize = owed.iter().map(|(_, u)| u.len()).sum();
    let mut done = 0usize;
    let mut capped = true;
    'tiers: for (tier, units) in &owed {
        for (j, &i) in units.iter().enumerate() {
            let progress = format!("{} [{}/{}]", tier.label(), j + 1, units.len());
            idx.index_unit(i, &[*tier], &progress).await?;
            done += 1;
            if capped && start.elapsed() >= budget {
                let go_on = match finish {
                    Finish::All => true,
                    Finish::Ask if idx.interactive => {
                        confirm_continue(estimate_remaining(start.elapsed(), done, passes))
                    }
                    _ => false,
                };
                if !go_on {
                    eprintln!(
                        "{} pass(es) left — per-turn indexing (or a `funes index` rerun) picks them up",
                        passes - done
                    );
                    idx.work_remaining = true;
                    break 'tiers;
                }
                capped = false; // finish the rest now
            }
        }
    }
    idx.finalize().await
}

/// Index a set of already-opened sources fully — every tier of every unit, one read each — sharing
/// one embedder, `state.json`, and dataset handle across them (state keyed by absolute path /
/// `hf://…` shard, so incremental works cross-source). On a first interactive index it estimates
/// the run after the first session and asks before the long haul.
async fn index_sources(sources: Vec<Box<dyn source::TraceSource>>, no_thinking: bool, yes: bool) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let mut indexer = Indexer::open(sources, no_thinking).await?;
    let total = indexer.unit_count();

    // Per-source tally. This run indexes every tier, so a unit counts as cached only once it has
    // reached the highest.
    let target = *Tier::ALL.iter().max().expect("Tier::ALL is non-empty");
    for (si, src) in indexer.sources.iter().enumerate() {
        let units = indexer.units.iter().filter(|(i, _)| *i == si);
        let (mut n, mut cached) = (0usize, 0usize);
        for (_, u) in units {
            n += 1;
            if u.signature
                .as_ref()
                .is_some_and(|s| unit_current(indexer.state.get(&u.key), s, target))
            {
                cached += 1;
            }
        }
        eprintln!("{} — {} to index, {cached} cached", src.describe(), n - cached);
    }

    // First interactive index: after the first session lands, estimate the whole run from its time
    // and — if it looks long — ask whether to continue or bail and re-run with --limit.
    let mut probe_pending = indexer.first_index && !yes && interactive;

    for i in 0..total {
        // Time from before the read so a first-index estimate covers parse + I/O, not just embedding.
        let t_unit = Instant::now();
        let progress = format!("[{}/{}]", i + 1, total);
        let added = indexer.index_unit(i, &Tier::ALL, &progress).await?;

        // Estimate off the first session that actually embedded, and ask before a long haul.
        if probe_pending && added > 0 {
            probe_pending = false;
            let est = t_unit.elapsed().mul_f64(total as f64);
            if est >= Duration::from_secs(FIRST_INDEX_PROMPT_SECS) && !confirm_full_index(total, est) {
                eprintln!(
                    "stopped after 1 session (kept — the index is resumable). Re-run \
                     `funes index --limit M` for the most recent M, or `funes index` to do all."
                );
                indexer.work_remaining = true;
                break;
            }
        }
    }

    indexer.finalize().await
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

/// Prompt before a long first index (interactive only): continue all, or bail and re-run with a
/// `--limit`. Returns whether to proceed.
fn confirm_full_index(total: usize, est: Duration) -> bool {
    confirm(
        &format!(
            "indexing all {total} sessions is estimated at ~{} (rough, from one session). Continue? [y/N]  \
             (or re-run with `--limit M` for the most recent M)",
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

    #[test]
    fn unit_current_needs_matching_sig_and_reached_target_tier() {
        let l1 = UnitState {
            sig: "10:20".into(),
            level: Tier::Text,
        };
        // Signature mismatch is never current, whatever the tier.
        assert!(!unit_current(Some(&l1), "99:99", Tier::Text));
        // Sig matches and the recorded tier meets the target → current.
        assert!(unit_current(Some(&l1), "10:20", Tier::Text));
        // Sig matches but a higher tier is targeted → NOT current; L2/L3 still owe a pass.
        assert!(!unit_current(Some(&l1), "10:20", Tier::ToolUse));
        assert!(!unit_current(Some(&l1), "10:20", Tier::ToolResult));
        // A fully-indexed unit satisfies every target.
        let full = UnitState {
            sig: "10:20".into(),
            level: Tier::ToolResult,
        };
        assert!(unit_current(Some(&full), "10:20", Tier::Text));
        assert!(unit_current(Some(&full), "10:20", Tier::ToolResult));
        // No record → not current.
        assert!(!unit_current(None, "10:20", Tier::Text));
    }

    #[test]
    fn run_summary_says_up_to_date_only_on_a_done_no_op() {
        // Interactive rerun that added nothing and owes nothing → the friendly no-op.
        assert_eq!(run_summary(true, 0, 30, 0, 30), "up to date (30 sessions, all tiers)");
        // A run that indexed something → the tally, not "up to date".
        assert_eq!(
            run_summary(true, 2, 28, 57, 30),
            "indexed sessions=2 skipped=28 chunks=57"
        );
        // Stopped early (or no reader at all) → the tally, even with nothing added: work is owed.
        assert_eq!(
            run_summary(false, 0, 30, 0, 30),
            "indexed sessions=0 skipped=30 chunks=0"
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
    fn schema_column_order_is_load_bearing() {
        // Column order must match build_batch's array order exactly, or Lance writes the
        // wrong column. Pin it so a reorder can't slip through.
        let s = schema();
        let names: Vec<&str> = s.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(
            names,
            vec![
                "id",
                "text",
                "session_id",
                "workdir",
                "turn_uuid",
                "parent_uuid",
                "seq",
                "ts",
                "role",
                "block_type",
                "tool_name",
                "source_path",
                "block_idx",
                "split_idx",
                "vector",
                "harness",
                "repo",
            ]
        );
    }

    #[test]
    fn redact_turns_replaces_secrets_in_block_text() {
        struct Fake(Vec<scan::Finding>);
        impl scan::SecretScanner for Fake {
            fn scan(&self, _blob: &str) -> Result<Vec<scan::Finding>> {
                Ok(self.0.clone())
            }
        }
        let scanner = Fake(vec![
            scan::Finding {
                detector: "PrivateKey".into(),
                raw: "TOPSECRET".into(),
                line: None,
                decoder: "PLAIN".into(),
            },
            scan::Finding {
                detector: "VirusTotal".into(),
                raw: "cafef00d".into(),
                line: None,
                decoder: "PLAIN".into(),
            },
        ]);
        let mut turns = vec![trace::Turn {
            session_id: "sess".into(),
            workdir: "proj".into(),
            turn_uuid: "turn".into(),
            parent_uuid: None,
            seq: 0,
            ts: String::new(),
            role: "user".into(),
            blocks: vec![trace::Block {
                block_type: "text".into(),
                text: "key=TOPSECRET hash=cafef00d".into(),
                tool_name: None,
                tool_use_id: None,
            }],
            source_path: String::new(),
            harness: "claude_code".into(),
        }];
        redact_turns(&mut turns, &scanner, &chunk::Tier::ALL, true).unwrap();
        assert_eq!(
            turns[0].blocks[0].text,
            "key=[REDACTED:PrivateKey] hash=[REDACTED:VirusTotal]"
        );
    }

    #[test]
    fn redact_only_scans_the_pass_tier_blocks() {
        struct Fake;
        impl scan::SecretScanner for Fake {
            fn scan(&self, _blob: &str) -> Result<Vec<scan::Finding>> {
                Ok(vec![scan::Finding {
                    detector: "PrivateKey".into(),
                    raw: "SECRET".into(),
                    line: None,
                    decoder: "PLAIN".into(),
                }])
            }
        }
        let block = |bt: &str, text: &str| trace::Block {
            block_type: bt.into(),
            text: text.into(),
            tool_name: None,
            tool_use_id: None,
        };
        let mut turns = vec![trace::Turn {
            session_id: "sess".into(),
            workdir: "proj".into(),
            turn_uuid: "turn".into(),
            parent_uuid: None,
            seq: 0,
            ts: String::new(),
            role: "user".into(),
            blocks: vec![
                block("text", "note SECRET here"),
                block("tool_result", "output SECRET dump"),
            ],
            source_path: String::new(),
            harness: "claude_code".into(),
        }];
        // A text-only pass redacts the text block but leaves the tool_result it won't store untouched.
        redact_turns(&mut turns, &Fake, &[chunk::Tier::Text], true).unwrap();
        assert!(
            turns[0].blocks[0].text.contains("[REDACTED:PrivateKey]"),
            "text block redacted"
        );
        assert_eq!(
            turns[0].blocks[1].text, "output SECRET dump",
            "unindexed tool_result untouched"
        );
    }
}
