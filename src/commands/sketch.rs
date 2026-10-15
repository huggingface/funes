//! `funes sketch`: what a session contains, without a query.
//!
//! [`select`] chooses the passages most distinctive within the session, from the vectors already
//! stored; [`cache`] keeps the sketch of a whole session, so a second pass over it costs nothing.
//! Building a sketch writes nothing to the memory.

mod cache;
mod select;

use super::target::Target;
use crate::home;
use crate::memory::dataset;
use crate::memory::Memory;
use crate::ui::render;
use anyhow::{bail, Result};
use lance::Dataset;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Places a sketch shows when the caller doesn't say.
const DEFAULT_UNITS: usize = 8;

/// Characters a sketch renders when the caller doesn't say — cheap by default, since a caller can
/// always ask for more and an over-fetch is paid for by whoever reads the reply.
const DEFAULT_CHARS: usize = 8_000;

/// The version of the [`SessionSketch`] the cache stores: an entry of another is built again.
const SKETCH_SCHEMA_VERSION: u32 = 1;

/// Digest one session, rendered in the agent format. `units` and `max_chars` are requests: both are
/// clamped to what a reply can carry, and any narrowing is reported rather than applied quietly.
pub async fn run(
    target: impl Into<Target>,
    session_id: String,
    from: Option<i64>,
    to: Option<i64>,
    units: Option<usize>,
    max_chars: Option<usize>,
) -> Result<String> {
    let target = target.into();
    let hint = target.hint();
    let memory = target.holder(&session_id).await?;
    let asked = SketchOptions {
        budget: units.unwrap_or(DEFAULT_UNITS),
        char_budget: max_chars.unwrap_or(DEFAULT_CHARS),
    };
    let (options, clamped) = asked.clamp();
    // A whole-session digest is deterministic in the session's content, so it is worth caching: a
    // second pass over the same candidate costs nothing. A window is not cached — a cache entry
    // keyed on the session would otherwise answer for a stretch of it.
    let sketch = if from.is_none() && to.is_none() {
        let mut batch = generate_many_cached(&memory, std::slice::from_ref(&session_id), options).await?;
        let failure = batch.failures.remove(&session_id);
        match batch.sketches.remove(&session_id) {
            Some(sketch) => sketch,
            None => bail!(
                "{}",
                failure.unwrap_or_else(|| format!("no session {session_id} in {}", memory.label()))
            ),
        }
    } else {
        generate(&memory, &session_id, from, to, options).await?
    };
    if sketch.evidence.is_empty() {
        bail!("nothing to sketch in session {session_id} — it holds no text worth selecting");
    }
    Ok(render::sketch_agent("", &hint, &sketch, clamped))
}

/// What a sketch is asked for: places to select, and characters they may render.
#[derive(Clone, Copy, Debug)]
struct SketchOptions {
    budget: usize,
    char_budget: usize,
}

/// The widest and cheapest a sketch may be asked to be. The upper bounds are a reply ceiling: past
/// them the answer is one no caller receives, so a larger request is clamped and the clamp reported.
const UNITS_MAX: usize = 40;
const CHARS_MAX: usize = 40_000;
const CHARS_MIN: usize = 1_000;

/// What a request was narrowed to, so a caller that asked for more learns it did not get it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clamped {
    pub units: Option<usize>,
    pub chars: Option<usize>,
}

impl SketchOptions {
    /// Bring a request inside the bounds, reporting each value that moved. `units` is additionally
    /// held to what the character budget can render at [`select::MIN_UNIT_CHARS`] apiece: asking
    /// for forty places inside a thousand characters would return slivers, not evidence.
    fn clamp(self) -> (Self, Clamped) {
        let chars = self.char_budget.clamp(CHARS_MIN, CHARS_MAX);
        let fits = (chars / select::MIN_UNIT_CHARS).max(1);
        let units = self.budget.clamp(1, UNITS_MAX.min(fits));
        (
            SketchOptions {
                budget: units,
                char_budget: chars,
            },
            Clamped {
                units: (units != self.budget).then_some(units),
                chars: (chars != self.char_budget).then_some(chars),
            },
        )
    }
}

/// A stored row of a sketched session, as [`read_sessions`] reads it.
#[derive(Clone)]
struct RawRow {
    session_id: String,
    id: String,
    text: String,
    turn_uuid: String,
    seq: i64,
    ts: String,
    role: String,
    block_type: String,
    tool_name: String,
    block_idx: i64,
    split_idx: i64,
    vector: Option<Vec<f32>>,
}

/// Per-session results from one shared memory scan. One malformed session does not hide sketches
/// for the others; its error is recorded in `failures` instead.
#[derive(Default)]
struct SketchBatch {
    sketches: HashMap<String, SessionSketch>,
    failures: HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSketch {
    pub schema_version: u32,
    pub selector_version: String,
    pub memory: String,
    pub session_id: String,
    pub source_fingerprint: String,
    pub embedding_fingerprint: String,
    pub source_chunks: usize,
    pub eligible_units: usize,
    pub evidence: Vec<Evidence>,
    pub diagnostics: Diagnostics,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Evidence {
    pub id: String,
    pub turn_uuid: String,
    pub seq: i64,
    pub block_idx: i64,
    pub ts: String,
    pub role: String,
    pub block_type: String,
    pub tool_name: Option<String>,
    pub reasons: Vec<String>,
    pub truncated: bool,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Diagnostics {
    pub axes: usize,
    pub transitions: usize,
    pub near_duplicate_groups: usize,
    pub duplicate_strategy: String,
    pub duplicate_vector_comparisons: usize,
    pub candidates: usize,
    pub rendered_characters: usize,
    pub budget: usize,
    pub char_budget: usize,
    pub elapsed_ms: u128,
    pub fallback: Option<String>,
}

/// Build one sketch of a session, or of the `from`..`to` window of it. Reads the memory and nothing
/// else. `options` is taken as asked: clamp it with [`SketchOptions::clamp`] first if it came from a
/// caller, so the narrowing can be reported.
async fn generate(
    memory: &Memory,
    session_id: &str,
    from: Option<i64>,
    to: Option<i64>,
    options: SketchOptions,
) -> Result<SessionSketch> {
    let ds = memory.open().await?;
    let rows = read_sessions(&ds, &[session_id.to_string()]).await?;
    if rows.is_empty() {
        bail!("no session {session_id} in {}", memory.label());
    }
    let turns = rows.iter().map(|r| r.seq).collect::<HashSet<_>>().len();
    let rows: Vec<RawRow> = rows
        .into_iter()
        .filter(|r| from.is_none_or(|f| r.seq >= f) && to.is_none_or(|t| r.seq <= t))
        .collect();
    if rows.is_empty() {
        bail!("no turns in that range of session {session_id} (it holds {turns})");
    }
    select::build_sketch(
        memory.label(),
        session_id.to_string(),
        rows,
        embedding_fingerprint(&ds),
        options.budget,
        options.char_budget,
        Instant::now(),
    )
}

/// Build or reuse sketches of whole sessions from one dataset scan, cached under funes's home. A
/// session's own failure is recorded in the batch; opening or scanning the memory fails the whole
/// call. `options` is taken as asked, as in [`generate`].
async fn generate_many_cached(memory: &Memory, session_ids: &[String], options: SketchOptions) -> Result<SketchBatch> {
    if session_ids.is_empty() {
        return Ok(SketchBatch::default());
    }

    let cache_root = home::sketch_cache_dir();
    let ds = memory.open().await?;
    let embedding_fingerprint = embedding_fingerprint(&ds);
    let rows = read_sessions(&ds, session_ids).await?;
    let mut by_session: HashMap<String, Vec<RawRow>> = HashMap::new();
    for row in rows {
        by_session.entry(row.session_id.clone()).or_default().push(row);
    }

    let mut batch = SketchBatch::default();
    let memory_label = memory.label();
    for session_id in session_ids {
        let Some(rows) = by_session.remove(session_id) else {
            batch
                .failures
                .insert(session_id.clone(), format!("no session {session_id} in {memory_label}"));
            continue;
        };

        let source_fingerprint = select::fingerprint(&rows);
        let cache_path = cache::cache_path(&cache_root, &memory_label, session_id);
        if let Some(sketch) = cache::load_cached_sketch(
            &cache_path,
            &memory_label,
            session_id,
            &source_fingerprint,
            &embedding_fingerprint,
            options,
        ) {
            batch.sketches.insert(session_id.clone(), sketch);
            continue;
        }

        match select::build_sketch(
            memory_label.clone(),
            session_id.clone(),
            rows,
            embedding_fingerprint.clone(),
            options.budget,
            options.char_budget,
            Instant::now(),
        ) {
            Ok(sketch) => {
                let _ = cache::store_cached_sketch(&cache_path, &sketch);
                batch.sketches.insert(session_id.clone(), sketch);
            }
            Err(error) => {
                batch.failures.insert(session_id.clone(), format!("{error:#}"));
            }
        }
    }
    Ok(batch)
}

async fn read_sessions(ds: &Dataset, sessions: &[String]) -> Result<Vec<RawRow>> {
    let columns = [
        "session_id",
        "id",
        "text",
        "turn_uuid",
        "seq",
        "ts",
        "role",
        "block_type",
        "tool_name",
        "block_idx",
        "split_idx",
        "vector",
    ];
    let filter = dataset::in_list("session_id", sessions.iter().map(String::as_str));
    let batches = dataset::scan_rows(ds, &columns, Some(&filter), None).await?;
    let mut rows = Vec::new();
    for batch in &batches {
        let s = |name: &str| dataset::strs(batch, name);
        let n = |name: &str| dataset::ints(batch, name);
        let (session_id, id, text, turn, ts, role, block_type, tool_name) = (
            s("session_id"),
            s("id"),
            s("text"),
            s("turn_uuid"),
            s("ts"),
            s("role"),
            s("block_type"),
            s("tool_name"),
        );
        let (seq, block_idx, split_idx) = (n("seq"), n("block_idx"), n("split_idx"));
        let vectors = dataset::vectors(batch, "vector");
        for i in 0..batch.num_rows() {
            rows.push(RawRow {
                session_id: session_id.get(i).to_string(),
                id: id.get(i).to_string(),
                text: text.get(i).to_string(),
                turn_uuid: turn.get(i).to_string(),
                seq: seq.get(i),
                ts: ts.get(i).to_string(),
                role: role.get(i).to_string(),
                block_type: block_type.get(i).to_string(),
                tool_name: tool_name.get(i).to_string(),
                block_idx: block_idx.get(i),
                split_idx: split_idx.get(i),
                vector: vectors.get(i),
            });
        }
    }
    rows.sort_by(|a, b| {
        (&a.session_id, a.seq, &a.turn_uuid, a.block_idx, a.split_idx, &a.id).cmp(&(
            &b.session_id,
            b.seq,
            &b.turn_uuid,
            b.block_idx,
            b.split_idx,
            &b.id,
        ))
    });
    Ok(rows)
}

/// The embedding model a cached sketch's vectors came from, as a cache key.
fn embedding_fingerprint(ds: &Dataset) -> String {
    match dataset::embedding_model(ds) {
        Ok(model) => format!("embedding_model:{}", model.id()),
        Err(_) => "unrecorded".to_string(),
    }
}
