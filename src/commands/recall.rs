//! The read surface: `recall`, `get`, `sessions`, `scan`, `status` over the existing index.
//! Recall pipeline: hybrid (vector + BM25, fused by reciprocal rank) → cross-encoder rerank →
//! recency reweight → neighbor expansion. `recall`/`get`/`sessions`/`scan` return results rendered
//! in the agent format; `recall_hits`/`get_turns` return the structured results for other
//! renderings (see `render`).

use super::curate;
use crate::chunk;
use crate::inference::{self, Embedder, Reranker};
use crate::memory::dataset;
use crate::memory::{Memory, MemoryState};
use crate::traces::harness::Harness;
use anyhow::{anyhow, bail, Context, Result};
use arrow_array::{Float32Array, Int64Array, RecordBatch, StringArray, UInt64Array};
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use lance::dataset::{Dataset, ROW_ID};
use lance_index::scalar::FullTextSearchQuery;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use tokio::sync::{Mutex, OnceCell};

/// Columns a [`Hit`] needs from a search scan.
const HIT_COLS: &[&str] = &[
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
    pub text: String,
    pub session_id: String,
    pub workdir: String,
    pub turn_uuid: String,
    pub seq: i64,
    pub ts: String,
    pub block_type: String,
    pub harness: String,
    pub neighbors: Vec<Neighbor>,
}

/// Matching blocks `scan` lists before it stops. A session runs to thousands of turns, so a common
/// word could still fill a reply; what the cap dropped is always reported.
const SCAN_HIT_CAP: usize = 200;

/// Why a `scan` listing stopped short, and what the caller can do about it.
pub enum ScanCut {
    /// Hits remain from this turn onward; a continuing scan starts exactly there. The page was cut
    /// back to a turn boundary so that resuming neither repeats a hit nor skips one.
    Resume(i64),
    /// This one turn holds more matches than the cap by itself, so paging cannot step over it —
    /// there is no coordinate that both makes progress and keeps every hit. Read the turn instead.
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
    /// Why the listing stopped, when it did — and where a continuing scan picks up.
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
    /// Distinct turns, not rows: chunking is an indexing artifact, and a turn is what `get` reads —
    /// so this counts (seq, turn_uuid) pairs, the same unit `get` renders and `--from`/`--to` index.
    /// A uuid alone would undercount: a compacted transcript replays turns under the same uuid.
    pub turns: usize,
    /// The opening real prompt, scaffolding skipped — what the session was for, in one line.
    pub first_prompt: String,
}

impl Session {
    /// The `YYYY-MM-DD` the session started.
    pub fn date(&self) -> &str {
        self.ts.get(..10).unwrap_or(&self.ts)
    }

    /// Best available provenance: the repo when the checkout resolved, else the working directory.
    pub fn origin(&self) -> &str {
        self.repo.split_whitespace().next().unwrap_or(&self.workdir)
    }
}

/// How many sessions a listing renders before it elides the rest. A row carries an opening prompt
/// now, so a memory of a few hundred sessions would otherwise answer past any tool-result ceiling —
/// the very failure the filters exist to avoid.
const SESSIONS_LIMIT: usize = 50;

/// The most rows one listing will render, whatever `limit` asks for. Past this the reply is larger
/// than a tool result can carry, so a higher `limit` would only produce an answer nobody receives:
/// walking with `offset` is the way to see more.
const SESSIONS_LIMIT_MAX: usize = 200;

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
fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// `block_type = '…' AND harness = '…'` over whichever filters are set, else None.
fn build_where(block_type: Option<&str>, harness: Option<&str>) -> Option<String> {
    let mut clauses = Vec::new();
    if let Some(bt) = block_type {
        clauses.push(format!("block_type = '{}'", esc(bt)));
    }
    if let Some(h) = harness {
        clauses.push(format!("harness = '{}'", esc(h)));
    }
    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" AND "))
    }
}

/// 0.5^(age/half_life): 1.0 for fresh, decaying with age. half_life <= 0 disables.
fn recency_weight(ts: &str, now: DateTime<Utc>, half_life: f64) -> f64 {
    if half_life <= 0.0 {
        return 1.0;
    }
    match DateTime::parse_from_rfc3339(ts) {
        Ok(t) => {
            let age_days = (now - t.with_timezone(&Utc)).num_seconds() as f64 / 86_400.0;
            0.5f64.powf(age_days.max(0.0) / half_life)
        }
        Err(_) => 1.0,
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

/// The embedder + reranker, loaded once and shared. Loading them (ONNX init) is the costly part of
/// a recall, so a long-lived process — the MCP server — pays it on the first call and reuses them
/// after. The `Mutex` serializes recalls (both models run with `&mut`), which is fine: the work is
/// CPU-bound and the server's calls are serial anyway.
struct Models {
    embedder: Box<dyn Embedder>,
    reranker: Box<dyn Reranker>,
}

static MODELS: OnceCell<Mutex<Models>> = OnceCell::const_new();

/// The shared model cache, built on first use.
async fn models() -> Result<&'static Mutex<Models>> {
    MODELS
        .get_or_try_init(|| async {
            let embedder = inference::embedder()?;
            let reranker = inference::reranker()?;
            Ok::<_, anyhow::Error>(Mutex::new(Models { embedder, reranker }))
        })
        .await
}

/// Run the recall pipeline over one memory and return the results rendered in the agent format.
#[allow(clippy::too_many_arguments)]
pub async fn recall(
    memory: Memory,
    query: String,
    k: usize,
    candidates: usize,
    half_life: f64,
    neighbors: i64,
    block_type: Option<String>,
    harness: Option<String>,
) -> Result<String> {
    let (note, memory_label, hits) = recall_hits(
        memory,
        query,
        k,
        candidates,
        half_life,
        neighbors,
        block_type,
        harness,
        &|_| (),
    )
    .await?;
    if hits.is_empty() {
        return Ok(format!("{note}no results"));
    }
    Ok(crate::ui::render::recall_agent(
        &note,
        &memory_hint(memory_label.as_deref()),
        &hits,
    ))
}

/// Run the recall pipeline over one memory: hybrid retrieval → rerank → recency reweight →
/// neighbor expansion. Returns the degradation note (empty when the memory opened normally), the
/// label of the memory actually read, and the scored hits, best
/// first — rendering is the caller's choice. `progress` hears a short label as each slow phase
/// starts (model load, search, rerank); pass a no-op to run silently.
#[allow(clippy::too_many_arguments)]
pub async fn recall_hits(
    memory: Memory,
    query: String,
    k: usize,
    candidates: usize,
    half_life: f64,
    neighbors: i64,
    block_type: Option<String>,
    harness: Option<String>,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<(String, Option<String>, Vec<(Hit, f64)>)> {
    // `--harness` accepts the same spellings as `index`/`add` (claude|codex|pi); normalize to the
    // stored facet (Claude's is `claude_code`) so `--harness claude` filters instead of silently
    // matching nothing, and an unknown value errors here rather than returning zero hits.
    let harness = harness
        .map(|h| Harness::parse(&h))
        .transpose()?
        .map(|h| h.as_str().to_string());

    progress("loading model…");
    let mut guard = models().await?.lock().await;
    let Models { embedder, reranker } = &mut *guard;

    let qv: Vec<f32> = embedder
        .embed(&[query.as_str()])?
        .into_iter()
        .next()
        .context("empty embedding")?;

    progress(&format!("searching {}…", memory.label()));
    let read = open_read(&memory).await?;
    let note = read.note.clone().unwrap_or_default();
    let ds = &read.ds;
    // A `--harness` filter needs the column; on an un-migrated memory it would fail deep inside Lance
    // with an opaque schema error, so refuse with a clear message instead.
    if harness.is_some() && !has_harness_col(ds) {
        return Err(anyhow!(
            "this memory predates the harness facet — reindex it, or drop --harness"
        ));
    }
    let where_clause = build_where(block_type.as_deref(), harness.as_deref());

    // Hybrid retrieval: a vector ANN scan and a BM25 scan, fused by reciprocal rank. The FTS index
    // can be absent (it's best-effort at index time), so the FTS leg is skipped when it errors —
    // recall then falls back to vector-only.
    let hits = hybrid_candidates(ds, &qv, &query, candidates, where_clause.as_deref()).await?;
    if hits.is_empty() {
        return Ok((note, read.memory_label.clone(), Vec::new()));
    }

    let docs: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
    progress(&format!("reranking {} candidates…", docs.len()));
    let scores = reranker.rerank(query.as_str(), &docs)?;

    let now = Utc::now();
    let mut scored: Vec<(usize, f64)> = scores
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let relevance = 1.0 / (1.0 + (-(s as f64)).exp());
            (i, relevance * recency_weight(&hits[i].ts, now, half_life))
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);

    // Keep only the top-k hits, in scored order, carrying their score along.
    let mut top: Vec<(Hit, f64)> = Vec::with_capacity(scored.len());
    let mut taken: Vec<Option<Hit>> = hits.into_iter().map(Some).collect();
    for (idx, score) in &scored {
        if let Some(h) = taken[*idx].take() {
            top.push((h, *score));
        }
    }

    if neighbors > 0 {
        progress("expanding neighbors…");
        let mut refs: Vec<&mut Hit> = top.iter_mut().map(|(h, _)| h).collect();
        attach_neighbors(ds, &mut refs, neighbors).await?;
    }

    Ok((note, read.memory_label.clone(), top))
}

/// Vector ANN + BM25 candidates fused by reciprocal rank, top `candidates`. The FTS leg is
/// best-effort: a memory with no FTS index makes that scan error, and we fall back to vector-only.
async fn hybrid_candidates(
    ds: &Dataset,
    qv: &[f32],
    query: &str,
    candidates: usize,
    filter: Option<&str>,
) -> Result<Vec<Hit>> {
    let vector = vector_candidates(ds, qv, candidates, filter).await?;
    let fts = fts_candidates(ds, query, candidates, filter).await.unwrap_or_default();
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

/// Whether the memory carries the `harness` column — false for one built before the facet existed
/// (an un-migrated memory).
fn has_harness_col(ds: &Dataset) -> bool {
    has_col(ds, "harness")
}

/// Whether the dataset carries `name`. Projecting a column a memory predates errors, so every
/// migrated column is asked for before it is read.
fn has_col(ds: &Dataset, name: &str) -> bool {
    arrow_schema::Schema::from(ds.schema()).column_with_name(name).is_some()
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
        let (text, sess, proj, turn, ts, bt) = (
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
                    text: sval(text, i),
                    session_id: sval(sess, i),
                    workdir: sval(proj, i),
                    turn_uuid: sval(turn, i),
                    seq: ival(seq, i),
                    ts: sval(ts, i),
                    block_type: sval(bt, i),
                    harness: sval(harness, i),
                    neighbors: Vec::new(),
                },
            ));
        }
    }
    Ok(out)
}

/// Reciprocal-rank fusion (k=60): each list contributes `1/(rank + 60)` to a row's score; return
/// the top `limit` rows by fused score, deduped by `_rowid`.
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
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(limit);
    ranked.into_iter().filter_map(|(id, _)| rows.remove(&id)).collect()
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

/// Which turns of a session to read. `seq` is a session's own coordinate — a dense counter over its
/// turns — so a range is exactly "turns n through m", and it is the only way to address a turn.
#[derive(Default)]
pub struct TurnRange {
    /// First seq to read. Defaults to the session's start.
    pub from: Option<i64>,
    /// Last seq to read. Defaults to [`DEFAULT_SPAN`] turns from `from`.
    pub to: Option<i64>,
}

/// Turns a coordinate read covers when only a start is given — enough to see what a stretch of a
/// session is doing, without committing to a session that runs to thousands of turns.
const DEFAULT_SPAN: i64 = 20;

/// Read a range of a session's turns, rendered in the agent format. A hit's `→ get` line carries
/// the range it sits in, and a session id alone reads from the start — so choosing a session and
/// reading it are the same act, addressed the same way.
pub async fn get(memory: Memory, session_id: String, range: TurnRange) -> Result<String> {
    let (note, turns, total) = get_turns(memory, session_id.clone(), range).await?;
    if turns.is_empty() {
        return Ok(format!(
            "{note}no turns in that range of session {session_id} (it holds {total})\n"
        ));
    }
    Ok(crate::ui::render::get_agent(&note, &turns, total))
}

/// What narrows a listing: the population a criterion is about, before anything of it is read.
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
    /// Skip this many of the most recent matches before taking `limit` — how a listing is walked
    /// back through time. Rows are ordered on (timestamp, session id), so a given offset always
    /// names the same row: continuing neither repeats a session nor skips one.
    pub offset: usize,
}

/// The sessions of a memory that `filter` keeps, oldest first, rendered in the agent format. Ranked
/// retrieval reaches what a query reaches and says nothing about the rest, so enumeration is the
/// only thing that answers what a memory holds and how much of it a pass has covered — which is why
/// an elided row is always counted, never dropped quietly.
///
/// The prompts are read after the filter and the bound, so their cost follows the rows that will be
/// rendered rather than the size of the memory.
pub async fn sessions(memory: Memory, filter: SessionFilter) -> Result<String> {
    // Zero once meant "every match", back when a listing had no ceiling. It now means an empty
    // reply, which is never what a caller wants — so say so instead of returning nothing.
    if filter.limit == Some(0) {
        bail!("a limit of 0 would list nothing — omit it for {SESSIONS_LIMIT} rows, raise it to at most {SESSIONS_LIMIT_MAX}, and walk the rest with --offset");
    }
    let read = open_read(&memory).await?;
    let note = read.note.clone().unwrap_or_default();
    let label = read.memory_label.clone().unwrap_or_else(|| memory.label());
    let all = scan_sessions(&read.ds).await?;
    if all.is_empty() {
        return Ok(format!("{note}no sessions in {label}\n"));
    }
    let matched: Vec<Session> = all.into_iter().filter(|s| filter.keeps(s)).collect();
    if matched.is_empty() {
        return Ok(format!("{note}no session in {label} matches\n"));
    }

    // Oldest first is the reading order, but a page is taken from the *recent* end — the tail of a
    // memory is what a pass is usually catching up on — and `offset` walks back from there.
    let total = matched.len();
    let limit = filter.limit.unwrap_or(SESSIONS_LIMIT).min(SESSIONS_LIMIT_MAX);
    let end = total.saturating_sub(filter.offset);
    let mut shown: Vec<Session> = matched.into_iter().take(end).skip(end.saturating_sub(limit)).collect();
    if shown.is_empty() {
        return Ok(format!(
            "{note}offset {} is past the {total} session(s) in {label}\n",
            filter.offset
        ));
    }
    let ids: Vec<String> = shown.iter().map(|s| s.session_id.clone()).collect();
    let mut prompts = first_prompts(&read.ds, &ids).await?;
    for s in shown.iter_mut() {
        s.first_prompt = prompts.remove(&s.session_id).unwrap_or_default();
    }
    Ok(crate::ui::render::sessions_agent(&note, &shown, total, filter.offset))
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
        if let Some(since) = &self.since {
            if s.date() < since.as_str() {
                return false;
            }
        }
        if let Some(until) = &self.until {
            if s.date() > until.as_str() {
                return false;
            }
        }
        true
    }
}

/// The opening real prompt of each session in `ids` — its earliest user text block that isn't
/// injected scaffolding, collapsed to one line. A session whose user turns are all scaffolding is
/// absent from the map.
async fn first_prompts(ds: &Dataset, ids: &[String]) -> Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let list: Vec<String> = ids.iter().map(|id| format!("'{}'", esc(id))).collect();
    // Split 0 only: `is_scaffolding` reads a block's start, and a later split begins mid-text.
    let filter = format!(
        "session_id IN ({}) AND role = 'user' AND block_type = 'text' AND split_idx = 0",
        list.join(", ")
    );
    let cols = ["session_id", "seq", "block_idx", "text"];
    let batches = dataset::scan_rows(ds, &cols, Some(&filter), None).await?;
    let mut best: HashMap<String, ((i64, i64), String)> = HashMap::new();
    for batch in &batches {
        let (sid, text) = (scol(batch, "session_id"), scol(batch, "text"));
        let (seq, bi) = (icol(batch, "seq"), icol(batch, "block_idx"));
        for i in 0..batch.num_rows() {
            let body = sval(text, i);
            if crate::commands::curate::is_scaffolding(&body) {
                continue;
            }
            let key = (ival(seq, i), ival(bi, i));
            let entry = best.entry(sval(sid, i));
            match entry {
                std::collections::hash_map::Entry::Occupied(mut e) if key < e.get().0 => {
                    e.insert((key, body));
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((key, body));
                }
                _ => {}
            }
        }
    }
    Ok(best.into_iter().map(|(id, (_, text))| (id, text)).collect())
}

/// Fold every row into its session: earliest timestamp, provenance, and distinct turn count. Reads
/// metadata only — the opening prompts cost a `text` read, so they are fetched separately for the
/// rows that survive the filter.
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
/// recur at different positions in a session — a compacted transcript replays turns — and two
/// blocks that merely share a uuid are two blocks, not one.
type BlockParts = HashMap<(i64, String, i64), (Block, Vec<(i64, String)>)>;

/// Find `needle` in every block of one session, rendered in the agent format. Where recall ranks
/// and can only show what is present, this is exhaustive over the session: a needle that returns
/// nothing is absent from all of it. Literal, never a pattern — a regex that silently matches
/// nothing would read as exactly that clearance.
///
/// A session that isn't in the memory is an error, not an empty result: a mistyped id would
/// otherwise read as the clearance this verb exists to give.
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
        bail!("no session {session_id} in {label} — `funes sessions` lists the ones there are");
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
    // rendered lies before that turn. Cutting mid-turn instead would force a resume that either
    // repeats the turn's earlier hits or skips its later ones.
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

/// Lowercase `c` when that is a single char — which covers the case variation `--ignore-case` is
/// for. A char whose lowercase is several (`İ`) stays as it is and simply won't fold-match.
fn fold(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// The turns behind `get`, each reassembled (blocks in order, splits de-overlapped). Returns the
/// degradation note, the turns — empty when the range holds none — and how many turns the session
/// holds in total, so a partial read can say what it is part of.
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

/// The reassembled user prompts (role `user`, block type `text`) of each session in `ids`, keyed by
/// session id — one scan, for previewing candidates before a curation decision. Only user turns
/// carry the human's judgment; assistant replies and tool results are left out. Sessions with no
/// prompts (or an empty `ids`) are simply absent from the map.
pub async fn session_prompts(memory: &Memory, ids: &[String]) -> Result<HashMap<String, Vec<Turn>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let read = open_read(memory).await?;
    let cols = [
        "session_id",
        "turn_uuid",
        "seq",
        "ts",
        "role",
        "text",
        "block_idx",
        "split_idx",
    ];
    let list: Vec<String> = ids.iter().map(|id| format!("'{}'", esc(id))).collect();
    let filter = format!(
        "session_id IN ({}) AND role = 'user' AND block_type = 'text'",
        list.join(", ")
    );
    let batches = dataset::scan_rows(&read.ds, &cols, Some(&filter), None).await?;
    let mut by_session: HashMap<String, Vec<TurnRow>> = HashMap::new();
    for batch in batches {
        let sid = scol(&batch, "session_id");
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
            by_session.entry(sval(sid, i)).or_default().push((
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
    Ok(by_session
        .into_iter()
        .map(|(k, rows)| (k, turns_from_rows(rows.iter())))
        .collect())
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

/// The indexation lines of a local memory: how many sessions it holds and when it was last
/// written to (an `index` or `scrub` run). A version with no recorded timestamp is omitted.
async fn index_lines(ds: &Dataset, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    if let Some(n) = session_count(ds).await {
        let _ = writeln!(out, "sessions: {n}");
    }
    if let Some(line) = index_coverage_line() {
        out.push_str(&line);
    }
    let t = ds.version().timestamp;
    if t.timestamp() > 0 {
        let _ = writeln!(out, "last indexed: {}", stamp(t, now));
    }
    out
}

pub async fn status(memory: Memory) -> Result<String> {
    match open_for_read(&memory).await? {
        ReadOutcome::Ready(ds) => {
            let now = Utc::now();
            let rows = ds.count_rows(None).await?;
            let mut out = format!("memory: {}\nchunks: {rows}\n", memory.label());
            match &memory {
                Memory::Local { .. } => out.push_str(&index_lines(&ds, now).await),
                Memory::Remote { uri } => {
                    // A project memory announces itself and this machine's review backlog.
                    let project = curate::project(&ds);
                    if let Some(project) = &project {
                        let _ = writeln!(out, "project memory of {project}");
                        let pending = curate::pending_count(&ds, uri).await?;
                        if pending > 0 {
                            let _ = writeln!(out, "pending review: {pending} session(s) — run `funes curate {}`", memory.label());
                        }
                    }
                    // Every write to a remote memory is a `funes push` (data or reindex commit),
                    // so the head version's timestamp is when it was last pushed to.
                    let t = ds.version().timestamp;
                    if t.timestamp() > 0 {
                        let _ = writeln!(out, "last push: {}", stamp(t, now));
                    }
                    let unindexed = crate::memory::remote::max_unindexed_rows(&ds).await;
                    if unindexed > 0 {
                        let _ = writeln!(
                            out,
                            "unindexed: {unindexed} chunks (searched brute-force until a push reindexes)"
                        );
                    }
                    // The local index is what pushes here — show it alongside, so one status
                    // answers both "what's published" and "what's indexed on this machine".
                    if let Ok(local) = Memory::local().open().await {
                        let local_rows = local.count_rows(None).await?;
                        let _ = writeln!(
                            out,
                            "\nlocal index: {}\nchunks: {local_rows}",
                            Memory::local().label()
                        );
                        let local_sessions = session_count(&local).await;
                        if let Some(n) = local_sessions {
                            let _ = writeln!(out, "sessions: {n}");
                        }
                        if let Some(line) = index_coverage_line() {
                            out.push_str(&line);
                        }
                        // A shared remote's total says nothing about this host's backlog. Personal
                        // memories use the local receipt maintained by push; project memories have
                        // their decision-aware pending-review report above.
                        if project.is_none() {
                            if let Some(coverage) = super::push::local_push_coverage(&local, uri).await
                            {
                                if coverage.pending == 0 {
                                    let _ = writeln!(
                                        out,
                                        "local push: up to date ({} session{})",
                                        coverage.total,
                                        if coverage.total == 1 { "" } else { "s" }
                                    );
                                } else {
                                    let _ = writeln!(
                                        out,
                                        "local push: {} of {} session{} pending — run `funes push {}`",
                                        coverage.pending,
                                        coverage.total,
                                        if coverage.total == 1 { "" } else { "s" },
                                        memory.label()
                                    );
                                    if let Some(held) = &coverage.held {
                                        let _ = writeln!(
                                            out,
                                            "  {} pending row(s) hold secrets ({}) — run `funes scrub` first",
                                            held.rows, held.summary
                                        );
                                    }
                                }
                            } else {
                                let _ = writeln!(
                                    out,
                                    "local push coverage: unknown — run `funes push {}` once",
                                    memory.label()
                                );
                            }
                        }
                        let t = local.version().timestamp;
                        if t.timestamp() > 0 {
                            let _ = writeln!(out, "last indexed: {}", stamp(t, now));
                        }
                    }
                }
            }
            Ok(out)
        }
        // An unreachable remote shows the local index's status instead, like the read commands.
        ReadOutcome::Offline => {
            let body = Box::pin(status(Memory::local())).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
    fn build_where_combines_set_filters() {
        assert_eq!(build_where(None, None), None);
        assert_eq!(build_where(Some("text"), None).as_deref(), Some("block_type = 'text'"));
        assert_eq!(build_where(None, Some("codex")).as_deref(), Some("harness = 'codex'"));
        assert_eq!(
            build_where(Some("tool_use"), Some("pi")).as_deref(),
            Some("block_type = 'tool_use' AND harness = 'pi'")
        );
        // values are escaped against filter-string injection.
        assert_eq!(build_where(None, Some("a'b")).as_deref(), Some("harness = 'a''b'"));
    }

    #[test]
    fn find_folded_matches_case_insensitively_at_original_offsets() {
        let folded = |n: &str| -> Vec<char> { n.chars().map(fold).collect() };
        let text = "the Cache is Pinned to a Commit";
        // The offset indexes the original text, not a lowercased copy.
        assert_eq!(find_folded(text, &folded("PINNED")), Some(13));
        assert_eq!(&text[13..19], "Pinned");
        assert_eq!(find_folded(text, &folded("cache")), Some(4));
        assert_eq!(find_folded(text, &folded("absent")), None);
        // An empty needle never matches, so it can't report the whole memory as a hit.
        assert_eq!(find_folded(text, &[]), None);
        // A needle longer than the text is not a match rather than a panic.
        assert_eq!(find_folded("ab", &folded("abc")), None);
    }

    #[test]
    fn fold_lowercases_only_one_to_one_mappings() {
        assert_eq!(fold('A'), 'a');
        assert_eq!(fold('É'), 'é');
        assert_eq!(fold('a'), 'a');
        // `İ` lowercases to two chars, which would break offset arithmetic — left as it is.
        assert_eq!(fold('İ'), 'İ');
    }

    #[test]
    fn recency_weight_halves_each_half_life() {
        let now = Utc.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).unwrap();
        // disabled
        assert_eq!(recency_weight("2026-01-01T00:00:00Z", now, 0.0), 1.0);
        // fresh
        assert!((recency_weight("2026-01-31T00:00:00Z", now, 30.0) - 1.0).abs() < 1e-9);
        // exactly one half-life (30 days) old -> 0.5
        assert!((recency_weight("2026-01-01T00:00:00Z", now, 30.0) - 0.5).abs() < 1e-9);
        // future timestamps clamp to fresh, not >1.
        assert!((recency_weight("2026-02-10T00:00:00Z", now, 30.0) - 1.0).abs() < 1e-9);
        // unparseable -> neutral 1.0
        assert_eq!(recency_weight("not-a-date", now, 30.0), 1.0);
    }
}
