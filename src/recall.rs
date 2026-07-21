//! The read surface: `recall`, `get`, `status` over the existing index.
//! Recall pipeline: hybrid (vector + BM25, fused by reciprocal rank) → cross-encoder rerank →
//! recency reweight → neighbor expansion. `recall`/`get` return results rendered in the agent
//! format; `recall_hits`/`get_turns` return the structured results for other renderings
//! (see `render`).

use crate::chunk;
use crate::curate;
use crate::dataset;
use crate::harness::Harness;
use crate::hub::{self, Memory, Reachability};
use crate::inference::{self, Embedder, Reranker};
use anyhow::{anyhow, Context, Result};
use arrow_array::{Float32Array, Int64Array, RecordBatch, StringArray, UInt64Array};
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use lance::dataset::{Dataset, ROW_ID};
use lance::Error as LanceError;
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

/// The outcome of resolving a memory for reading. `Offline` degrades to the local index (this
/// module's to apply); a missing or empty remote is a hard error with a clear message; `NoIndex`
/// (the default local memory, unbuilt) is a clear error from the read verbs and a friendly note in
/// `status`.
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

/// Resolve a memory for reading — the one place every source state is handled. An offline remote
/// degrades (`Offline`); a missing or empty remote errors with a clear message; an absent default
/// local index degrades (`NoIndex`); a present memory opens (`Ready`). All read verbs route through
/// this; the remote-state classification and messages come from `hub`.
async fn open_for_read(memory: &Memory) -> Result<ReadOutcome> {
    if let Memory::Remote { uri } = memory {
        match hub::remote_reachability(uri).await {
            Reachability::Offline => return Ok(ReadOutcome::Offline),
            Reachability::Missing => return Err(hub::missing_remote(uri)),
            Reachability::Ok => {}
        }
    }
    match memory.open().await {
        Ok(ds) => Ok(ReadOutcome::Ready(ds)),
        // The default local memory with no dataset yet → NoIndex (a clear "run funes add" error
        // below). Gated on `is_missing_dataset` so a real open failure (permissions, corruption, an
        // incompatible schema) isn't masked as "no index" — it falls through to surface as itself.
        Err(e) if memory.is_default_local() && is_missing_dataset(&e) => Ok(ReadOutcome::NoIndex),
        // The Hub refused the read on auth (401/403): a clear message beats lance's opendal dump.
        Err(e) if is_auth_error(&e) => match memory {
            Memory::Remote { uri } => Err(hub::unauthorized_remote(uri)),
            Memory::Local { .. } => Err(e),
        },
        // Opened to nothing: a reachable remote never pushed to, or a local path with no dataset.
        // Either way, a clear message beats lance's internal path error.
        Err(e) if is_missing_dataset(&e) => match memory {
            Memory::Remote { uri } => Err(hub::empty_remote(uri)),
            Memory::Local { path } => Err(anyhow::anyhow!("no index found at {}", path.display())),
        },
        Err(e) => Err(e),
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

/// True if `e` is lance reporting the `chunks.lance` dataset isn't there — the memory opened to no
/// index (an empty or never-pushed remote, or a path with no dataset). Lets reads report an empty
/// memory instead of leaking lance's internal path/`_versions` error.
fn is_missing_dataset(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| matches!(c.downcast_ref::<LanceError>(), Some(LanceError::DatasetNotFound { .. })))
}

/// True if `e` is the Hub refusing a remote read on auth (401/403). lance has no typed auth variant
/// — it buries an opendal `PermissionDenied` (with the HTTP status) in an `IO` error — so match the
/// chain's text.
fn is_auth_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string();
        s.contains("PermissionDenied") || s.contains("status: 401") || s.contains("status: 403")
    })
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
    Ok(crate::render::recall_agent(
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
    arrow_schema::Schema::from(ds.schema())
        .column_with_name("harness")
        .is_some()
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

/// Drill down on a recall hit: the named turn plus the turns within `window` of it, rendered in
/// the agent format.
pub async fn get(memory: Memory, session_id: String, turn_uuid: String, window: i64) -> Result<String> {
    let (note, turns) = get_turns(memory, session_id.clone(), turn_uuid.clone(), window).await?;
    if turns.is_empty() {
        return Ok(format!("{note}turn {turn_uuid} not found in session {session_id}\n"));
    }
    Ok(crate::render::get_agent(&note, &turns))
}

/// The turns behind `get`: the named one plus those within `window` of it, each reassembled
/// (blocks in order, splits de-overlapped). Returns the degradation note and the turns — empty
/// when the turn isn't in the session; rendering is the caller's choice.
pub async fn get_turns(
    memory: Memory,
    session_id: String,
    turn_uuid: String,
    window: i64,
) -> Result<(String, Vec<Turn>)> {
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

    let center = match rows.iter().find(|r| r.1 == turn_uuid) {
        Some(r) => r.0,
        None => return Ok((note, Vec::new())),
    };
    Ok((
        note,
        turns_from_rows(rows.iter().filter(|r| (r.0 - center).abs() <= window)),
    ))
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

/// Distinct sessions in the memory — the human-scale size of the index. Best-effort: a failed
/// scan omits the line rather than failing status.
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

/// The indexation lines of a local memory: how many sessions it holds and when it was last
/// written to (an `index` or `scrub` run). A version with no recorded timestamp is omitted.
async fn index_lines(ds: &Dataset, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    if let Some(n) = session_count(ds).await {
        let _ = writeln!(out, "sessions: {n}");
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
                    if let Some(project) = curate::project(&ds) {
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
                    let unindexed = crate::hf_dataset::max_unindexed_rows(&ds).await;
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
                        let _ = write!(out, "\nlocal index: {}\nchunks: {local_rows}", Memory::local().label());
                        // A count difference only approximates the push delta (scrub-held
                        // rows and other hosts' pushes also move it).
                        if local_rows > rows {
                            let _ = write!(out, " (≈{} not yet pushed)", local_rows - rows);
                        }
                        out.push('\n');
                        out.push_str(&index_lines(&local, now).await);
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

    #[tokio::test]
    async fn missing_dataset_is_detected() {
        // Opening a path with no dataset is lance's DatasetNotFound — the empty/absent case.
        let err = dataset::open("/nonexistent/funes-empty-memory/chunks.lance", HashMap::new())
            .await
            .unwrap_err();
        assert!(is_missing_dataset(&err));
    }

    #[test]
    fn unrelated_error_is_not_missing_dataset() {
        assert!(!is_missing_dataset(&anyhow::anyhow!("some other failure")));
    }

    #[test]
    fn auth_error_is_detected() {
        // Shape verified against a live 401: an opendal PermissionDenied with the HTTP status.
        let err = anyhow::anyhow!(
            "LanceError(IO): Generic PermissionDenied error: PermissionDenied (permanent) at list, \
             response: status: 401"
        );
        assert!(is_auth_error(&err));
    }

    #[test]
    fn unrelated_error_is_not_auth_error() {
        assert!(!is_auth_error(&anyhow::anyhow!("some other failure")));
        // a missing-dataset error must not be misread as auth
        assert!(!is_auth_error(&anyhow::anyhow!(
            "LanceError: DatasetNotFound, no such file"
        )));
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
