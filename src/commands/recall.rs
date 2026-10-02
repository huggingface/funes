//! The read surface: `recall`, `get`, `status` over the existing index.
//! Recall pipeline: hybrid (vector + BM25, fused by reciprocal rank) → cross-encoder rerank →
//! neighbor expansion. `recall`/`get` return results rendered in the agent format;
//! `recall_hits`/`get_turns` return the structured results for other renderings (see `render`).

use crate::chunk;
use crate::inference::{self, Embedder, Reranker};
use crate::memory::dataset;
use crate::memory::{Memory, MemoryState};
use anyhow::{anyhow, bail, Context, Result};
use arrow_array::{Float32Array, Int64Array, RecordBatch, StringArray, UInt64Array};
use chrono::{DateTime, NaiveDate, Utc};
use futures::TryStreamExt;
use lance::dataset::{Dataset, ROW_ID};
use lance_index::scalar::FullTextSearchQuery;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use tokio::sync::{Mutex, OnceCell};

/// Columns a [`Hit`] needs from a search scan.
const HIT_COLS: &[&str] = &[
    "id",
    "text",
    "session_id",
    "workdir",
    "turn_uuid",
    "ts",
    "block_type",
    "seq",
    "harness",
];

/// Scanned row for neighbor expansion: (session_id, seq, turn_uuid, block_idx, split_idx, role, block_type, text).
type NeighborRow = (String, i64, String, i64, i64, String, String, String);

/// Scanned row for `get`: (seq, turn_uuid, ts, role, block_idx, split_idx, text).
type TurnRow = (i64, String, String, String, i64, i64, String);

/// One adjacent chunk pulled in to give a hit some surrounding context.
pub struct Neighbor {
    pub seq: i64,
    pub role: String,
    pub block_type: String,
    pub text: String,
}

/// One candidate row carried from retrieval through rerank to display.
pub struct Hit {
    pub id: String,
    pub text: String,
    pub session_id: String,
    pub workdir: String,
    pub turn_uuid: String,
    pub seq: i64,
    pub ts: String,
    pub block_type: String,
    pub harness: String,
    pub neighbors: Vec<Neighbor>,
    pub memory: String,
    pub fused: f32,
}

/// Matching blocks `scan` lists before it stops. What the cap dropped is always reported.
const SCAN_HIT_CAP: usize = 200;

/// Characters of surrounding text a `scan` hit shows on each side of its match.
pub const DEFAULT_CONTEXT: usize = 100;

/// Why a `scan` listing stopped short, and what the caller can do about it.
pub enum ScanCut {
    /// Hits remain from this turn onward; a continuing scan starts exactly there. The page was cut
    /// back to a turn boundary so that resuming neither repeats a hit nor skips one.
    Resume(i64),
    /// This one turn holds more matches than the cap by itself, so paging cannot step over it.
    Crowded(i64),
}

/// One block of a session carrying a `scan` needle.
pub struct ScanHit {
    pub turn_uuid: String,
    pub ts: String,
    pub block_type: String,
    pub seq: i64,
    /// Byte offset of the match within `text`.
    pub at: usize,
    /// Chars the match spans — its own length, which case folding leaves unchanged.
    pub len: usize,
    /// The whole reassembled block, for the caller to excerpt around `at`.
    pub text: String,
}

/// What a `scan` needle found in one session, or in the window of it that was scanned.
pub struct ScanResult {
    pub needle: String,
    pub session_id: String,
    /// Matching blocks in reading order, capped at [`SCAN_HIT_CAP`].
    pub hits: Vec<ScanHit>,
    /// Matching blocks past the cap, absent from `hits`.
    pub dropped: usize,
    /// Why the listing stopped, when it did.
    pub cut: Option<ScanCut>,
    /// The seq window scanned, when one was asked for. A zero over a window clears the window, not
    /// the session, so the window rides with the result.
    pub from: Option<i64>,
    pub to: Option<i64>,
}

/// One session in a memory's listing: when and where it started, how much it holds, and the prompt
/// it opened with.
pub struct Session {
    pub session_id: String,
    /// First timestamp in the session.
    pub ts: String,
    pub workdir: String,
    pub harness: String,
    /// The session's source repo(s) as `owner/name`, space-joined; empty when unresolvable.
    pub repo: String,
    /// Distinct turns, not rows: chunking is an indexing artifact, and a turn is what `get` reads,
    /// so this counts (seq, turn_uuid) pairs. A uuid alone would undercount — a compacted
    /// transcript replays turns under a uuid it has already used.
    pub turns: usize,
    /// The opening real prompt, scaffolding skipped — what the session was for, in one line.
    pub first_prompt: String,
}

impl Session {
    /// The `YYYY-MM-DD` the session started.
    pub fn date(&self) -> &str {
        day(&self.ts)
    }

    /// Best available provenance: the repo when the checkout resolved, else the working directory.
    pub fn origin(&self) -> &str {
        self.repo.split_whitespace().next().unwrap_or(&self.workdir)
    }
}

/// Sessions a listing renders when no limit is given.
pub const SESSIONS_LIMIT: usize = 50;

/// The most rows one listing will render, whatever `limit` asks for. Past this the reply is larger
/// than a tool result can carry; `offset` walks the rest.
pub const SESSIONS_LIMIT_MAX: usize = 200;

/// One reassembled turn from `get`: its blocks in order, splits stitched back together.
pub struct Turn {
    pub seq: i64,
    pub turn_uuid: String,
    pub ts: String,
    pub role: String,
    pub blocks: Vec<String>,
}

fn scol<'a>(b: &'a RecordBatch, name: &str) -> Option<&'a StringArray> {
    b.column_by_name(name)?.as_any().downcast_ref::<StringArray>()
}

fn icol<'a>(b: &'a RecordBatch, name: &str) -> Option<&'a Int64Array> {
    b.column_by_name(name)?.as_any().downcast_ref::<Int64Array>()
}

fn sval(a: Option<&StringArray>, i: usize) -> String {
    a.map(|c| c.value(i).to_string()).unwrap_or_default()
}

fn ival(a: Option<&Int64Array>, i: usize) -> i64 {
    a.map(|c| c.value(i)).unwrap_or(0)
}

/// Escape a value for inlining into a Lance SQL filter string.
pub(crate) fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// The day `ts` falls on: a `ts` is RFC 3339 in UTC, so its first ten characters.
fn day(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
}

/// A bound as the `YYYY-MM-DD` it names, in that exact spelling: a bound is also compared as
/// text, which `2026-9-18` would not survive.
fn parse_day(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .filter(|d| d.to_string() == s)
        .ok_or_else(|| anyhow!("{s:?} is not a date: expected YYYY-MM-DD"))
}

/// The days from `since` through `until`, each an optional `YYYY-MM-DD`, both inclusive.
#[derive(Default)]
struct DayRange<'a> {
    since: Option<&'a str>,
    until: Option<&'a str>,
}

impl DayRange<'_> {
    /// Whether the day `ts` falls on is in the range.
    fn holds(&self, ts: &str) -> bool {
        let d = day(ts);
        self.since.is_none_or(|s| d >= s) && self.until.is_none_or(|u| d <= u)
    }

    /// The error a bound that is not a date deserves, else nothing.
    fn check(&self) -> Result<()> {
        for bound in [self.since, self.until].into_iter().flatten() {
            parse_day(bound)?;
        }
        Ok(())
    }

    /// The same test as Lance filter clauses on `ts`: `ts >= since` and `ts < the day after until`.
    /// A filter cannot take a substring of `ts`, so here the bounds must parse.
    fn clauses(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if let Some(since) = self.since {
            out.push(format!("ts >= '{}'", parse_day(since)?));
        }
        if let Some(until) = self.until {
            let next = parse_day(until)?.succ_opt().context("the day after is out of range")?;
            out.push(format!("ts < '{next}'"));
        }
        Ok(out)
    }
}

/// `block_type = '…' AND harness IN ('…') AND ts >= '…'` over whichever filters are set, else
/// None.
fn build_where(block_type: Option<&str>, harness: &[String], days: &DayRange) -> Result<Option<String>> {
    let mut clauses = Vec::new();
    if let Some(bt) = block_type {
        clauses.push(format!("block_type = '{}'", esc(bt)));
    }
    match harness {
        [] => {}
        [h] => clauses.push(format!("harness = '{}'", esc(h))),
        many => {
            let list: Vec<String> = many.iter().map(|h| format!("'{}'", esc(h))).collect();
            clauses.push(format!("harness IN ({})", list.join(", ")));
        }
    }
    clauses.extend(days.clauses()?);
    Ok(if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" AND "))
    })
}

/// The stored `harness` facets a `--harness` value names. `claude` and `claude_code` name each
/// other: Claude's turns were stored under the second before its integration wrote the first, and
/// a memory may hold both. Any other value matches as given.
fn harness_spellings(h: String) -> Vec<String> {
    match h.as_str() {
        "claude" | "claude_code" => vec!["claude_code".to_string(), "claude".to_string()],
        _ => vec![h],
    }
}

/// A dataset opened for reading.
struct Read {
    ds: Dataset,
    /// A degradation note to prepend to the command's output (e.g. the remote was unreachable);
    /// `None` when the requested memory opened normally.
    note: Option<String>,
    /// Label of the memory the dataset actually came from (the requested one, or the local memory
    /// after an offline degrade).
    memory_label: Option<String>,
}

/// What a read verb does about the memory's state: query it, degrade to the local index, or point a
/// fresh install at onboarding. The states a verb can't act on are already errors by the time this
/// is built.
// A transient return value, never stored en masse, so the `Ready(Dataset)`/unit size gap is fine —
// boxing would only add indirection.
#[allow(clippy::large_enum_variant)]
enum ReadOutcome {
    /// Opened and ready to query.
    Ready(Dataset),
    /// The remote is unreachable — recall from the local index instead.
    Offline,
    /// The default local memory has no index yet.
    NoIndex,
}

/// Resolve a memory for reading: ask [`Memory::state`] what it is, then decide what a read verb
/// does about it. Only two states are actionable — degrade when offline, onboard when the default
/// local memory is unbuilt; the rest are errors carrying the domain's message.
async fn open_for_read(memory: &Memory) -> Result<ReadOutcome> {
    match memory.state().await? {
        MemoryState::Ready(ds) => Ok(ReadOutcome::Ready(ds)),
        MemoryState::Offline => Ok(ReadOutcome::Offline),
        MemoryState::Missing => Err(memory.missing_error()),
        MemoryState::Unauthorized => Err(memory.unauthorized_error()),
        // Nothing there yet. The default local memory is a fresh install (onboarding, below); an
        // explicit path or a never-pushed remote says so instead.
        MemoryState::Empty if memory.is_default_local() => Ok(ReadOutcome::NoIndex),
        MemoryState::Empty => Err(memory.empty_error()),
    }
}

/// A caller that named a memory must never silently read a different one: surfaces the errors
/// [`open_for_read`] would, and refuses the offline degrade the read verbs apply.
pub async fn check_readable(memory: &Memory) -> Result<()> {
    match open_for_read(memory).await? {
        ReadOutcome::Ready(_) => Ok(()),
        ReadOutcome::NoIndex => Err(no_index_error()),
        ReadOutcome::Offline => Err(anyhow!(
            "{} is unreachable right now — try again once you're back online",
            memory.label()
        )),
    }
}

/// Open a memory for reading, applying the fallback [`open_for_read`] leaves to the caller: an
/// unreachable remote degrades to the local index, so recall keeps working offline. A missing or
/// empty remote, and a fresh install with no local index, surface as clear errors.
async fn open_read(memory: &Memory) -> Result<Read> {
    match open_for_read(memory).await? {
        ReadOutcome::Ready(ds) => Ok(Read {
            ds,
            note: None,
            memory_label: Some(memory.label()),
        }),
        ReadOutcome::Offline => degrade_offline(&memory.label()).await,
        ReadOutcome::NoIndex => Err(no_index_error()),
    }
}

/// The error a read verb returns when the default local memory has no index yet — points at the
/// onboarding command instead of leaking lance's internals.
fn no_index_error() -> anyhow::Error {
    anyhow!("no index yet — run `funes add <agent>` to build one (or `funes index`), then recall your own history")
}

/// An unreachable remote degrades to the local index, carrying a note that explains what happened;
/// with no local index either there's nothing to read, so it errors.
async fn degrade_offline(uri: &str) -> Result<Read> {
    // `?` propagates a real local-open failure rather than folding it into "no local index".
    match open_for_read(&Memory::local()).await? {
        ReadOutcome::Ready(ds) => Ok(Read {
            ds,
            note: Some(format!("remote {uri} unreachable — recalling from your local memory\n")),
            memory_label: Some(Memory::local().label()),
        }),
        // No local index either — point at onboarding (a local memory is never classified Offline).
        _ => Err(anyhow!(
            "remote {uri} unreachable and no local index yet — run `funes add <agent>` (or `funes index`) to build one"
        )),
    }
}

/// The memory suffix for a hit's `→ get` hint: every hit names the memory it was read from, so the
/// hint drills into that memory from any context. A hit with no memory label yields no suffix.
pub fn memory_hint(read: Option<&str>) -> String {
    match read {
        Some(label) => format!(" --memory {label}"),
        None => String::new(),
    }
}

/// The embedder, and the reranker once a rerank has asked for it, loaded once and shared. Loading
/// a model is the costly part of a recall, so a long-lived process (the MCP server) pays it on the
/// first call and reuses the models after, and a search that never reranks never loads the
/// reranker. The `Mutex` serializes recalls (both models run with `&mut`), which is fine: the work
/// is CPU-bound and the server's calls are serial anyway.
struct Models {
    embedder: Box<dyn Embedder>,
    reranker: Option<Box<dyn Reranker>>,
}

impl Models {
    /// The reranker, built on its first use.
    fn reranker(&mut self) -> Result<&mut dyn Reranker> {
        if self.reranker.is_none() {
            self.reranker = Some(inference::reranker()?);
        }
        Ok(self.reranker.as_deref_mut().expect("built above"))
    }
}

static MODELS: OnceCell<Mutex<Models>> = OnceCell::const_new();

/// The shared model cache, built on first use.
async fn models() -> Result<&'static Mutex<Models>> {
    MODELS
        .get_or_try_init(|| async {
            let embedder = inference::embedder()?;
            Ok::<_, anyhow::Error>(Mutex::new(Models {
                embedder,
                reranker: None,
            }))
        })
        .await
}

/// A recall's defaults, owned here so the CLI and the MCP server search the same way.
pub const DEFAULT_K: usize = 8;
pub const DEFAULT_CANDIDATES: usize = 30;
pub const DEFAULT_NEIGHBORS: i64 = 1;

/// Run the recall pipeline over one memory and return the results rendered in the agent format.
pub async fn recall(
    memory: Memory,
    query: String,
    k: usize,
    candidates: usize,
    neighbors: i64,
    filter: RecallFilter,
) -> Result<String> {
    let (note, hits) = recall_hits(memory, query, k, candidates, neighbors, filter, &|_| ()).await?;
    Ok(rendered(&note, &hits))
}

/// Recall results in the agent format, or the note and `no results` when nothing matched.
pub fn rendered(note: &str, hits: &[(Hit, f64)]) -> String {
    if hits.is_empty() {
        return format!("{note}no results");
    }
    crate::ui::render::recall_agent(note, hits)
}

/// Run the recall pipeline over one memory: hybrid retrieval → rerank → neighbor expansion.
/// Returns the degradation note (empty when the memory opened normally) and the scored hits, best
/// first — rendering is the caller's choice. `progress` hears a short label as each slow phase
/// starts (model load, search, rerank); pass a no-op to run silently.
pub async fn recall_hits(
    memory: Memory,
    query: String,
    k: usize,
    candidates: usize,
    neighbors: i64,
    filter: RecallFilter,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<(String, Vec<(Hit, f64)>)> {
    let search = Search::new(query, candidates, filter, progress).await?;
    let pool = search.candidates(&memory, progress).await?;
    search.rank(vec![pool], k, neighbors, progress).await
}

/// What narrows a search.
#[derive(Default)]
pub struct RecallFilter {
    /// Keep chunks of this block type: `text`, `thinking`, `tool_use` or `tool_result`.
    pub block_type: Option<String>,
    /// Keep turns of this harness, as the turns carry it (`claude` also names `claude_code`).
    pub harness: Option<String>,
    /// Keep turns on or after this `YYYY-MM-DD`.
    pub since: Option<String>,
    /// Keep turns on or before this `YYYY-MM-DD`.
    pub until: Option<String>,
}

impl RecallFilter {
    fn days(&self) -> DayRange<'_> {
        DayRange {
            since: self.since.as_deref(),
            until: self.until.as_deref(),
        }
    }
}

/// One query, embedded once, and the filters every memory's search applies. A recall is
/// [`Search::candidates`] from one or more memories, pooled into a single [`Search::rank`].
pub struct Search {
    query: String,
    qv: Vec<f32>,
    candidates: usize,
    harness_filtered: bool,
    where_clause: Option<String>,
    /// Whether `rank` reranks the pool. Unset, it does.
    rerank: Option<bool>,
}

/// One memory's candidates for a [`Search`], before the rerank.
pub struct Candidates {
    ds: Dataset,
    note: String,
    hits: Vec<Hit>,
}

impl Candidates {
    /// Keep only the candidates `keep` accepts.
    pub fn retain(&mut self, keep: impl Fn(&Hit) -> bool) {
        self.hits.retain(keep);
    }
}

impl Search {
    /// Embed `query` for searching up to `candidates` rows per memory that `filter` keeps.
    pub async fn new(
        query: String,
        candidates: usize,
        filter: RecallFilter,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<Self> {
        let harness = filter.harness.clone().map(harness_spellings).unwrap_or_default();
        progress("loading model…");
        let qv: Vec<f32> = models()
            .await?
            .lock()
            .await
            .embedder
            .embed(&[query.as_str()])?
            .into_iter()
            .next()
            .context("empty embedding")?;
        Ok(Self {
            where_clause: build_where(filter.block_type.as_deref(), &harness, &filter.days())?,
            harness_filtered: !harness.is_empty(),
            query,
            qv,
            candidates,
            rerank: None,
        })
    }

    /// Rerank the pool or not. Unset, the pool is reranked.
    pub fn with_rerank(mut self, rerank: bool) -> Self {
        self.rerank = Some(rerank);
        self
    }

    /// Hybrid retrieval over one memory: a vector ANN scan and a BM25 scan, fused by reciprocal
    /// rank.
    pub async fn candidates(&self, memory: &Memory, progress: &(dyn Fn(&str) + Sync)) -> Result<Candidates> {
        progress(&format!("searching {}…", memory.label()));
        let read = open_read(memory).await?;
        // A `--harness` filter needs the column; on an un-migrated memory it would fail deep inside
        // Lance with an opaque schema error, so refuse with a clear message instead.
        if self.harness_filtered && !has_harness_col(&read.ds) {
            return Err(anyhow!(
                "this memory predates the harness facet — reindex it, or drop --harness"
            ));
        }
        let mut hits = hybrid_candidates(
            &read.ds,
            &self.qv,
            &self.query,
            self.candidates,
            self.where_clause.as_deref(),
        )
        .await?;
        let label = read.memory_label.unwrap_or_default();
        for h in &mut hits {
            h.memory = label.clone();
        }
        Ok(Candidates {
            ds: read.ds,
            note: read.note.unwrap_or_default(),
            hits,
        })
    }

    /// Rank the pooled candidates, a row several memories hold counted once, keep the top `k` and
    /// attach `neighbors` from the memory each hit came from. However many pools there are, at most
    /// `candidates` of them go on, the best by fused score. A rerank scores those with the
    /// cross-encoder, at a cost per candidate that dominates a recall; without one the fused order
    /// stands, with the fused score as the score. Returns the pools' degradation notes and the
    /// scored hits, best first.
    pub async fn rank(
        &self,
        pools: Vec<Candidates>,
        k: usize,
        neighbors: i64,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<(String, Vec<(Hit, f64)>)> {
        let note: String = pools.iter().map(|p| p.note.as_str()).collect();
        let mut seen = HashSet::new();
        let mut hits: Vec<(usize, Hit)> = Vec::new();
        let mut sources = Vec::with_capacity(pools.len());
        for (i, pool) in pools.into_iter().enumerate() {
            hits.extend(
                pool.hits
                    .into_iter()
                    .filter(|h| seen.insert(h.id.clone()))
                    .map(|h| (i, h)),
            );
            sources.push(pool.ds);
        }
        // Stable, so equal scores keep their memory's order and the earlier pool's rows.
        hits.sort_by(|(_, a), (_, b)| b.fused.partial_cmp(&a.fused).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(self.candidates);
        if hits.is_empty() {
            return Ok((note, Vec::new()));
        }

        let mut scored: Vec<(usize, f64)> = if self.rerank.unwrap_or(true) {
            let docs: Vec<&str> = hits.iter().map(|(_, h)| h.text.as_str()).collect();
            progress(&format!("reranking {} candidates…", docs.len()));
            let scores = models()
                .await?
                .lock()
                .await
                .reranker()?
                .rerank(self.query.as_str(), &docs)?;
            scores
                .iter()
                .enumerate()
                .map(|(i, &s)| (i, 1.0 / (1.0 + (-(s as f64)).exp())))
                .collect()
        } else {
            hits.iter().enumerate().map(|(i, (_, h))| (i, h.fused as f64)).collect()
        };
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);

        // Keep only the top-k hits, in scored order, carrying their score and source along.
        let mut top: Vec<(usize, Hit, f64)> = Vec::with_capacity(scored.len());
        let mut taken: Vec<Option<(usize, Hit)>> = hits.into_iter().map(Some).collect();
        for (idx, score) in &scored {
            if let Some((source, h)) = taken[*idx].take() {
                top.push((source, h, *score));
            }
        }

        if neighbors > 0 {
            progress("expanding neighbors…");
            for (i, ds) in sources.iter().enumerate() {
                let mut refs: Vec<&mut Hit> = top.iter_mut().filter(|t| t.0 == i).map(|t| &mut t.1).collect();
                attach_neighbors(ds, &mut refs, neighbors).await?;
            }
        }

        Ok((note, top.into_iter().map(|(_, h, score)| (h, score)).collect()))
    }
}

/// Vector ANN + BM25 candidates fused by reciprocal rank, top `candidates`.
async fn hybrid_candidates(
    ds: &Dataset,
    qv: &[f32],
    query: &str,
    candidates: usize,
    filter: Option<&str>,
) -> Result<Vec<Hit>> {
    let vector = vector_candidates(ds, qv, candidates, filter).await?;
    let fts = fts_candidates(ds, query, candidates, filter).await?;
    Ok(rrf_fuse(vector, fts, candidates))
}

/// Top-`limit` rows by vector distance, each with its `_rowid` (the fusion key).
async fn vector_candidates(ds: &Dataset, qv: &[f32], limit: usize, filter: Option<&str>) -> Result<Vec<(u64, Hit)>> {
    let query = Float32Array::from(qv.to_vec());
    let mut scan = ds.scan();
    scan.nearest("vector", &query, limit)?;
    if let Some(f) = filter {
        // Prefilter: apply the filter before the ANN search, not as a post-filter on the top-`limit`
        // nearest rows. A selective `--type`/`--harness` would otherwise drop most (or all) of a
        // globally-nearest pool, returning far fewer than `limit` hits even when matches exist.
        scan.prefilter(true);
        scan.filter(f)?;
    }
    scan.project(&hit_cols(ds))?;
    scan.with_row_id();
    collect_hits(scan).await
}

/// Top-`limit` rows by BM25 score, each with its `_rowid`. Errors if the memory has no FTS index.
async fn fts_candidates(ds: &Dataset, query: &str, limit: usize, filter: Option<&str>) -> Result<Vec<(u64, Hit)>> {
    let mut scan = ds.scan();
    scan.full_text_search(FullTextSearchQuery::new(query.to_string()))?;
    if let Some(f) = filter {
        // Prefilter so the filter shapes the FTS result set before `limit`, not after.
        scan.prefilter(true);
        scan.filter(f)?;
    }
    scan.project(&hit_cols(ds))?;
    scan.with_row_id();
    scan.limit(Some(limit as i64), None)?;
    collect_hits(scan).await
}

/// Whether the dataset carries `name`. Projecting a column a memory predates errors, so every
/// migrated column is asked for before it is read.
fn has_col(ds: &Dataset, name: &str) -> bool {
    arrow_schema::Schema::from(ds.schema()).column_with_name(name).is_some()
}

/// Whether the memory carries the `harness` column — false for one built before the facet existed.
fn has_harness_col(ds: &Dataset) -> bool {
    has_col(ds, "harness")
}

/// `HIT_COLS`, minus `harness` on an un-migrated memory: projecting a column the dataset lacks errors,
/// so drop it and let `collect_hits` default the field to "".
fn hit_cols(ds: &Dataset) -> Vec<&'static str> {
    let has_harness = has_harness_col(ds);
    HIT_COLS
        .iter()
        .copied()
        .filter(|&c| c != "harness" || has_harness)
        .collect()
}

/// Drain a scan into `(rowid, Hit)` rows, preserving the scan's order (its rank).
async fn collect_hits(scan: lance::dataset::scanner::Scanner) -> Result<Vec<(u64, Hit)>> {
    let mut stream = scan.try_into_stream().await?;
    let mut out = Vec::new();
    while let Some(batch) = stream.try_next().await? {
        let rowid = batch
            .column_by_name(ROW_ID)
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>());
        let (chunk_id, text, sess, proj, turn, ts, bt) = (
            scol(&batch, "id"),
            scol(&batch, "text"),
            scol(&batch, "session_id"),
            scol(&batch, "workdir"),
            scol(&batch, "turn_uuid"),
            scol(&batch, "ts"),
            scol(&batch, "block_type"),
        );
        let seq = icol(&batch, "seq");
        let harness = scol(&batch, "harness");
        for i in 0..batch.num_rows() {
            let id = rowid.map(|c| c.value(i)).unwrap_or(0);
            out.push((
                id,
                Hit {
                    id: sval(chunk_id, i),
                    text: sval(text, i),
                    session_id: sval(sess, i),
                    workdir: sval(proj, i),
                    turn_uuid: sval(turn, i),
                    seq: ival(seq, i),
                    ts: sval(ts, i),
                    block_type: sval(bt, i),
                    harness: sval(harness, i),
                    neighbors: Vec::new(),
                    memory: String::new(),
                    fused: 0.0,
                },
            ));
        }
    }
    Ok(out)
}

/// Reciprocal-rank fusion (k=60): each list contributes `1/(rank + 60)` to a row's score; return
/// the top `limit` rows by fused score, deduped by `_rowid`, each carrying its score.
fn rrf_fuse(vector: Vec<(u64, Hit)>, fts: Vec<(u64, Hit)>, limit: usize) -> Vec<Hit> {
    const K: f32 = 60.0;
    let mut scores: HashMap<u64, f32> = HashMap::new();
    let mut rows: HashMap<u64, Hit> = HashMap::new();
    for list in [vector, fts] {
        for (rank, (id, hit)) in list.into_iter().enumerate() {
            *scores.entry(id).or_insert(0.0) += 1.0 / (rank as f32 + K);
            rows.entry(id).or_insert(hit);
        }
    }
    let mut ranked: Vec<(u64, f32)> = scores.into_iter().collect();
    // Sorted by fused score, and by row id where scores are equal, so the cut never moves.
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    ranked.truncate(limit);
    ranked
        .into_iter()
        .filter_map(|(id, fused)| rows.remove(&id).map(|h| Hit { fused, ..h }))
        .collect()
}

/// For each hit, pull chunks in the same session within `window` of its seq (excluding the
/// hit's own turn) as surrounding context. One combined scan covers every hit.
async fn attach_neighbors(ds: &Dataset, hits: &mut [&mut Hit], window: i64) -> Result<()> {
    if hits.is_empty() {
        return Ok(());
    }
    let pred = hits
        .iter()
        .map(|h| {
            format!(
                "(session_id = '{}' AND seq >= {} AND seq <= {})",
                esc(&h.session_id),
                h.seq - window,
                h.seq + window
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");

    let cols = [
        "session_id",
        "turn_uuid",
        "seq",
        "role",
        "block_type",
        "text",
        "block_idx",
        "split_idx",
    ];
    let batches = dataset::scan_rows(ds, &cols, Some(pred.as_str()), None).await?;

    let mut rows: Vec<NeighborRow> = Vec::new();
    for batch in batches {
        let (sess, turn, role, bt, text) = (
            scol(&batch, "session_id"),
            scol(&batch, "turn_uuid"),
            scol(&batch, "role"),
            scol(&batch, "block_type"),
            scol(&batch, "text"),
        );
        let (seq, bi, si) = (
            icol(&batch, "seq"),
            icol(&batch, "block_idx"),
            icol(&batch, "split_idx"),
        );
        for i in 0..batch.num_rows() {
            rows.push((
                sval(sess, i),
                ival(seq, i),
                sval(turn, i),
                ival(bi, i),
                ival(si, i),
                sval(role, i),
                sval(bt, i),
                sval(text, i),
            ));
        }
    }

    for h in hits.iter_mut() {
        let mut ns: Vec<&NeighborRow> = rows
            .iter()
            .filter(|r| r.0 == h.session_id && r.2 != h.turn_uuid && (r.1 - h.seq).abs() <= window)
            .collect();
        ns.sort_by_key(|r| (r.1, r.3, r.4));
        h.neighbors = ns
            .into_iter()
            .map(|r| Neighbor {
                seq: r.1,
                role: r.5.clone(),
                block_type: r.6.clone(),
                text: r.7.clone(),
            })
            .collect();
    }
    Ok(())
}

/// Which turns of a session to read, as a `seq` range: `seq` is the session's own dense counter over
/// its turns, so a range is turns n through m.
#[derive(Default)]
pub struct TurnRange {
    /// First seq to read. Defaults to the session's start.
    pub from: Option<i64>,
    /// Last seq to read. Defaults to [`DEFAULT_SPAN`] turns from `from`.
    pub to: Option<i64>,
}

/// Turns a read covers when only a start is given. The CLI and the MCP server defer to it rather
/// than carrying a default of their own.
pub const DEFAULT_SPAN: i64 = 20;

/// Read a range of a session's turns, rendered in the agent format.
pub async fn get(memory: Memory, session_id: String, range: TurnRange) -> Result<String> {
    let label = memory.label();
    let (note, turns, total) = get_turns(memory, session_id.clone(), range).await?;
    // A session with no rows is absent, not empty: only a range can come back empty.
    if total == 0 {
        bail!("no session {session_id} in {label}");
    }
    if turns.is_empty() {
        return Ok(format!(
            "{note}no turns in that range of session {session_id} (it holds {total})\n"
        ));
    }
    Ok(crate::ui::render::get_agent(&note, &turns, total))
}

/// The turns behind `get`, each reassembled (blocks in order, splits de-overlapped). Returns the
/// degradation note, the turns — empty when the range holds none — and the session's turn count.
pub async fn get_turns(memory: Memory, session_id: String, range: TurnRange) -> Result<(String, Vec<Turn>, usize)> {
    let read = open_read(&memory).await?;
    let note = read.note.clone().unwrap_or_default();
    let ds = &read.ds;

    let cols = ["turn_uuid", "seq", "ts", "role", "text", "block_idx", "split_idx"];
    let filter = format!("session_id = '{}'", esc(&session_id));
    let batches = dataset::scan_rows(ds, &cols, Some(filter.as_str()), None).await?;

    // `text` is already the rendered chunk as stored by the indexer — do not re-render.
    let mut rows: Vec<TurnRow> = Vec::new();
    for batch in batches {
        let (turn, ts, role, text) = (
            scol(&batch, "turn_uuid"),
            scol(&batch, "ts"),
            scol(&batch, "role"),
            scol(&batch, "text"),
        );
        let (seq, bi, si) = (
            icol(&batch, "seq"),
            icol(&batch, "block_idx"),
            icol(&batch, "split_idx"),
        );
        for i in 0..batch.num_rows() {
            rows.push((
                ival(seq, i),
                sval(turn, i),
                sval(ts, i),
                sval(role, i),
                ival(bi, i),
                ival(si, i),
                sval(text, i),
            ));
        }
    }

    let total = rows.iter().map(|r| (r.0, &r.1)).collect::<HashSet<_>>().len();
    let from = range
        .from
        .unwrap_or_else(|| rows.iter().map(|r| r.0).min().unwrap_or(0));
    let to = range.to.unwrap_or(from + DEFAULT_SPAN - 1);
    let kept = rows.iter().filter(|r| r.0 >= from && r.0 <= to);
    Ok((note, turns_from_rows(kept), total))
}

/// What narrows a listing.
#[derive(Default)]
pub struct SessionFilter {
    /// Keep sessions whose stored repo names this `owner/name`.
    pub repo: Option<String>,
    /// Keep sessions that started on or after this `YYYY-MM-DD`.
    pub since: Option<String>,
    /// Keep sessions that started on or before this `YYYY-MM-DD`.
    pub until: Option<String>,
    /// Rows to render; `None` takes [`SESSIONS_LIMIT`], and anything above [`SESSIONS_LIMIT_MAX`] is
    /// clamped to it.
    pub limit: Option<usize>,
    /// Skip this many of the most recent matches before taking `limit`. Rows are ordered on
    /// (timestamp, session id), so a given offset always names the same row.
    pub offset: usize,
}

impl SessionFilter {
    /// Whether `s` survives every filter that was given.
    fn keeps(&self, s: &Session) -> bool {
        // A session's repo field can name several checkouts; any of them counts. Empty means the
        // checkout didn't resolve at index time, which no `--repo` can claim.
        if let Some(repo) = &self.repo {
            if !s.repo.split_whitespace().any(|i| i == repo) {
                return false;
            }
        }
        self.days().holds(&s.ts)
    }

    fn days(&self) -> DayRange<'_> {
        DayRange {
            since: self.since.as_deref(),
            until: self.until.as_deref(),
        }
    }
}

/// The sessions of a memory that `filter` keeps, oldest first, rendered in the agent format.
pub async fn sessions(memory: Memory, filter: SessionFilter) -> Result<String> {
    check_filter(&filter)?;
    list_sessions(vec![SessionPool::open(&memory).await?], filter).await
}

/// One memory's sessions, before a listing pools, filters and pages them.
pub struct SessionPool {
    ds: Dataset,
    note: String,
    label: String,
    sessions: Vec<Session>,
}

impl SessionPool {
    /// Keep only the sessions `keep` accepts.
    pub fn retain(&mut self, keep: impl Fn(&Session) -> bool) {
        self.sessions.retain(keep);
    }

    /// Open `memory` and fold its rows into sessions.
    pub async fn open(memory: &Memory) -> Result<Self> {
        let read = open_read(memory).await?;
        Ok(Self {
            sessions: scan_sessions(&read.ds).await?,
            note: read.note.unwrap_or_default(),
            label: read.memory_label.unwrap_or_else(|| memory.label()),
            ds: read.ds,
        })
    }
}

/// Zero would render nothing, which is never what a caller wants, and a bound that is not a date
/// would match nothing or everything without a word.
fn check_filter(filter: &SessionFilter) -> Result<()> {
    if filter.limit == Some(0) {
        bail!("a limit of 0 would list nothing — omit it for {SESSIONS_LIMIT} rows, raise it to at most {SESSIONS_LIMIT_MAX}, and walk the rest with --offset");
    }
    filter.days().check()
}

/// The sessions of the pooled memories that `filter` keeps, oldest first, rendered in the agent
/// format; a session several pools hold is listed from the first. The prompts are read after the
/// filter and the bound, so their cost follows the rows rendered rather than the size of the memory.
pub async fn list_sessions(mut pools: Vec<SessionPool>, filter: SessionFilter) -> Result<String> {
    check_filter(&filter)?;
    let note: String = pools.iter().map(|p| p.note.as_str()).collect();
    let label = pools.first().map(|p| p.label.clone()).unwrap_or_default();
    let mut seen = HashSet::new();
    let mut all: Vec<(usize, Session)> = Vec::new();
    for (i, pool) in pools.iter_mut().enumerate() {
        let sessions = std::mem::take(&mut pool.sessions);
        all.extend(
            sessions
                .into_iter()
                .filter(|s| seen.insert(s.session_id.clone()))
                .map(|s| (i, s)),
        );
    }
    if all.is_empty() {
        return Ok(format!("{note}no sessions in {label}\n"));
    }
    all.sort_by(|(_, a), (_, b)| (&a.ts, &a.session_id).cmp(&(&b.ts, &b.session_id)));
    let matched: Vec<(usize, Session)> = all.into_iter().filter(|(_, s)| filter.keeps(s)).collect();
    if matched.is_empty() {
        return Ok(format!("{note}no session in {label} matches\n"));
    }

    // Oldest first is the reading order, but a page is taken from the recent end and `offset` walks
    // back from there.
    let total = matched.len();
    let limit = filter.limit.unwrap_or(SESSIONS_LIMIT).min(SESSIONS_LIMIT_MAX);
    let end = total.saturating_sub(filter.offset);
    let shown: Vec<(usize, Session)> = matched.into_iter().take(end).skip(end.saturating_sub(limit)).collect();
    if shown.is_empty() {
        return Ok(format!(
            "{note}offset {} is past the {total} session(s) in {label}\n",
            filter.offset
        ));
    }
    let mut prompts = HashMap::new();
    for (i, pool) in pools.iter().enumerate() {
        let ids: Vec<String> = shown
            .iter()
            .filter(|(from, _)| *from == i)
            .map(|(_, s)| s.session_id.clone())
            .collect();
        prompts.extend(first_prompts(&pool.ds, &ids).await?);
    }
    let shown: Vec<Session> = shown
        .into_iter()
        .map(|(_, s)| Session {
            first_prompt: prompts.remove(&s.session_id).unwrap_or_default(),
            ..s
        })
        .collect();
    Ok(crate::ui::render::sessions_agent(&note, &shown, total, filter.offset))
}

/// Whether a user text block's start is injected scaffolding rather than the human's words. Harness
/// wrappers are XML-ish tags (`<ide_opened_file>`, `<command-name>`, `<environment_context>`,
/// `<system-reminder>`, …); codex/pi agent-notes open with a markdown heading (`# AGENTS.md
/// instructions…`); a Claude skill loads with the `Base directory for this skill:` preamble; a
/// compacted session replays its machine-written recap as a user turn opening `This session is being
/// continued…`. All are recognizable from the block's start (a mid-block split can begin anywhere, so
/// test split 0).
pub(crate) fn is_scaffolding(block_start: &str) -> bool {
    let t = block_start.trim_start();
    t.starts_with('<')
        || t.starts_with('#')
        || t.starts_with("Base directory for this skill")
        || t.starts_with("This session is being continued from a previous conversation")
}

/// The opening real prompt of each session in `ids` — its earliest `user` text block that isn't
/// injected scaffolding, or its earliest text block of any role when it has no `user` turn at all.
/// A session whose `user` turns are all scaffolding is absent from the map.
async fn first_prompts(ds: &Dataset, ids: &[String]) -> Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let list: Vec<String> = ids.iter().map(|id| format!("'{}'", esc(id))).collect();
    // Split 0 only: `is_scaffolding` reads a block's start, and a later split begins mid-text.
    let filter = format!(
        "session_id IN ({}) AND block_type = 'text' AND split_idx = 0",
        list.join(", ")
    );
    let cols = ["session_id", "seq", "block_idx", "role", "text"];
    let batches = dataset::scan_rows(ds, &cols, Some(&filter), None).await?;
    // `has_user`: a session whose `user` texts are all scaffolding opens on nothing, not on a reply.
    #[derive(Default)]
    struct Opening {
        user: Option<((i64, i64), String)>,
        has_user: bool,
        any: Option<((i64, i64), String)>,
    }
    fn earliest(slot: &mut Option<((i64, i64), String)>, key: (i64, i64), body: &str) {
        if slot.as_ref().is_none_or(|(k, _)| key < *k) {
            *slot = Some((key, body.to_string()));
        }
    }
    let mut best: HashMap<String, Opening> = HashMap::new();
    for batch in &batches {
        let (sid, role, text) = (scol(batch, "session_id"), scol(batch, "role"), scol(batch, "text"));
        let (seq, bi) = (icol(batch, "seq"), icol(batch, "block_idx"));
        for i in 0..batch.num_rows() {
            let body = sval(text, i);
            let key = (ival(seq, i), ival(bi, i));
            let opening = best.entry(sval(sid, i)).or_default();
            if sval(role, i) == "user" {
                opening.has_user = true;
                if !is_scaffolding(&body) {
                    earliest(&mut opening.user, key, &body);
                }
            }
            earliest(&mut opening.any, key, &body);
        }
    }
    Ok(best
        .into_iter()
        .filter_map(|(id, o)| {
            let pick = if o.has_user { o.user } else { o.any };
            pick.map(|(_, text)| (id, text))
        })
        .collect())
}

/// Fold every row into its session: earliest timestamp, provenance, and distinct turn count. Reads
/// metadata only — the opening prompts cost a `text` read, so they are fetched separately.
async fn scan_sessions(ds: &Dataset) -> Result<Vec<Session>> {
    let mut cols = vec!["session_id", "ts", "workdir", "turn_uuid", "seq"];
    if has_harness_col(ds) {
        cols.push("harness");
    }
    if has_col(ds, "repo") {
        cols.push("repo");
    }
    let batches = dataset::scan_rows(ds, &cols, None, None).await?;

    let mut by_id: HashMap<String, (Session, HashSet<(i64, String)>)> = HashMap::new();
    for batch in &batches {
        let (sid, ts, wd, turn, harness, repo) = (
            scol(batch, "session_id"),
            scol(batch, "ts"),
            scol(batch, "workdir"),
            scol(batch, "turn_uuid"),
            scol(batch, "harness"),
            scol(batch, "repo"),
        );
        let seq = icol(batch, "seq");
        for i in 0..batch.num_rows() {
            let id = sval(sid, i);
            let (session, turns) = by_id.entry(id.clone()).or_insert_with(|| {
                (
                    Session {
                        session_id: id,
                        ts: sval(ts, i),
                        workdir: sval(wd, i),
                        harness: sval(harness, i),
                        repo: sval(repo, i),
                        turns: 0,
                        first_prompt: String::new(),
                    },
                    HashSet::new(),
                )
            });
            // Rows arrive in scan order, not time order, so the first one seen isn't the earliest.
            let row_ts = sval(ts, i);
            if row_ts < session.ts {
                session.ts = row_ts;
            }
            turns.insert((ival(seq, i), sval(turn, i)));
        }
    }

    let mut out: Vec<Session> = by_id
        .into_values()
        .map(|(session, turns)| Session {
            turns: turns.len(),
            ..session
        })
        .collect();
    out.sort_by(|a, b| (&a.ts, &a.session_id).cmp(&(&b.ts, &b.session_id)));
    Ok(out)
}

/// One block of a memory, its splits stitched back together, with the facets a `scan` hit prints.
struct Block {
    turn_uuid: String,
    ts: String,
    block_type: String,
    seq: i64,
    block_idx: i64,
    text: String,
}

/// Blocks under assembly, keyed by (seq, turn_uuid, block_idx): each one's facets, and the split
/// rows still to be stitched into its `text`. The seq is part of the key because a turn uuid can
/// recur at different positions in a session — a compacted transcript replays turns — so two blocks
/// that merely share a uuid are two blocks, not one.
type BlockParts = HashMap<(i64, String, i64), (Block, Vec<(i64, String)>)>;

/// Find `needle` in every block of one session, rendered in the agent format. Literal, never a
/// pattern: a regex that silently matched nothing would read as a clearance.
///
/// A session that isn't in the memory is an error, not an empty result.
pub async fn scan(
    memory: Memory,
    needle: String,
    session_id: String,
    from: Option<i64>,
    to: Option<i64>,
    ignore_case: bool,
    context: usize,
) -> Result<String> {
    let read = open_read(&memory).await?;
    let note = read.note.clone().unwrap_or_default();
    let label = read.memory_label.clone().unwrap_or_else(|| memory.label());
    let blocks = reassembled_blocks(&read.ds, &session_id, from, to).await?;
    if blocks.is_empty() {
        // A window that holds nothing is not the same as a session that isn't there: only the
        // unwindowed case can conclude the session is absent.
        if from.is_some() || to.is_some() {
            let scanned = reassembled_blocks(&read.ds, &session_id, None, None).await?;
            if !scanned.is_empty() {
                return Ok(format!(
                    "{note}no turns in that range of session {session_id} (it holds {})\n",
                    scanned.iter().map(|b| b.seq).collect::<HashSet<_>>().len()
                ));
            }
        }
        bail!("no session {session_id} in {label}");
    }
    let result = find_needle(&blocks, &needle, &session_id, from, to, ignore_case);
    Ok(crate::ui::render::scan_agent(
        &note,
        &memory_hint(read.memory_label.as_deref()),
        &result,
        context,
    ))
}

/// Every block of one session, splits de-overlapped. Matching raw chunks would miss a needle that
/// straddles a split boundary, so the session's rows are bucketed by block before anything is
/// matched. Ordered by position in the session. Empty when the session isn't in the memory.
async fn reassembled_blocks(ds: &Dataset, session_id: &str, from: Option<i64>, to: Option<i64>) -> Result<Vec<Block>> {
    let cols = ["turn_uuid", "seq", "ts", "block_type", "block_idx", "split_idx", "text"];
    let mut filter = format!("session_id = '{}'", esc(session_id));
    if let Some(from) = from {
        filter.push_str(&format!(" AND seq >= {from}"));
    }
    if let Some(to) = to {
        filter.push_str(&format!(" AND seq <= {to}"));
    }
    let batches = dataset::scan_rows(ds, &cols, Some(filter.as_str()), None).await?;

    // Splits of one block can land in different batches, so every row is bucketed before any of it
    // is stitched.
    let mut blocks: BlockParts = HashMap::new();
    for batch in &batches {
        let (turn, ts, bt, text) = (
            scol(batch, "turn_uuid"),
            scol(batch, "ts"),
            scol(batch, "block_type"),
            scol(batch, "text"),
        );
        let (seq, bi, si) = (icol(batch, "seq"), icol(batch, "block_idx"), icol(batch, "split_idx"));
        for i in 0..batch.num_rows() {
            let key = (ival(seq, i), sval(turn, i), ival(bi, i));
            let entry = blocks.entry(key).or_insert_with(|| {
                (
                    Block {
                        turn_uuid: sval(turn, i),
                        ts: sval(ts, i),
                        block_type: sval(bt, i),
                        seq: ival(seq, i),
                        block_idx: ival(bi, i),
                        text: String::new(),
                    },
                    Vec::new(),
                )
            });
            entry.1.push((ival(si, i), sval(text, i)));
        }
    }
    drop(batches);

    let mut out: Vec<Block> = blocks
        .into_values()
        .map(|(mut block, mut splits)| {
            splits.sort_by_key(|(si, _)| *si);
            let mut pieces = splits.into_iter().map(|(_, t)| t);
            block.text = pieces.next().unwrap_or_default();
            for piece in pieces {
                block.text = chunk::stitch(&block.text, &piece);
            }
            block
        })
        .collect();
    out.sort_by_key(|b| (b.seq, b.block_idx));
    Ok(out)
}

/// Every block of the scanned window carrying `needle`, in reading order, capped at
/// [`SCAN_HIT_CAP`] — with the coordinate a continuing scan resumes from when the cap bites.
fn find_needle(
    blocks: &[Block],
    needle: &str,
    session_id: &str,
    from: Option<i64>,
    to: Option<i64>,
    ignore_case: bool,
) -> ScanResult {
    let folded: Vec<char> = if ignore_case {
        needle.chars().map(fold).collect()
    } else {
        Vec::new()
    };
    let mut hits: Vec<ScanHit> = Vec::new();
    for b in blocks {
        let at = if ignore_case {
            find_folded(&b.text, &folded)
        } else {
            b.text.find(needle)
        };
        let Some(at) = at else { continue };
        hits.push(ScanHit {
            turn_uuid: b.turn_uuid.clone(),
            ts: b.ts.clone(),
            block_type: b.block_type.clone(),
            seq: b.seq,
            at,
            len: needle.chars().count(),
            text: b.text.clone(),
        });
    }

    // Cut at a turn boundary. Hits are in reading order, so dropping the trailing hits that share
    // the first dropped hit's turn leaves a page a caller can continue from exactly: everything
    // rendered lies before that turn.
    let found = hits.len();
    let cut = hits.get(SCAN_HIT_CAP).map(|h| h.seq).map(|boundary| {
        match hits[..SCAN_HIT_CAP].iter().rposition(|h| h.seq < boundary) {
            Some(last) => {
                hits.truncate(last + 1);
                ScanCut::Resume(boundary)
            }
            // The cap falls inside a single turn's own matches: no boundary to cut at.
            None => {
                hits.truncate(SCAN_HIT_CAP);
                ScanCut::Crowded(boundary)
            }
        }
    });
    ScanResult {
        needle: needle.to_string(),
        session_id: session_id.to_string(),
        dropped: found - hits.len(),
        hits,
        cut,
        from,
        to,
    }
}

/// Byte offset of the first case-folded occurrence of `needle` (already folded) in `text`. Folding
/// is per char, so the offset stays an offset into the original.
fn find_folded(text: &str, needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let hay: Vec<(usize, char)> = text.char_indices().collect();
    if hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).find_map(|w| {
        w.iter()
            .zip(needle)
            .all(|(&(_, c), &want)| fold(c) == want)
            .then_some(w[0].0)
    })
}

/// Lowercase `c` when that is a single char. One whose lowercase is several (`İ`) stays as it is and
/// simply won't fold-match.
fn fold(c: char) -> char {
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(one), None) => one,
        _ => c,
    }
}

/// Reassemble rows into turns: group by (seq, turn_uuid), order blocks by (block_idx, split_idx),
/// stitching consecutive splits of one block. Ordered by seq. `text` is already the rendered chunk
/// as stored by the indexer — never re-rendered.
fn turns_from_rows<'a>(rows: impl Iterator<Item = &'a TurnRow>) -> Vec<Turn> {
    let mut groups: BTreeMap<(i64, String), Vec<&TurnRow>> = BTreeMap::new();
    for r in rows {
        groups.entry((r.0, r.1.clone())).or_default().push(r);
    }
    let mut turns = Vec::new();
    for ((seq, turn), mut chunks) in groups {
        chunks.sort_by_key(|r| (r.4, r.5)); // block_idx, split_idx
        let head = chunks[0];
        let mut blocks: Vec<String> = Vec::new();
        let mut cur_bi: Option<i64> = None;
        let mut cur = String::new();
        for r in &chunks {
            let bi = r.4;
            let piece = &r.6;
            if Some(bi) != cur_bi {
                if !cur.is_empty() {
                    blocks.push(std::mem::take(&mut cur));
                }
                cur_bi = Some(bi);
                cur = piece.clone();
            } else {
                cur = chunk::stitch(&cur, piece);
            }
        }
        if !cur.is_empty() {
            blocks.push(cur);
        }
        turns.push(Turn {
            seq,
            turn_uuid: turn,
            ts: head.2.clone(),
            role: head.3.clone(),
            blocks,
        });
    }
    turns
}

/// `2026-07-07 13:30 UTC (2 days ago)` — a status timestamp with its coarse age.
fn stamp(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    format!("{} ({})", t.format("%Y-%m-%d %H:%M UTC"), age(t, now))
}

/// Coarse relative age: "just now", then minutes, hours (up to two days), days.
fn age(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let mins = (now - t).num_minutes().max(0);
    let (n, unit) = match mins {
        0 => return "just now".to_string(),
        1..=59 => (mins, "minute"),
        60..=2879 => (mins / 60, "hour"),
        _ => (mins / (24 * 60), "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Distinct sessions in a local memory — the human-scale size of the index. Best-effort: a failed
/// scan omits the line rather than failing status. Never call this for a remote: even a projected
/// column scan can download an enormous published memory.
async fn session_count(ds: &Dataset) -> Option<usize> {
    let batches = dataset::scan_rows(ds, &["session_id"], None, None).await.ok()?;
    let mut sessions = HashSet::new();
    for batch in &batches {
        let col = batch
            .column_by_name("session_id")?
            .as_any()
            .downcast_ref::<StringArray>()?;
        for i in 0..batch.num_rows() {
            sessions.insert(col.value(i).to_string());
        }
    }
    Some(sessions.len())
}

async fn pending_embeddings_line(ds: &Dataset) -> Result<String> {
    let n = ds
        .count_rows(Some("vector IS NULL".into()))
        .await
        .context("counting chunks awaiting embedding")?;
    Ok(if n == 0 {
        String::new()
    } else {
        format!("{n} chunk{} awaiting embedding\n", if n == 1 { "" } else { "s" })
    })
}

fn index_coverage_line() -> Option<String> {
    let coverage = super::index::local_index_coverage()?;
    (coverage.pending > 0).then(|| {
        format!(
            "pending indexing: {} source session{} — run `funes index`\n",
            coverage.pending,
            if coverage.pending == 1 { "" } else { "s" }
        )
    })
}

/// Local index progress and its last write time. A version with no recorded timestamp is omitted.
async fn index_lines(ds: &Dataset, now: DateTime<Utc>) -> Result<String> {
    let mut out = String::new();
    if let Some(n) = session_count(ds).await {
        let _ = writeln!(out, "sessions: {n}");
    }
    out.push_str(&pending_embeddings_line(ds).await?);
    if let Some(line) = index_coverage_line() {
        out.push_str(&line);
    }
    let t = ds.version().timestamp;
    if t.timestamp() > 0 {
        let _ = writeln!(out, "last indexed: {}", stamp(t, now));
    }
    Ok(out)
}

/// A remote memory's published state: when it was last pushed to and its unindexed backlog.
async fn remote_lines(ds: &Dataset, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    // Every write to a remote memory is a `funes push` (data or reindex commit), so the head
    // version's timestamp is when it was last pushed to.
    let t = ds.version().timestamp;
    if t.timestamp() > 0 {
        let _ = writeln!(out, "last push: {}", stamp(t, now));
    }
    let unindexed = crate::memory::remote::max_unindexed_rows(ds).await;
    if unindexed > 0 {
        let _ = writeln!(
            out,
            "unindexed: {unindexed} chunks (searched brute-force until a push reindexes)"
        );
    }
    out
}

/// What this host has yet to push to `memory`, a memory an agent here is bound to, from the receipt
/// `push` keeps: a shared remote's total says nothing about this host's backlog. Without a receipt,
/// how to start one.
async fn push_coverage_lines(local: &Dataset, memory: &Memory, uri: &str) -> String {
    let mut out = String::new();
    let Some(coverage) = super::push::local_push_coverage(local, uri).await else {
        let _ = writeln!(
            out,
            "local push coverage: unknown — run `funes push {}` once",
            memory.label()
        );
        return out;
    };
    let plural = if coverage.total == 1 { "" } else { "s" };
    if coverage.pending == 0 {
        let _ = writeln!(out, "local push: up to date ({} session{plural})", coverage.total);
        return out;
    }
    let _ = writeln!(
        out,
        "local push: {} of {} session{plural} pending — run `funes push {}`",
        coverage.pending,
        coverage.total,
        memory.label()
    );
    if let Some(held) = &coverage.held {
        let _ = writeln!(
            out,
            "  {} pending row(s) hold secrets ({}) — run `funes scrub` first",
            held.rows, held.summary
        );
    }
    out
}

/// `memory`'s status. A remote reports this host's push coverage only when an agent here is `bound`
/// to it: coverage is a question only for a memory this host pushes to.
pub async fn status(memory: Memory, bound: bool) -> Result<String> {
    match open_for_read(&memory).await? {
        ReadOutcome::Ready(ds) => {
            let now = Utc::now();
            let rows = ds.count_rows(None).await?;
            let mut out = format!("memory: {}\nchunks: {rows}\n", memory.label());
            match &memory {
                Memory::Local { .. } => out.push_str(&index_lines(&ds, now).await?),
                Memory::Remote { uri } => {
                    out.push_str(&remote_lines(&ds, now).await);
                    // The local index is what pushes here — show it alongside, so one status
                    // answers both "what's published" and "what's indexed on this machine".
                    if let Ok(local) = Memory::local().open().await {
                        let local_rows = local.count_rows(None).await?;
                        let _ = writeln!(
                            out,
                            "\nlocal index: {}\nchunks: {local_rows}",
                            Memory::local().label()
                        );
                        out.push_str(&index_lines(&local, now).await?);
                        if bound {
                            out.push_str(&push_coverage_lines(&local, &memory, uri).await);
                        }
                    }
                }
            }
            Ok(out)
        }
        // An unreachable remote shows the local index's status instead, like the read commands.
        ReadOutcome::Offline => {
            let body = Box::pin(status(Memory::local(), false)).await?;
            Ok(format!(
                "remote {} unreachable — showing your local memory instead\n{body}",
                memory.label()
            ))
        }
        // No personal index yet: point at the onboarding command instead of erroring. (recall/get/
        // list return a clear "no index" error in the same situation.)
        ReadOutcome::NoIndex => Ok(format!(
            "memory: {}\nno index yet — run `funes add <agent>` to build one (or `funes index`), then recall your own history.\n",
            memory.label(),
        )),
    }
}

/// This host's status: the local memory, then each memory its integrations are bound to — `bound`
/// pairs a memory spec with the ids bound to it — and the ids installed with no memory recorded.
pub async fn host_status(bound: &[(String, Vec<String>)], unrecorded: &[String]) -> Result<String> {
    let mut out = status(Memory::local(), false).await?;
    let local = Memory::local().open().await.ok();
    let now = Utc::now();
    for (spec, ids) in bound {
        let memory = Memory::parse(spec);
        let _ = writeln!(out, "\nmemory: {} (bound: {})", memory.label(), ids.join(", "));
        let ds = match open_for_read(&memory).await {
            Ok(ReadOutcome::Ready(ds)) => ds,
            Ok(ReadOutcome::Offline) => {
                out.push_str("unreachable\n");
                continue;
            }
            Ok(ReadOutcome::NoIndex) => continue,
            Err(e) => {
                let _ = writeln!(out, "{e:#}");
                continue;
            }
        };
        let _ = writeln!(out, "chunks: {}", ds.count_rows(None).await?);
        if let Memory::Remote { uri } = &memory {
            out.push_str(&remote_lines(&ds, now).await);
            if let Some(local) = &local {
                out.push_str(&push_coverage_lines(local, &memory, uri).await);
            }
        }
    }
    if !unrecorded.is_empty() {
        let _ = writeln!(
            out,
            "\ninstalled with no memory recorded: {} — if one publishes, `funes add <agent> <memory>` records where",
            unrecorded.join(", ")
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traces::{Block, Turn, FORMAT_VERSION};
    use arrow_array::RecordBatchIterator;
    use chrono::TimeZone;
    use std::path::Path;

    #[test]
    fn is_scaffolding_flags_wrappers_and_headings() {
        assert!(is_scaffolding("<ide_opened_file>/foo/bar.rs</ide_opened_file>"));
        assert!(is_scaffolding("<environment_context>\n  <cwd>/w</cwd>"));
        assert!(is_scaffolding("   <system-reminder>be nice"));
        assert!(is_scaffolding("# AGENTS.md instructions for /w\n\n<INSTRUCTIONS>"));
        assert!(is_scaffolding(
            "Base directory for this skill: /home/u/.claude/skills/funes\n\n# funes"
        ));
        assert!(is_scaffolding(
            "This session is being continued from a previous conversation that ran out of context."
        ));
        assert!(!is_scaffolding("explain me again why funes push finds secrets"));
        assert!(!is_scaffolding("why did we drop lancedb for funes"));
    }

    #[test]
    fn age_picks_the_coarsest_readable_unit() {
        let now = Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap();
        let at = |y, mo, d, h, mi| Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap();
        assert_eq!(
            age(Utc.with_ymd_and_hms(2026, 7, 9, 11, 59, 30).unwrap(), now),
            "just now"
        );
        assert_eq!(age(at(2026, 7, 9, 11, 59), now), "1 minute ago");
        assert_eq!(age(at(2026, 7, 9, 11, 15), now), "45 minutes ago");
        assert_eq!(age(at(2026, 7, 9, 9, 0), now), "3 hours ago");
        assert_eq!(age(at(2026, 7, 8, 11, 0), now), "25 hours ago"); // hours up to 2 days
        assert_eq!(age(at(2026, 7, 4, 12, 0), now), "5 days ago");
        // A future timestamp (clock skew) clamps to "just now" rather than going negative.
        assert_eq!(age(at(2026, 7, 9, 13, 0), now), "just now");
    }

    #[test]
    fn stamp_formats_utc_with_age() {
        let now = Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap();
        let t = Utc.with_ymd_and_hms(2026, 7, 7, 13, 30, 0).unwrap();
        assert_eq!(stamp(t, now), "2026-07-07 13:30 UTC (46 hours ago)");
    }

    /// A memory holding `turns`, written to a temp dir.
    async fn memory_of(turns: &[Turn], dir: &Path) -> Dataset {
        let chunks = chunk::chunks_from_turns(turns, &chunk::Tier::ALL, true);
        let vectors = vec![vec![0.0f32; dataset::DIM as usize]; chunks.len()];
        let batch = dataset::build_batch(&chunks, Some(&vectors)).unwrap();
        let reader = RecordBatchIterator::new(vec![Ok(batch)], dataset::schema());
        Dataset::write(reader, dir.to_str().unwrap(), None).await.unwrap()
    }

    #[tokio::test]
    async fn recall_requires_fts_even_when_vector_candidates_exist() {
        let dir = tempfile::tempdir().unwrap();
        let ds = memory_of(
            &[text_turn("session", 0, "user", "A narwhal parses transcripts.")],
            dir.path(),
        )
        .await;
        let qv = vec![1.0; dataset::DIM as usize];
        assert_eq!(vector_candidates(&ds, &qv, 5, None).await.unwrap().len(), 1);
        assert!(
            hybrid_candidates(&ds, &qv, "narwhal", 5, None).await.is_err(),
            "a missing FTS index must fail recall even when vector search finds a candidate"
        );
    }

    #[tokio::test]
    async fn status_counts_pending_embeddings_until_they_are_filled() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::parse(&dir.path().to_string_lossy());
        let turns = [
            text_turn("session", 0, "user", "A narwhal parses transcripts."),
            text_turn("session", 1, "assistant", "Each line becomes a typed turn."),
        ];
        let chunks = chunk::chunks_from_turns(&turns, &chunk::Tier::ALL, true);
        let batch = dataset::build_batch(&chunks, None).unwrap();
        let reader = RecordBatchIterator::new([Ok(batch)], dataset::schema());
        let mut ds = Dataset::write(reader, &dataset::table_uri(&memory.label()), None)
            .await
            .unwrap();
        let prefix = format!("memory: {}\nchunks: 2\nsessions: 1\n", memory.label());

        for (chunk, line) in chunks
            .iter()
            .zip(["2 chunks awaiting embedding\n", "1 chunk awaiting embedding\n"])
        {
            let out = status(memory.clone(), false).await.unwrap();
            assert!(out.starts_with(&format!("{prefix}{line}")), "{out}");
            ds = dataset::fill_vectors(&ds, &[chunk.id.as_str()], &[vec![0.0; dataset::DIM as usize]])
                .await
                .unwrap();
        }
        let out = status(memory, false).await.unwrap();
        assert!(out.starts_with(&prefix), "{out}");
        assert!(!out.contains("awaiting embedding"), "{out}");
    }

    fn text_turn(session: &str, seq: i64, role: &str, text: &str) -> Turn {
        Turn {
            format: FORMAT_VERSION,
            session_id: session.into(),
            cwd: None,
            workdir: String::new(),
            turn_uuid: format!("{session}-{seq}"),
            parent_uuid: None,
            seq,
            ts: "2026-01-01T00:00:00Z".into(),
            role: role.into(),
            blocks: vec![Block {
                block_type: "text".into(),
                text: text.into(),
                tool_name: None,
                tool_use_id: None,
            }],
            source_path: String::new(),
            harness: "t".into(),
        }
    }

    #[tokio::test]
    async fn first_prompts_open_on_user_text_else_the_first_text_of_any_role() {
        let dir = tempfile::tempdir().unwrap();
        let turns = [
            // An agent session: the first non-scaffolding user text, not the earlier scaffolding.
            text_turn("agent", 0, "user", "<system-reminder>…</system-reminder>"),
            text_turn("agent", 1, "user", "how do we parse transcripts"),
            text_turn("agent", 2, "assistant", "with serde"),
            // A tracker thread: no `user` turn at all — the first text block opens it.
            text_turn("issue", 0, "contributor", "**@someone** opened: build fails on arm64"),
            text_turn("issue", 1, "member", "reproduced on main"),
            // Only scaffolding user text: nothing opens it, a reply is not a prompt.
            text_turn("scaffold", 0, "user", "# skill preamble"),
            text_turn("scaffold", 1, "assistant", "ok"),
        ];
        let ds = memory_of(&turns, dir.path()).await;
        let ids: Vec<String> = ["agent", "issue", "scaffold"].map(String::from).to_vec();
        let prompts = first_prompts(&ds, &ids).await.unwrap();
        assert_eq!(prompts["agent"], "how do we parse transcripts");
        assert_eq!(prompts["issue"], "**@someone** opened: build fails on arm64");
        assert!(!prompts.contains_key("scaffold"), "{prompts:?}");
    }

    /// The `limit` cut usually lands inside a tie, and `scores` is a HashMap whose order is seeded
    /// per process, so fusion has to impose a total order of its own: same lists, same rows, same
    /// order, every run.
    #[test]
    fn rrf_fuse_settles_ties_by_row_id() {
        let hit = |id: u64| Hit {
            id: id.to_string(),
            text: id.to_string(),
            session_id: String::new(),
            workdir: String::new(),
            turn_uuid: String::new(),
            seq: 0,
            ts: String::new(),
            block_type: String::new(),
            harness: String::new(),
            neighbors: Vec::new(),
            memory: String::new(),
            fused: 0.0,
        };
        // Disjoint lists: vector row 2i and FTS row 2i+1 both sit at rank i, so every rank ties.
        let lists = || {
            let v: Vec<(u64, Hit)> = (0..20).map(|i| (2 * i, hit(2 * i))).collect();
            let f: Vec<(u64, Hit)> = (0..20).map(|i| (2 * i + 1, hit(2 * i + 1))).collect();
            (v, f)
        };
        let ids = |hits: Vec<Hit>| hits.into_iter().map(|h| h.text).collect::<Vec<_>>();

        let (v, f) = lists();
        let want: Vec<String> = (0..40).map(|i: u64| i.to_string()).collect();
        assert_eq!(ids(rrf_fuse(v, f, 40)), want, "fused order is not the tie-broken order");

        // The cut through a tie takes the same rows every run, not an arbitrary half of it.
        let (v, f) = lists();
        assert_eq!(ids(rrf_fuse(v, f, 5)), ["0", "1", "2", "3", "4"]);
    }

    #[test]
    fn esc_doubles_single_quotes() {
        assert_eq!(esc("o'brien"), "o''brien");
        assert_eq!(esc("plain"), "plain");
    }

    #[test]
    fn memory_hint_names_the_read_memory() {
        assert_eq!(
            memory_hint(Some("hf://datasets/acme/kb")),
            " --memory hf://datasets/acme/kb"
        );
        // A hit with no memory label yields no suffix.
        assert_eq!(memory_hint(None), "");
    }

    #[test]
    fn harness_spellings_cover_both_names_of_a_known_agent_and_pass_others_through() {
        assert_eq!(harness_spellings("claude".into()), ["claude_code", "claude"]);
        assert_eq!(harness_spellings("claude_code".into()), ["claude_code", "claude"]);
        assert_eq!(harness_spellings("codex".into()), ["codex"]);
        assert_eq!(harness_spellings("opencode".into()), ["opencode"]);
    }

    #[test]
    fn build_where_combines_set_filters() {
        let one = |h: &str| vec![h.to_string()];
        let any = DayRange::default();
        let w = |bt: Option<&str>, h: &[String], d: &DayRange| build_where(bt, h, d).unwrap();
        assert_eq!(w(None, &[], &any), None);
        assert_eq!(w(Some("text"), &[], &any).as_deref(), Some("block_type = 'text'"));
        assert_eq!(w(None, &one("codex"), &any).as_deref(), Some("harness = 'codex'"));
        assert_eq!(
            w(Some("tool_use"), &one("pi"), &any).as_deref(),
            Some("block_type = 'tool_use' AND harness = 'pi'")
        );
        assert_eq!(
            w(None, &harness_spellings("claude".into()), &any).as_deref(),
            Some("harness IN ('claude_code', 'claude')")
        );
        // A day range is inclusive: the upper clause is the day after `until`, exclusive.
        let week = DayRange {
            since: Some("2026-09-14"),
            until: Some("2026-09-20"),
        };
        assert_eq!(
            w(Some("text"), &[], &week).as_deref(),
            Some("block_type = 'text' AND ts >= '2026-09-14' AND ts < '2026-09-21'")
        );
        let year_end = DayRange {
            since: None,
            until: Some("2026-12-31"),
        };
        assert_eq!(w(None, &[], &year_end).as_deref(), Some("ts < '2027-01-01'"));
        // values are escaped against filter-string injection; a bound that is not a date is refused.
        assert_eq!(w(None, &one("a'b"), &any).as_deref(), Some("harness = 'a''b'"));
        let err = build_where(
            None,
            &[],
            &DayRange {
                since: Some("x'y"),
                until: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("expected YYYY-MM-DD"), "{err}");
        // So is a date in another spelling: the text compare on `ts` needs the zero padding.
        let unpadded = DayRange {
            since: Some("2026-9-14"),
            until: None,
        };
        assert!(build_where(None, &[], &unpadded).is_err());
        assert!(unpadded.check().is_err());
    }

    #[test]
    fn day_range_is_inclusive_at_both_ends_and_open_without_a_bound() {
        let week = DayRange {
            since: Some("2026-09-14"),
            until: Some("2026-09-20"),
        };
        assert!(week.holds("2026-09-14T00:00:00Z"));
        assert!(week.holds("2026-09-20T23:59:59Z"));
        assert!(!week.holds("2026-09-13T23:59:59Z"));
        assert!(!week.holds("2026-09-21T00:00:00Z"));
        assert!(DayRange::default().holds("1999-01-01T00:00:00Z"));
        assert!(DayRange {
            since: Some("2026-09-14"),
            until: None
        }
        .holds("2030-01-01T00:00:00Z"));
        // The filter clauses draw the same line: `ts >= since` and `ts < the day after until`.
        for ts in [
            "2026-09-13T23:59:59Z",
            "2026-09-14T00:00:00Z",
            "2026-09-20T23:59:59.999Z",
            "2026-09-21T00:00:00Z",
        ] {
            assert_eq!(week.holds(ts), ("2026-09-14".."2026-09-21").contains(&ts), "{ts}");
        }
    }
}
