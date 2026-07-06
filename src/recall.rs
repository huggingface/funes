//! The read surface: `recall`, `list`, `get`, `status` over the existing index.
//! Recall pipeline: hybrid (vector + BM25, fused by reciprocal rank) → cross-encoder rerank →
//! recency reweight → neighbor expansion. Every command returns a `String` so the CLI
//! prints it and the MCP server returns it verbatim.

use crate::chunk;
use crate::dataset;
use crate::harness::Harness;
use crate::hello;
use crate::hub::{self, Reachability, Store};
use anyhow::{anyhow, Context, Result};
use arrow_array::{Float32Array, Int64Array, RecordBatch, StringArray, UInt64Array};
use chrono::{DateTime, Utc};
use fastembed::{EmbeddingModel, InitOptions, RerankInitOptions, RerankerModel, TextEmbedding, TextRerank};
use futures::TryStreamExt;
use lance::dataset::{Dataset, ROW_ID};
use lance::Error as LanceError;
use lance_index::scalar::FullTextSearchQuery;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use tokio::sync::{Mutex, OnceCell};

/// Columns a [`Hit`] needs from a search scan.
const HIT_COLS: &[&str] = &[
    "text",
    "session_id",
    "project",
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
struct Neighbor {
    seq: i64,
    role: String,
    block_type: String,
    text: String,
}

/// One candidate row carried from retrieval through rerank to display.
struct Hit {
    text: String,
    session_id: String,
    project: String,
    turn_uuid: String,
    seq: i64,
    ts: String,
    block_type: String,
    harness: String,
    neighbors: Vec<Neighbor>,
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

/// `block_type = '…' AND project = '…' AND harness = '…'` over whichever filters are set, else None.
fn build_where(block_type: Option<&str>, project: Option<&str>, harness: Option<&str>) -> Option<String> {
    let mut clauses = Vec::new();
    if let Some(bt) = block_type {
        clauses.push(format!("block_type = '{}'", esc(bt)));
    }
    if let Some(p) = project {
        clauses.push(format!("project = '{}'", esc(p)));
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

/// A dataset opened for reading, plus an optional temp dir keeping a built-in fallback alive.
struct Read {
    /// Keeps the hello-world temp dir alive for the dataset's lifetime; `None` for a real store.
    _hello: Option<tempfile::TempDir>,
    ds: Dataset,
    /// A degradation note to prepend to the command's output (e.g. the remote was unreachable);
    /// `None` when the requested store opened normally.
    note: Option<String>,
}

/// The outcome of resolving a store for reading. The embedder-backed fallbacks (`Offline` → the
/// local index, `NoIndex` → the built-in guide) are this module's to apply, so the resolver stays
/// free of model/`hello` concerns; a missing or empty remote is a hard error with a clear message.
// A transient return value, never stored en masse, so the `Ready(Dataset)`/unit size gap is fine —
// boxing would only add indirection.
#[allow(clippy::large_enum_variant)]
enum ReadOutcome {
    /// Opened and ready to query.
    Ready(Dataset),
    /// The remote is unreachable — recall from the local index instead.
    Offline,
    /// No local index yet — show the built-in guide.
    NoIndex,
}

/// Resolve a store for reading — the one place every source state is handled. An offline remote
/// degrades (`Offline`); a missing or empty remote errors with a clear message; an absent default
/// local index degrades (`NoIndex`); a present store opens (`Ready`). All read verbs route through
/// this; the remote-state classification and messages come from `hub`.
async fn open_for_read(store: &Store) -> Result<ReadOutcome> {
    if let Store::Remote { uri } = store {
        match hub::remote_reachability(uri).await {
            Reachability::Offline => return Ok(ReadOutcome::Offline),
            Reachability::Missing => return Err(hub::missing_remote(uri)),
            Reachability::Ok => {}
        }
    }
    match store.open().await {
        Ok(ds) => Ok(ReadOutcome::Ready(ds)),
        // The default local store with no index yet → the built-in guide (built below).
        Err(_) if store.is_default_local() => Ok(ReadOutcome::NoIndex),
        // The Hub refused the read on auth (401/403): a clear message beats lance's opendal dump.
        Err(e) if is_auth_error(&e) => match store {
            Store::Remote { uri } => Err(hub::unauthorized_remote(uri)),
            Store::Local { .. } => Err(e),
        },
        // Opened to nothing: a reachable remote never pushed to, or a local path with no dataset.
        // Either way, a clear message beats lance's internal path error.
        Err(e) if is_missing_dataset(&e) => match store {
            Store::Remote { uri } => Err(hub::empty_remote(uri)),
            Store::Local { path } => Err(anyhow::anyhow!("no index found at {}", path.display())),
        },
        Err(e) => Err(e),
    }
}

/// True if `e` is lance reporting the `chunks.lance` dataset isn't there — the store opened to no
/// index (an empty or never-pushed remote, or a path with no dataset). Lets reads report an empty
/// store instead of leaking lance's internal path/`_versions` error.
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

/// Open a store for reading, applying the embedder-backed fallbacks [`open_for_read`] leaves to
/// the caller: an unreachable remote degrades to the local index (then the built-in guide), and an
/// absent local index serves the guide — so recall keeps working offline and on a fresh install. A
/// missing or empty remote surfaces as an error from the helper. Passing `embedder` gives the
/// built-in corpus real vectors for search (recall); `None` suits `get`/`list`.
async fn open_read(store: &Store, embedder: Option<&mut TextEmbedding>) -> Result<Read> {
    match open_for_read(store).await? {
        ReadOutcome::Ready(ds) => Ok(Read {
            _hello: None,
            ds,
            note: None,
        }),
        ReadOutcome::Offline => degrade_offline(&store.label(), embedder).await,
        ReadOutcome::NoIndex => {
            let (dir, ds) = hello::dataset(embedder).await?;
            Ok(Read {
                _hello: Some(dir),
                ds,
                note: None,
            })
        }
    }
}

/// An unreachable remote degrades to the local index, or to the built-in guide if there's no local
/// index either, carrying a note that explains what happened.
async fn degrade_offline(uri: &str, embedder: Option<&mut TextEmbedding>) -> Result<Read> {
    match open_for_read(&Store::local()).await {
        Ok(ReadOutcome::Ready(ds)) => Ok(Read {
            _hello: None,
            ds,
            note: Some(format!("remote {uri} unreachable — recalling from your local store\n")),
        }),
        _ => {
            let (dir, ds) = hello::dataset(embedder).await?;
            Ok(Read {
                _hello: Some(dir),
                ds,
                note: Some(format!(
                    "remote {uri} unreachable and no local store yet — showing the built-in guide\n"
                )),
            })
        }
    }
}

/// The embedder + reranker, loaded once and shared. Loading them (ONNX init) is the costly part of
/// a recall, so a long-lived process — the MCP server — pays it on the first call and reuses them
/// after. The `Mutex` serializes recalls (both models run with `&mut`), which is fine: the work is
/// CPU-bound and the server's calls are serial anyway.
struct Models {
    embedder: TextEmbedding,
    reranker: TextRerank,
}

static MODELS: OnceCell<Mutex<Models>> = OnceCell::const_new();

/// The shared model cache, built on first use.
async fn models() -> Result<&'static Mutex<Models>> {
    MODELS
        .get_or_try_init(|| async {
            let embedder = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))?;
            let reranker = TextRerank::try_new(RerankInitOptions::new(RerankerModel::BGERerankerBase))?;
            Ok::<_, anyhow::Error>(Mutex::new(Models { embedder, reranker }))
        })
        .await
}

/// Run the recall pipeline over one store and return the formatted results as text.
#[allow(clippy::too_many_arguments)]
pub async fn recall(
    store: Store,
    query: String,
    k: usize,
    candidates: usize,
    half_life: f64,
    neighbors: i64,
    block_type: Option<String>,
    project: Option<String>,
    harness: Option<String>,
) -> Result<String> {
    // `--harness` accepts the same spellings as `index`/`add` (claude|codex|pi); normalize to the
    // stored facet (Claude's is `claude_code`) so `--harness claude` filters instead of silently
    // matching nothing, and an unknown value errors here rather than returning zero hits.
    let harness = harness
        .map(|h| Harness::parse(&h))
        .transpose()?
        .map(|h| h.as_str().to_string());

    let mut guard = models().await?.lock().await;
    let Models { embedder, reranker } = &mut *guard;

    let qv: Vec<f32> = embedder
        .embed(vec![query.clone()], None)?
        .into_iter()
        .next()
        .context("empty embedding")?;

    let read = open_read(&store, Some(&mut *embedder)).await?;
    let note = read.note.clone().unwrap_or_default();
    let ds = &read.ds;
    // A `--harness` filter needs the column; on an un-migrated store it would fail deep inside Lance
    // with an opaque schema error, so refuse with a clear message instead.
    if harness.is_some() && !has_harness_col(ds) {
        return Err(anyhow!(
            "this store predates the harness facet — reindex it, or drop --harness"
        ));
    }
    let where_clause = build_where(block_type.as_deref(), project.as_deref(), harness.as_deref());

    // Hybrid retrieval: a vector ANN scan and a BM25 scan, fused by reciprocal rank. The FTS index
    // can be absent (it's best-effort at index time), so the FTS leg is skipped when it errors —
    // recall then falls back to vector-only.
    let hits = hybrid_candidates(ds, &qv, &query, candidates, where_clause.as_deref()).await?;
    if hits.is_empty() {
        return Ok(format!("{note}no results"));
    }

    let docs: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
    let reranked = reranker.rerank(query.as_str(), docs, false, None)?;

    let now = Utc::now();
    let mut scored: Vec<(usize, f64)> = reranked
        .iter()
        .map(|r| {
            let relevance = 1.0 / (1.0 + (-(r.score as f64)).exp());
            (r.index, relevance * recency_weight(&hits[r.index].ts, now, half_life))
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
        let mut refs: Vec<&mut Hit> = top.iter_mut().map(|(h, _)| h).collect();
        attach_neighbors(ds, &mut refs, neighbors).await?;
    }

    let mut out = note;
    for (h, score) in &top {
        let s8 = &h.session_id[..h.session_id.len().min(8)];
        let _ = writeln!(
            out,
            "[{}] {} {}/{} {}  score={:.3}",
            h.ts, h.harness, h.project, s8, h.block_type, score
        );
        let _ = writeln!(out, "  → get {} {}", h.session_id, h.turn_uuid);
        let preview: String = h.text.chars().take(400).collect();
        let _ = writeln!(out, "{preview}");
        for n in &h.neighbors {
            let np: String = n.text.chars().take(160).collect();
            let _ = writeln!(out, "  ~ [{} {} seq{}] {}", n.role, n.block_type, n.seq, np);
        }
        let _ = writeln!(out, "---");
    }
    Ok(out)
}

/// Vector ANN + BM25 candidates fused by reciprocal rank, top `candidates`. The FTS leg is
/// best-effort: a store with no FTS index makes that scan error, and we fall back to vector-only.
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
        // nearest rows. A selective `--project`/`--type` would otherwise drop most (or all) of a
        // globally-nearest pool, returning far fewer than `limit` hits even when matches exist.
        scan.prefilter(true);
        scan.filter(f)?;
    }
    scan.project(&hit_cols(ds))?;
    scan.with_row_id();
    collect_hits(scan).await
}

/// Top-`limit` rows by BM25 score, each with its `_rowid`. Errors if the store has no FTS index.
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

/// Whether the store carries the `harness` column — false for one built before the facet existed
/// (an un-migrated store).
fn has_harness_col(ds: &Dataset) -> bool {
    arrow_schema::Schema::from(ds.schema())
        .column_with_name("harness")
        .is_some()
}

/// `HIT_COLS`, minus `harness` on an un-migrated store: projecting a column the dataset lacks errors,
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
            scol(&batch, "project"),
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
                    project: sval(proj, i),
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

/// Browse indexed sessions: one line per session, newest activity first.
pub async fn list(store: Store, project: Option<String>, limit: usize) -> Result<String> {
    let read = open_read(&store, None).await?;
    let note = read.note.clone().unwrap_or_default();
    let ds = &read.ds;

    let cols = ["session_id", "project", "ts", "role", "text"];
    let filter = project.as_deref().map(|p| format!("project = '{}'", esc(p)));
    let batches = dataset::scan_rows(ds, &cols, filter.as_deref(), None).await?;

    struct Sess {
        project: String,
        chunks: u64,
        first_ts: String,
        last_ts: String,
        first_user: Option<String>,
    }
    let mut sessions: BTreeMap<String, Sess> = BTreeMap::new();
    for batch in batches {
        let (sess, proj, ts, role, text) = (
            scol(&batch, "session_id"),
            scol(&batch, "project"),
            scol(&batch, "ts"),
            scol(&batch, "role"),
            scol(&batch, "text"),
        );
        for i in 0..batch.num_rows() {
            let sid = sval(sess, i);
            let ts_i = sval(ts, i);
            let s = sessions.entry(sid).or_insert_with(|| Sess {
                project: sval(proj, i),
                chunks: 0,
                first_ts: ts_i.clone(),
                last_ts: ts_i.clone(),
                first_user: None,
            });
            s.chunks += 1;
            if !ts_i.is_empty() {
                if s.first_ts.is_empty() || ts_i < s.first_ts {
                    s.first_ts = ts_i.clone();
                }
                if ts_i > s.last_ts {
                    s.last_ts = ts_i.clone();
                }
            }
            if s.first_user.is_none() && sval(role, i) == "user" {
                s.first_user = Some(sval(text, i).chars().take(120).collect());
            }
        }
    }

    let mut rows: Vec<(String, Sess)> = sessions.into_iter().collect();
    rows.sort_by(|a, b| b.1.last_ts.cmp(&a.1.last_ts));
    rows.truncate(limit);

    let mut out = String::new();
    for (sid, s) in &rows {
        let s8 = &sid[..sid.len().min(8)];
        let _ = writeln!(
            out,
            "[{}] {}/{}  chunks={}  {}",
            s.last_ts,
            s.project,
            s8,
            s.chunks,
            s.first_user.as_deref().unwrap_or("")
        );
    }
    if out.is_empty() {
        out.push_str("no sessions\n");
    }
    Ok(format!("{note}{out}"))
}

/// Drill down on a recall hit: the named turn plus the turns within `window` of it, each
/// reassembled (blocks in order, splits de-overlapped) into one readable passage.
pub async fn get(store: Store, session_id: String, turn_uuid: String, window: i64) -> Result<String> {
    let read = open_read(&store, None).await?;
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
        None => return Ok(format!("{note}turn {turn_uuid} not found in session {session_id}\n")),
    };

    // Group selected rows by (seq, turn_uuid).
    let mut groups: BTreeMap<(i64, String), Vec<&TurnRow>> = BTreeMap::new();
    for r in rows.iter().filter(|r| (r.0 - center).abs() <= window) {
        groups.entry((r.0, r.1.clone())).or_default().push(r);
    }

    let mut out = String::new();
    for ((seq, turn), mut chunks) in groups {
        chunks.sort_by_key(|r| (r.4, r.5)); // block_idx, split_idx
        let head = chunks[0];
        // Reassemble blocks: consecutive splits of one block_idx are stitched; a new
        // block_idx starts a new block.
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
        let _ = writeln!(out, "[{}] {} seq{} turn={}", head.2, head.3, seq, turn);
        let _ = writeln!(out, "{}", blocks.join("\n\n"));
        let _ = writeln!(out, "---");
    }
    Ok(format!("{note}{out}"))
}

pub async fn status(store: Store) -> Result<String> {
    match open_for_read(&store).await? {
        ReadOutcome::Ready(ds) => {
            let rows = ds.count_rows(None).await?;
            Ok(format!(
                "store: {}\ntable:  {}\nchunks: {rows}\n",
                store.label(),
                dataset::TABLE
            ))
        }
        // An unreachable remote shows the local index's status instead, like the read commands.
        ReadOutcome::Offline => {
            let body = Box::pin(status(Store::local())).await?;
            Ok(format!(
                "remote {} unreachable — showing your local store instead\n{body}",
                store.label()
            ))
        }
        // No personal index yet: point at `funes index` instead of erroring. (recall/get/list
        // quietly serve the built-in guide in the same situation.)
        ReadOutcome::NoIndex => Ok(format!(
            "store: {}\nno personal store yet — showing the built-in guide ({} passages).\nrun `funes index` to index ~/.claude/projects, then recall your own history.\n",
            store.label(),
            hello::PASSAGES.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[tokio::test]
    async fn missing_dataset_is_detected() {
        // Opening a path with no dataset is lance's DatasetNotFound — the empty/absent case.
        let err = dataset::open("/nonexistent/funes-empty-store/chunks.lance", HashMap::new())
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
    fn build_where_combines_set_filters() {
        assert_eq!(build_where(None, None, None), None);
        assert_eq!(
            build_where(Some("text"), None, None).as_deref(),
            Some("block_type = 'text'")
        );
        assert_eq!(
            build_where(None, Some("proj"), None).as_deref(),
            Some("project = 'proj'")
        );
        assert_eq!(
            build_where(None, None, Some("codex")).as_deref(),
            Some("harness = 'codex'")
        );
        assert_eq!(
            build_where(Some("tool_use"), Some("proj"), Some("pi")).as_deref(),
            Some("block_type = 'tool_use' AND project = 'proj' AND harness = 'pi'")
        );
        // values are escaped against filter-string injection.
        assert_eq!(
            build_where(None, Some("a'b"), None).as_deref(),
            Some("project = 'a''b'")
        );
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
