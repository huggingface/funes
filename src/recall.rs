//! The read surface: `recall`, `list`, `get`, `status` over the existing index.
//! Recall pipeline: hybrid (vector + BM25, fused by lancedb) → cross-encoder rerank →
//! recency reweight → neighbor expansion. Every command returns a `String` so the CLI
//! prints it and the MCP server returns it verbatim.

use crate::db;
use anyhow::{Context, Result};
use arrow_array::{Int64Array, RecordBatch, StringArray};
use chrono::{DateTime, Utc};
use fastembed::{EmbeddingModel, InitOptions, RerankInitOptions, RerankerModel, TextEmbedding, TextRerank};
use futures::TryStreamExt;
use lance_index::scalar::FullTextSearchQuery;
use lancedb::query::{ExecutableQuery, QueryBase, QueryExecutionOptions, Select};
use lancedb::Table;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A plain scan reads the whole matching set; Lance has no "no limit", so cap high.
const SCAN_LIMIT: usize = 10_000_000;

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

/// `block_type = '…' AND project = '…'` over whichever filters are set, else None.
fn build_where(block_type: Option<&str>, project: Option<&str>) -> Option<String> {
    let mut clauses = Vec::new();
    if let Some(bt) = block_type {
        clauses.push(format!("block_type = '{}'", esc(bt)));
    }
    if let Some(p) = project {
        clauses.push(format!("project = '{}'", esc(p)));
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

/// Join two consecutive splits of one block, dropping the overlapping region (the suffix
/// of `a` that equals the prefix of `b`). Falls back to a plain concat. Char-indexed.
fn stitch(a: &str, b: &str) -> String {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let max_k = ac.len().min(bc.len()).min(300);
    for k in (1..=max_k).rev() {
        if ac[ac.len() - k..] == bc[..k] {
            return ac.iter().chain(bc[k..].iter()).collect();
        }
    }
    format!("{a}{b}")
}

/// Run the recall pipeline and return the formatted results as text.
pub async fn recall(
    query: String,
    k: usize,
    candidates: usize,
    half_life: f64,
    neighbors: i64,
    block_type: Option<String>,
    project: Option<String>,
) -> Result<String> {
    let mut embedder = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))?;
    let qv: Vec<f32> = embedder
        .embed(vec![query.clone()], None)?
        .into_iter()
        .next()
        .context("empty embedding")?;

    let db = db::open_db().await?;
    let table = db.open_table(db::TABLE).execute().await?;

    let where_clause = build_where(block_type.as_deref(), project.as_deref());
    let mut q = table
        .query()
        .full_text_search(FullTextSearchQuery::new(query.clone()))
        .nearest_to(qv)?
        .limit(candidates);
    if let Some(w) = &where_clause {
        q = q.only_if(w);
    }
    let mut stream = q.execute_hybrid(QueryExecutionOptions::default()).await?;

    let mut hits: Vec<Hit> = Vec::new();
    while let Some(batch) = stream.try_next().await? {
        let (text, sess, proj, turn, ts, bt) = (
            scol(&batch, "text"),
            scol(&batch, "session_id"),
            scol(&batch, "project"),
            scol(&batch, "turn_uuid"),
            scol(&batch, "ts"),
            scol(&batch, "block_type"),
        );
        let seq = icol(&batch, "seq");
        for i in 0..batch.num_rows() {
            hits.push(Hit {
                text: sval(text, i),
                session_id: sval(sess, i),
                project: sval(proj, i),
                turn_uuid: sval(turn, i),
                seq: ival(seq, i),
                ts: sval(ts, i),
                block_type: sval(bt, i),
                neighbors: Vec::new(),
            });
        }
    }
    if hits.is_empty() {
        return Ok("no results".to_string());
    }

    let mut reranker = TextRerank::try_new(RerankInitOptions::new(RerankerModel::BGERerankerBase))?;
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
        attach_neighbors(&table, &mut refs, neighbors).await?;
    }

    let mut out = String::new();
    for (h, score) in &top {
        let s8 = &h.session_id[..h.session_id.len().min(8)];
        let _ = writeln!(
            out,
            "[{}] {}/{} {}  score={:.3}",
            h.ts, h.project, s8, h.block_type, score
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

/// For each hit, pull chunks in the same session within `window` of its seq (excluding the
/// hit's own turn) as surrounding context. One combined scan covers every hit.
async fn attach_neighbors(table: &Table, hits: &mut [&mut Hit], window: i64) -> Result<()> {
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
    let mut stream = table
        .query()
        .only_if(pred)
        .select(Select::columns(&cols))
        .limit(SCAN_LIMIT)
        .execute()
        .await?;

    let mut rows: Vec<NeighborRow> = Vec::new();
    while let Some(batch) = stream.try_next().await? {
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
pub async fn list(project: Option<String>, limit: usize) -> Result<String> {
    let db = db::open_db().await?;
    let table = db.open_table(db::TABLE).execute().await?;

    let cols = ["session_id", "project", "ts", "role", "text"];
    let mut q = table.query().select(Select::columns(&cols)).limit(SCAN_LIMIT);
    if let Some(p) = &project {
        q = q.only_if(format!("project = '{}'", esc(p)));
    }
    let mut stream = q.execute().await?;

    struct Sess {
        project: String,
        chunks: u64,
        first_ts: String,
        last_ts: String,
        first_user: Option<String>,
    }
    let mut sessions: BTreeMap<String, Sess> = BTreeMap::new();
    while let Some(batch) = stream.try_next().await? {
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
    Ok(out)
}

/// Drill down on a recall hit: the named turn plus the turns within `window` of it, each
/// reassembled (blocks in order, splits de-overlapped) into one readable passage.
pub async fn get(session_id: String, turn_uuid: String, window: i64) -> Result<String> {
    let db = db::open_db().await?;
    let table = db.open_table(db::TABLE).execute().await?;

    let cols = ["turn_uuid", "seq", "ts", "role", "text", "block_idx", "split_idx"];
    let mut stream = table
        .query()
        .only_if(format!("session_id = '{}'", esc(&session_id)))
        .select(Select::columns(&cols))
        .limit(SCAN_LIMIT)
        .execute()
        .await?;

    // `text` is already the rendered chunk as stored by the indexer — do not re-render.
    let mut rows: Vec<TurnRow> = Vec::new();
    while let Some(batch) = stream.try_next().await? {
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
        None => return Ok(format!("turn {turn_uuid} not found in session {session_id}\n")),
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
                cur = stitch(&cur, piece);
            }
        }
        if !cur.is_empty() {
            blocks.push(cur);
        }
        let _ = writeln!(out, "[{}] {} seq{} turn={}", head.2, head.3, seq, turn);
        let _ = writeln!(out, "{}", blocks.join("\n\n"));
        let _ = writeln!(out, "---");
    }
    Ok(out)
}

pub async fn status() -> Result<String> {
    let db = db::open_db().await?;
    let table = db.open_table(db::TABLE).execute().await?;
    let rows = table.count_rows(None).await?;
    Ok(format!(
        "db:     {}\ntable:  {}\nchunks: {rows}\n",
        db::lancedb_uri(),
        db::TABLE
    ))
}
