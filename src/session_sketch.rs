//! Deterministic session-sketch selection for guided curation.
//!
//! The structured sketch API powers the alternate preview in `funes curate`. Constructing a sketch
//! never writes memory or curation state; the cached entry is a derived local artifact.

use crate::chunk::OVERLAP;
use crate::curate;
use crate::dataset;
use crate::hub::Memory;
use anyhow::{bail, Context, Result};
use arrow_array::{Array, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, StringArray};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

// Initial host validation found that 8/16k retained the complete arc of focused sessions while the
// earlier 12/24k draft mostly admitted secondary transitions. Both remain experimental CLI knobs.
const DEFAULT_BUDGET: usize = 8;
const DEFAULT_CHAR_BUDGET: usize = 16_000;
const TOOL_RESULT_PREVIEW: usize = 2_000;
const NEAR_DUPLICATE: f32 = 0.97;
const VECTOR_EPSILON: f32 = 1e-7;
// Exact all-pairs grouping preserves the original selector on ordinary sessions. Beyond this size,
// deterministic random-hyperplane bands generate a bounded candidate set. At cosine 0.97, eight
// independent 10-bit bands have about a 99% chance of sharing at least one band under the usual
// SimHash model; a chronological window also catches locally repeated harness traffic.
const EXACT_DUPLICATE_MAX_UNITS: usize = 3_000;
const DUPLICATE_LSH_BANDS: usize = 8;
const DUPLICATE_LSH_BITS: usize = 10;
const DUPLICATE_BUCKET_HEAD: usize = 8;
const DUPLICATE_BUCKET_RECENT: usize = 40;
const DUPLICATE_TEMPORAL_WINDOW: usize = 16;
const SKETCH_SCHEMA_VERSION: u32 = 1;
const SELECTOR_VERSION: &str = "session-sketch-v2-experimental";

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

#[derive(Clone)]
struct Unit {
    id: String,
    text: String,
    turn_uuid: String,
    seq: i64,
    ts: String,
    role: String,
    block_type: String,
    tool_name: String,
    block_idx: i64,
    vector: Option<Vec<f32>>,
    quality: f32,
    mass: f32,
}

#[derive(Default)]
struct Candidate {
    reasons: BTreeSet<String>,
    mandatory: bool,
    transition: f32,
}

struct CandidatePool {
    by_unit: HashMap<usize, Candidate>,
    order: Vec<usize>,
}

impl CandidatePool {
    fn new() -> Self {
        Self {
            by_unit: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn add(&mut self, unit: usize, reason: impl Into<String>, mandatory: bool, transition: f32) {
        if !self.by_unit.contains_key(&unit) {
            self.order.push(unit);
        }
        let c = self.by_unit.entry(unit).or_default();
        c.reasons.insert(reason.into());
        c.mandatory |= mandatory;
        c.transition = c.transition.max(transition);
    }

    fn retain_first(&mut self, limit: usize) {
        if self.order.len() <= limit {
            return;
        }
        let keep: HashSet<usize> = self.order.iter().copied().take(limit).collect();
        self.order.truncate(limit);
        self.by_unit.retain(|i, _| keep.contains(i));
    }
}

/// Stable inputs to the deterministic selector. Context neighbors do not count against `budget`.
#[derive(Clone, Copy, Debug)]
pub struct SketchOptions {
    pub budget: usize,
    pub char_budget: usize,
}

impl Default for SketchOptions {
    fn default() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
            char_budget: DEFAULT_CHAR_BUDGET,
        }
    }
}

/// Per-session results from one shared memory scan. One malformed session does not hide sketches
/// for the others; its error is recorded in `failures` instead.
#[derive(Default)]
pub struct SketchBatch {
    pub sketches: HashMap<String, SessionSketch>,
    pub failures: HashMap<String, String>,
    pub cache_outcomes: HashMap<String, CacheOutcome>,
}

/// What one cached generation did. This lets a future detached hook report whether it did useful
/// work without coupling the selector to hook installation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheOutcome {
    /// No prior cache file existed.
    Created,
    /// The source and selector keys matched; the existing sketch was reused.
    Reused,
    /// A prior entry existed but session growth, a rewrite, or selector inputs made it stale.
    Refreshed,
    /// Generation succeeded but the best-effort cache write did not.
    NotStored,
}

/// One hook-friendly cache refresh, scoped to an exact session id.
pub struct SketchRefresh {
    pub sketch: SessionSketch,
    pub outcome: CacheOutcome,
}

#[derive(Deserialize, Serialize)]
pub struct SessionSketch {
    pub schema_version: u32,
    pub selector_version: String,
    pub memory: String,
    pub session_id: String,
    pub source_fingerprint: String,
    pub embedding_fingerprint: String,
    pub source_chunks: usize,
    pub eligible_units: usize,
    pub selected_units: Vec<SelectedUnit>,
    pub evidence: Vec<Evidence>,
    pub diagnostics: Diagnostics,
}

#[derive(Deserialize, Serialize)]
pub struct SelectedUnit {
    pub id: String,
    pub turn_uuid: String,
    pub seq: i64,
    pub block_idx: i64,
    pub reasons: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct Evidence {
    pub id: String,
    pub turn_uuid: String,
    pub seq: i64,
    pub block_idx: i64,
    pub ts: String,
    pub role: String,
    pub block_type: String,
    pub tool_name: Option<String>,
    pub selected: bool,
    pub reasons: Vec<String>,
    pub truncated: bool,
    pub text: String,
}

#[derive(Deserialize, Serialize)]
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

#[derive(Clone)]
struct TurnInfo {
    units: Vec<usize>,
    vector: Option<Vec<f32>>,
}

#[derive(Clone, Copy)]
struct Boundary {
    after_turn: usize,
    score: f32,
}

#[derive(Clone, Copy, Debug)]
struct DuplicateStats {
    groups: usize,
    strategy: &'static str,
    vector_comparisons: usize,
}

/// Build one sketch from an exact session id. This only reads the memory.
pub async fn generate(memory: &Memory, session_id: &str, options: SketchOptions) -> Result<SessionSketch> {
    validate_options(options)?;
    let ds = memory.open().await?;
    let rows = read_session(&ds, session_id).await?;
    if rows.is_empty() {
        bail!("session {session_id} has no stored chunks");
    }
    build_sketch(
        memory.label(),
        session_id.to_string(),
        rows,
        embedding_fingerprint(&ds),
        options.budget,
        options.char_budget,
        Instant::now(),
    )
}

/// Build sketches for several exact session ids from one dataset scan. Session-local selector
/// failures are isolated in the returned batch; opening or scanning the memory still fails the
/// whole operation.
pub async fn generate_many(memory: &Memory, session_ids: &[String], options: SketchOptions) -> Result<SketchBatch> {
    generate_many_inner(memory, session_ids, options, None).await
}

/// Build or reuse sketches for a picker. Cache entries live under the funes home and are accepted
/// only when the complete source/embedding fingerprints, selector version, schema, and budgets
/// match. A corrupt or unwritable cache quietly degrades to deterministic generation.
pub async fn generate_many_cached(
    memory: &Memory,
    session_ids: &[String],
    options: SketchOptions,
) -> Result<SketchBatch> {
    let cache = dataset::funes_dir().join("cache/session-sketch");
    generate_many_inner(memory, session_ids, options, Some(&cache)).await
}

/// Refresh exactly one session's cache entry. A future per-turn hook can call this after indexing
/// the active session without discovering a project or rebuilding sketches for unrelated sessions.
/// The deterministic selector still runs over the complete changed session because an appended
/// turn can change global axes, duplicate groups, transitions, and the closing anchor.
pub async fn refresh_cached(memory: &Memory, session_id: &str, options: SketchOptions) -> Result<SketchRefresh> {
    let mut batch = generate_many_cached(memory, &[session_id.to_string()], options).await?;
    if let Some(error) = batch.failures.remove(session_id) {
        bail!("refreshing session {session_id}: {error}");
    }
    let sketch = batch
        .sketches
        .remove(session_id)
        .with_context(|| format!("refreshing session {session_id} produced no sketch"))?;
    let outcome = batch.cache_outcomes.remove(session_id).unwrap_or(CacheOutcome::Created);
    Ok(SketchRefresh { sketch, outcome })
}

async fn generate_many_inner(
    memory: &Memory,
    session_ids: &[String],
    options: SketchOptions,
    cache_root: Option<&Path>,
) -> Result<SketchBatch> {
    validate_options(options)?;
    if session_ids.is_empty() {
        return Ok(SketchBatch::default());
    }

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
                .insert(session_id.clone(), "session has no stored chunks".to_string());
            continue;
        };

        let source_fingerprint = fingerprint(&rows);
        let cache_path = cache_root.map(|root| cache_path(root, &memory_label, session_id));
        let had_cache = cache_path.as_deref().is_some_and(Path::exists);
        if let Some(sketch) = cache_path.as_deref().and_then(|path| {
            load_cached_sketch(
                path,
                &memory_label,
                session_id,
                &source_fingerprint,
                &embedding_fingerprint,
                options,
            )
        }) {
            batch.sketches.insert(session_id.clone(), sketch);
            batch.cache_outcomes.insert(session_id.clone(), CacheOutcome::Reused);
            continue;
        }

        match build_sketch(
            memory_label.clone(),
            session_id.clone(),
            rows,
            embedding_fingerprint.clone(),
            options.budget,
            options.char_budget,
            Instant::now(),
        ) {
            Ok(sketch) => {
                if let Some(path) = cache_path.as_deref() {
                    let outcome = if store_cached_sketch(path, &sketch).is_err() {
                        CacheOutcome::NotStored
                    } else if had_cache {
                        CacheOutcome::Refreshed
                    } else {
                        CacheOutcome::Created
                    };
                    batch.cache_outcomes.insert(session_id.clone(), outcome);
                }
                batch.sketches.insert(session_id.clone(), sketch);
            }
            Err(error) => {
                batch.failures.insert(session_id.clone(), format!("{error:#}"));
            }
        }
    }
    Ok(batch)
}

fn cache_path(root: &Path, memory: &str, session_id: &str) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(memory.as_bytes());
    hash.update([0]);
    hash.update(session_id.as_bytes());
    root.join(format!("{}.json", hex::encode(hash.finalize())))
}

fn load_cached_sketch(
    path: &Path,
    memory: &str,
    session_id: &str,
    source_fingerprint: &str,
    embedding_fingerprint: &str,
    options: SketchOptions,
) -> Option<SessionSketch> {
    let file = std::fs::File::open(path).ok()?;
    let sketch: SessionSketch = serde_json::from_reader(file).ok()?;
    cache_matches(
        &sketch,
        memory,
        session_id,
        source_fingerprint,
        embedding_fingerprint,
        options,
    )
    .then_some(sketch)
}

fn cache_matches(
    sketch: &SessionSketch,
    memory: &str,
    session_id: &str,
    source_fingerprint: &str,
    embedding_fingerprint: &str,
    options: SketchOptions,
) -> bool {
    sketch.schema_version == SKETCH_SCHEMA_VERSION
        && sketch.selector_version == SELECTOR_VERSION
        && sketch.memory == memory
        && sketch.session_id == session_id
        && sketch.source_fingerprint == source_fingerprint
        && sketch.embedding_fingerprint == embedding_fingerprint
        && sketch.diagnostics.budget == options.budget
        && sketch.diagnostics.char_budget == options.char_budget
}

fn store_cached_sketch(path: &Path, sketch: &SessionSketch) -> Result<()> {
    let parent = path.parent().context("session-sketch cache path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating session-sketch cache at {}", parent.display()))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("staging session-sketch cache in {}", parent.display()))?;
    serde_json::to_writer(&mut staged, sketch).context("serializing session-sketch cache")?;
    staged.flush().context("flushing session-sketch cache")?;
    staged
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replacing session-sketch cache at {}", path.display()))?;
    Ok(())
}

fn validate_options(options: SketchOptions) -> Result<()> {
    if !(3..=32).contains(&options.budget) {
        bail!("sketch budget must be between 3 and 32");
    }
    if !(4_000..=200_000).contains(&options.char_budget) {
        bail!("sketch character budget must be between 4000 and 200000");
    }
    Ok(())
}

async fn read_session(ds: &lance::Dataset, session: &str) -> Result<Vec<RawRow>> {
    read_sessions(ds, &[session.to_string()]).await
}

async fn read_sessions(ds: &lance::Dataset, sessions: &[String]) -> Result<Vec<RawRow>> {
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
    let quoted = sessions
        .iter()
        .map(|session| format!("'{}'", session.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let filter = format!("session_id IN ({quoted})");
    let batches = dataset::scan_rows(ds, &columns, Some(&filter), None).await?;
    let mut rows = Vec::new();
    for batch in &batches {
        let session_id = str_col(batch, "session_id")?;
        let id = str_col(batch, "id")?;
        let text = str_col(batch, "text")?;
        let turn = str_col(batch, "turn_uuid")?;
        let seq = int_col(batch, "seq")?;
        let ts = str_col(batch, "ts")?;
        let role = str_col(batch, "role")?;
        let block_type = str_col(batch, "block_type")?;
        let tool_name = str_col(batch, "tool_name")?;
        let block_idx = int_col(batch, "block_idx")?;
        let split_idx = int_col(batch, "split_idx")?;
        let vectors = batch
            .column_by_name("vector")
            .context("memory has no `vector` column")?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .context("`vector` column is not a fixed-size list")?;
        for i in 0..batch.num_rows() {
            rows.push(RawRow {
                session_id: str_value(session_id, i),
                id: str_value(id, i),
                text: str_value(text, i),
                turn_uuid: str_value(turn, i),
                seq: int_value(seq, i),
                ts: str_value(ts, i),
                role: str_value(role, i),
                block_type: str_value(block_type, i),
                tool_name: str_value(tool_name, i),
                block_idx: int_value(block_idx, i),
                split_idx: int_value(split_idx, i),
                vector: vector_value(vectors, i)?,
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

fn str_col<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    batch
        .column_by_name(name)
        .with_context(|| format!("memory has no `{name}` column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("`{name}` column is not utf8"))
}

fn int_col<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a Int64Array> {
    batch
        .column_by_name(name)
        .with_context(|| format!("memory has no `{name}` column"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .with_context(|| format!("`{name}` column is not int64"))
}

fn str_value(col: &StringArray, i: usize) -> String {
    if col.is_null(i) {
        String::new()
    } else {
        col.value(i).to_string()
    }
}

fn int_value(col: &Int64Array, i: usize) -> i64 {
    if col.is_null(i) {
        0
    } else {
        col.value(i)
    }
}

fn vector_value(col: &FixedSizeListArray, i: usize) -> Result<Option<Vec<f32>>> {
    if col.is_null(i) {
        return Ok(None);
    }
    let value = col.value(i);
    let floats = value
        .as_any()
        .downcast_ref::<Float32Array>()
        .context("`vector` items are not float32")?;
    if floats.null_count() > 0 {
        return Ok(None);
    }
    Ok(Some((0..floats.len()).map(|j| floats.value(j)).collect()))
}

fn embedding_fingerprint(ds: &lance::Dataset) -> String {
    let schema = arrow_schema::Schema::from(ds.schema());
    for key in ["embedding_fingerprint", "embedding_model"] {
        if let Some(value) = schema.metadata().get(key) {
            return format!("{key}:{value}");
        }
    }
    "unrecorded".to_string()
}

fn build_sketch(
    memory: String,
    session_id: String,
    rows: Vec<RawRow>,
    embedding_fingerprint: String,
    budget: usize,
    char_budget: usize,
    started: Instant,
) -> Result<SessionSketch> {
    let source_chunks = rows.len();
    let source_fingerprint = fingerprint(&rows);
    let mut units = reconstruct_units(&rows)?;
    if units.is_empty() {
        bail!("session contains no eligible blocks after dropping thinking and scaffolding");
    }
    let duplicate_stats = assign_duplicate_mass(&mut units);
    let turns = build_turns(&units);

    let all_indices: Vec<usize> = (0..units.len()).collect();
    let all_chars = context_chars(&units, all_indices.iter().copied());
    let (selected, pool, axes, transitions, fallback) = if units.len() <= budget && all_chars <= char_budget {
        let mut pool = CandidatePool::new();
        for i in 0..units.len() {
            pool.add(i, "all_evidence", true, 0.0);
        }
        (
            all_indices,
            pool,
            0,
            0,
            Some("session fits the evidence budgets".to_string()),
        )
    } else {
        let mut pool = CandidatePool::new();
        add_anchors(&units, &mut pool);
        let anchor_count = pool.by_unit.values().filter(|c| c.mandatory).count();
        let axes = discover_axes(&units, budget, anchor_count, &mut pool);
        let transitions = add_transitions(&units, &turns, budget, &mut pool);
        pool.retain_first(budget.saturating_mul(4));
        let selected = select_final(&units, &turns, &pool, budget, char_budget);
        (selected, pool, axes, transitions, None)
    };

    let context = context_for_selected(&units, &turns, &selected);
    let rendered_characters = context_chars(&units, context.iter().copied());
    let selected_set: HashSet<usize> = selected.iter().copied().collect();
    let mut selected_sorted = selected.clone();
    selected_sorted.sort_by_key(|&i| unit_key(&units[i]));
    let selected_units = selected_sorted
        .iter()
        .map(|&i| {
            let candidate = pool.by_unit.get(&i);
            SelectedUnit {
                id: units[i].id.clone(),
                turn_uuid: units[i].turn_uuid.clone(),
                seq: units[i].seq,
                block_idx: units[i].block_idx,
                reasons: candidate
                    .map(|c| c.reasons.iter().cloned().collect())
                    .unwrap_or_else(|| vec!["selected".to_string()]),
            }
        })
        .collect();
    let evidence = context
        .iter()
        .map(|&i| {
            let (text, truncated) = display_text(&units[i]);
            let reasons = pool
                .by_unit
                .get(&i)
                .map(|c| c.reasons.iter().cloned().collect())
                .unwrap_or_default();
            Evidence {
                id: units[i].id.clone(),
                turn_uuid: units[i].turn_uuid.clone(),
                seq: units[i].seq,
                block_idx: units[i].block_idx,
                ts: units[i].ts.clone(),
                role: units[i].role.clone(),
                block_type: units[i].block_type.clone(),
                tool_name: (!units[i].tool_name.is_empty()).then(|| units[i].tool_name.clone()),
                selected: selected_set.contains(&i),
                reasons,
                truncated,
                text,
            }
        })
        .collect();

    Ok(SessionSketch {
        schema_version: SKETCH_SCHEMA_VERSION,
        selector_version: SELECTOR_VERSION.to_string(),
        memory,
        session_id,
        source_fingerprint,
        embedding_fingerprint,
        source_chunks,
        eligible_units: units.len(),
        selected_units,
        evidence,
        diagnostics: Diagnostics {
            axes,
            transitions,
            near_duplicate_groups: duplicate_stats.groups,
            duplicate_strategy: duplicate_stats.strategy.to_string(),
            duplicate_vector_comparisons: duplicate_stats.vector_comparisons,
            candidates: pool.order.len(),
            rendered_characters,
            budget,
            char_budget,
            elapsed_ms: started.elapsed().as_millis(),
            fallback,
        },
    })
}

fn fingerprint(rows: &[RawRow]) -> String {
    let mut hash = Sha256::new();
    for row in rows {
        for field in [
            row.id.as_str(),
            row.text.as_str(),
            row.turn_uuid.as_str(),
            row.ts.as_str(),
            row.role.as_str(),
            row.block_type.as_str(),
            row.tool_name.as_str(),
        ] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field.as_bytes());
        }
        hash.update(row.seq.to_le_bytes());
        hash.update(row.block_idx.to_le_bytes());
        hash.update(row.split_idx.to_le_bytes());
        match &row.vector {
            Some(vector) => {
                hash.update([1]);
                for value in vector {
                    hash.update(value.to_le_bytes());
                }
            }
            None => hash.update([0]),
        }
    }
    format!("sha256:{}", hex::encode(hash.finalize()))
}

fn reconstruct_units(rows: &[RawRow]) -> Result<Vec<Unit>> {
    let mut groups: BTreeMap<(i64, String, i64), Vec<&RawRow>> = BTreeMap::new();
    for row in rows {
        groups
            .entry((row.seq, row.turn_uuid.clone(), row.block_idx))
            .or_default()
            .push(row);
    }
    let mut units = Vec::new();
    for ((_seq, _turn, _block), mut pieces) in groups {
        pieces.sort_by_key(|row| row.split_idx);
        let head = pieces[0];
        let mut text = String::new();
        let dim = pieces.iter().find_map(|r| r.vector.as_ref().map(Vec::len));
        let mut aggregate = dim.map(|d| vec![0.0f32; d]);
        let mut total_weight = 0.0f32;
        for piece in &pieces {
            let overlap = if text.is_empty() {
                0
            } else {
                overlap_len(&text, &piece.text)
            };
            let weight = piece.text.chars().count().saturating_sub(overlap).max(1) as f32;
            if let (Some(sum), Some(vector)) = (&mut aggregate, &piece.vector) {
                if sum.len() != vector.len() {
                    bail!("inconsistent vector dimensions inside block {}", head.id);
                }
                for (dst, src) in sum.iter_mut().zip(vector) {
                    *dst += weight * src;
                }
                total_weight += weight;
            }
            text = if text.is_empty() {
                piece.text.clone()
            } else {
                stitch(&text, &piece.text)
            };
        }
        let vector = aggregate.and_then(|mut vector| {
            if total_weight > 0.0 {
                for value in &mut vector {
                    *value /= total_weight;
                }
            }
            normalize(vector)
        });
        let text = text.trim().to_string();
        if text.is_empty() || head.block_type == "thinking" {
            continue;
        }
        if head.role == "user" && head.block_type == "text" && curate::is_scaffolding(&text) {
            continue;
        }
        if head.block_type == "text" && is_harness_noise(&text) {
            continue;
        }
        let Some(type_weight) = type_weight(&head.block_type) else {
            continue;
        };
        let chars = text.chars().count() as f32;
        let length_factor = (chars / 200.0).sqrt().clamp(0.25, 1.0);
        units.push(Unit {
            id: head.id.clone(),
            text,
            turn_uuid: head.turn_uuid.clone(),
            seq: head.seq,
            ts: head.ts.clone(),
            role: head.role.clone(),
            block_type: head.block_type.clone(),
            tool_name: head.tool_name.clone(),
            block_idx: head.block_idx,
            vector,
            quality: type_weight * length_factor,
            mass: type_weight,
        });
    }
    units.sort_by(|a, b| {
        a.seq
            .cmp(&b.seq)
            .then_with(|| a.turn_uuid.cmp(&b.turn_uuid))
            .then_with(|| a.block_idx.cmp(&b.block_idx))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(units)
}

fn type_weight(block_type: &str) -> Option<f32> {
    match block_type {
        "text" => Some(1.0),
        "tool_use" => Some(0.35),
        "tool_result" => Some(0.20),
        "thinking" => None,
        _ => None,
    }
}

fn is_harness_noise(text: &str) -> bool {
    matches!(
        text.trim(),
        "[Request interrupted by user]" | "[Request interrupted by user for tool use]"
    )
}

fn overlap_len(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let max = left.len().min(right.len()).min(OVERLAP);
    (1..=max)
        .rev()
        .find(|&n| left[left.len() - n..] == right[..n])
        .unwrap_or(0)
}

fn stitch(left: &str, right: &str) -> String {
    let overlap = overlap_len(left, right);
    let suffix: String = right.chars().skip(overlap).collect();
    format!("{left}{suffix}")
}

fn normalize(mut vector: Vec<f32>) -> Option<Vec<f32>> {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= VECTOR_EPSILON {
        return None;
    }
    for value in &mut vector {
        *value /= norm;
    }
    Some(vector)
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn unit_key(unit: &Unit) -> (i64, &str, i64, &str) {
    (unit.seq, &unit.turn_uuid, unit.block_idx, &unit.id)
}

fn assign_duplicate_mass(units: &mut [Unit]) -> DuplicateStats {
    assign_duplicate_mass_with_limit(units, EXACT_DUPLICATE_MAX_UNITS)
}

fn assign_duplicate_mass_with_limit(units: &mut [Unit], exact_limit: usize) -> DuplicateStats {
    let mut dsu = DisjointSet::new(units.len());

    // Text equality is exact, cheap, and must also work for rows without stored embeddings.
    {
        let mut by_text: HashMap<&str, usize> = HashMap::new();
        for (i, unit) in units.iter().enumerate() {
            if let Some(previous) = by_text.insert(&unit.text, i) {
                dsu.union(previous, i);
            }
        }
    }

    let vector_count = units.iter().filter(|unit| unit.vector.is_some()).count();
    let (strategy, vector_comparisons) = if vector_count <= exact_limit {
        ("exact-all-pairs", group_near_duplicates_exact(units, &mut dsu))
    } else {
        ("simhash-8x10-bounded", group_near_duplicates_bounded(units, &mut dsu))
    };

    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for i in 0..units.len() {
        let root = dsu.find(i);
        *sizes.entry(root).or_default() += 1;
    }
    for (i, unit) in units.iter_mut().enumerate() {
        let root = dsu.find(i);
        unit.mass /= sizes[&root] as f32;
    }
    DuplicateStats {
        groups: sizes.values().filter(|&&size| size > 1).count(),
        strategy,
        vector_comparisons,
    }
}

fn group_near_duplicates_exact(units: &[Unit], dsu: &mut DisjointSet) -> usize {
    let mut comparisons = 0;
    for i in 0..units.len() {
        let Some(left) = units[i].vector.as_deref() else {
            continue;
        };
        for (j, unit) in units.iter().enumerate().skip(i + 1) {
            let Some(right) = unit.vector.as_deref() else {
                continue;
            };
            if dsu.find(i) == dsu.find(j) {
                continue;
            }
            comparisons += 1;
            if left.len() == right.len() && dot(left, right) >= NEAR_DUPLICATE {
                dsu.union(i, j);
            }
        }
    }
    comparisons
}

fn group_near_duplicates_bounded(units: &[Unit], dsu: &mut DisjointSet) -> usize {
    let mut planes_by_dimension: HashMap<usize, Vec<Vec<f32>>> = HashMap::new();
    let mut buckets: HashMap<(usize, usize, u16), Vec<usize>> = HashMap::new();
    let mut history_by_dimension: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut comparisons = 0;

    for (i, unit) in units.iter().enumerate() {
        let Some(vector) = unit.vector.as_deref() else {
            continue;
        };
        let dimension = vector.len();
        let keys = {
            let planes = planes_by_dimension
                .entry(dimension)
                .or_insert_with(|| duplicate_hyperplanes(dimension));
            duplicate_band_keys(vector, planes)
        };

        let mut candidates = BTreeSet::new();
        if let Some(history) = history_by_dimension.get(&dimension) {
            candidates.extend(history.iter().rev().take(DUPLICATE_TEMPORAL_WINDOW));
        }
        for (band, &key) in keys.iter().enumerate() {
            if let Some(bucket) = buckets.get(&(dimension, band, key)) {
                candidates.extend(bucket.iter().take(DUPLICATE_BUCKET_HEAD));
                candidates.extend(bucket.iter().rev().take(DUPLICATE_BUCKET_RECENT));
            }
        }

        for j in candidates {
            if dsu.find(i) == dsu.find(j) {
                continue;
            }
            let Some(other) = units[j].vector.as_deref() else {
                continue;
            };
            comparisons += 1;
            if dot(vector, other) >= NEAR_DUPLICATE {
                dsu.union(i, j);
            }
        }

        for (band, key) in keys.into_iter().enumerate() {
            buckets.entry((dimension, band, key)).or_default().push(i);
        }
        history_by_dimension.entry(dimension).or_default().push(i);
    }
    comparisons
}

fn duplicate_hyperplanes(dimension: usize) -> Vec<Vec<f32>> {
    (0..DUPLICATE_LSH_BANDS * DUPLICATE_LSH_BITS)
        .map(|plane| {
            (0..dimension)
                .map(|component| {
                    let seed = (plane as u64)
                        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                        .wrapping_add(component as u64)
                        .wrapping_add(0x6a09_e667_f3bc_c909);
                    if splitmix64(seed) & 1 == 0 {
                        -1.0
                    } else {
                        1.0
                    }
                })
                .collect()
        })
        .collect()
}

fn duplicate_band_keys(vector: &[f32], planes: &[Vec<f32>]) -> [u16; DUPLICATE_LSH_BANDS] {
    let mut keys = [0u16; DUPLICATE_LSH_BANDS];
    for (plane_idx, plane) in planes.iter().enumerate() {
        if dot(vector, plane) >= 0.0 {
            let band = plane_idx / DUPLICATE_LSH_BITS;
            let bit = plane_idx % DUPLICATE_LSH_BITS;
            keys[band] |= 1 << bit;
        }
    }
    keys
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

struct DisjointSet {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl DisjointSet {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            rank: vec![0; len],
        }
    }

    fn find(&mut self, i: usize) -> usize {
        if self.parent[i] != i {
            self.parent[i] = self.find(self.parent[i]);
        }
        self.parent[i]
    }

    fn union(&mut self, left: usize, right: usize) {
        let mut left = self.find(left);
        let mut right = self.find(right);
        if left == right {
            return;
        }
        if self.rank[left] < self.rank[right] {
            std::mem::swap(&mut left, &mut right);
        }
        self.parent[right] = left;
        if self.rank[left] == self.rank[right] {
            self.rank[left] += 1;
        }
    }
}

fn build_turns(units: &[Unit]) -> Vec<TurnInfo> {
    let mut grouped: BTreeMap<(i64, String), Vec<usize>> = BTreeMap::new();
    for (i, unit) in units.iter().enumerate() {
        grouped.entry((unit.seq, unit.turn_uuid.clone())).or_default().push(i);
    }
    grouped
        .into_iter()
        .map(|((_seq, _turn_uuid), indices)| {
            let vector = weighted_mean(
                indices
                    .iter()
                    .filter_map(|&i| units[i].vector.as_deref().map(|v| (units[i].mass, v))),
            );
            TurnInfo { units: indices, vector }
        })
        .collect()
}

fn weighted_mean<'a>(values: impl Iterator<Item = (f32, &'a [f32])>) -> Option<Vec<f32>> {
    let mut aggregate: Option<Vec<f32>> = None;
    let mut total = 0.0f32;
    for (weight, vector) in values {
        let sum = aggregate.get_or_insert_with(|| vec![0.0; vector.len()]);
        if sum.len() != vector.len() {
            continue;
        }
        for (dst, src) in sum.iter_mut().zip(vector) {
            *dst += weight * src;
        }
        total += weight;
    }
    aggregate.and_then(|mut vector| {
        if total > 0.0 {
            for value in &mut vector {
                *value /= total;
            }
        }
        normalize(vector)
    })
}

fn centroid(units: &[Unit]) -> Option<Vec<f32>> {
    let mut aggregate: Option<Vec<f32>> = None;
    let mut total = 0.0f32;
    for unit in units {
        let Some(vector) = unit.vector.as_deref() else {
            continue;
        };
        let sum = aggregate.get_or_insert_with(|| vec![0.0; vector.len()]);
        for (dst, src) in sum.iter_mut().zip(vector) {
            *dst += unit.mass * src;
        }
        total += unit.mass;
    }
    aggregate.map(|mut vector| {
        if total > 0.0 {
            for value in &mut vector {
                *value /= total;
            }
        }
        vector
    })
}

fn add_anchors(units: &[Unit], pool: &mut CandidatePool) {
    if let Some((i, _)) = units
        .iter()
        .enumerate()
        .find(|(_, unit)| unit.role == "user" && unit.block_type == "text")
    {
        pool.add(i, "opening_user", true, 0.0);
    }
    if let Some((i, _)) = units
        .iter()
        .enumerate()
        .rev()
        .find(|(_, unit)| unit.role == "assistant" && unit.block_type == "text")
    {
        pool.add(i, "closing_assistant", true, 0.0);
    }
    if let Some(mu) = centroid(units).and_then(normalize) {
        let mut best: Option<(usize, f32)> = None;
        for (i, unit) in units.iter().enumerate() {
            if unit.block_type != "text" {
                continue;
            }
            let Some(vector) = unit.vector.as_deref() else {
                continue;
            };
            let score = unit.quality * dot(&mu, vector).max(0.0);
            if best.is_none_or(|(_, current)| score > current) {
                best = Some((i, score));
            }
        }
        if let Some((i, _)) = best {
            pool.add(i, "centroid_medoid", true, 0.0);
        }
    }
}

fn discover_axes(units: &[Unit], budget: usize, anchors: usize, pool: &mut CandidatePool) -> usize {
    let Some(mu) = centroid(units) else {
        return 0;
    };
    let geometric: Vec<usize> = units
        .iter()
        .enumerate()
        .filter_map(|(i, unit)| unit.vector.as_ref().map(|_| i))
        .collect();
    if geometric.is_empty() {
        return 0;
    }
    let wanted = ((budget.saturating_sub(anchors)) / 2).clamp(1, 6);
    let mut basis: Vec<Vec<f32>> = Vec::new();
    for axis in 0..wanted {
        let mut pivot: Option<(usize, Vec<f32>, f32)> = None;
        for &i in &geometric {
            let vector = units[i].vector.as_deref().unwrap();
            let mut residual: Vec<f32> = vector.iter().zip(&mu).map(|(x, mean)| x - mean).collect();
            for q in &basis {
                let projection = dot(&residual, q);
                for (value, direction) in residual.iter_mut().zip(q) {
                    *value -= projection * direction;
                }
            }
            let norm = residual.iter().map(|v| v * v).sum::<f32>().sqrt();
            let score = units[i].quality * norm;
            if pivot.as_ref().is_none_or(|(_, _, current)| score > *current) {
                pivot = Some((i, residual, score));
            }
        }
        let Some((_pivot_idx, residual, score)) = pivot else {
            break;
        };
        if !score.is_finite() || score <= VECTOR_EPSILON {
            break;
        }
        let Some(direction) = normalize(residual) else {
            break;
        };
        let positive = extreme(units, &geometric, &mu, &direction, 1.0);
        let negative = extreme(units, &geometric, &mu, &direction, -1.0);
        if let Some(i) = positive {
            pool.add(i, format!("axis_{}_positive", axis + 1), false, 0.0);
        }
        if let Some(i) = negative {
            pool.add(i, format!("axis_{}_negative", axis + 1), false, 0.0);
        }
        basis.push(direction);
    }
    basis.len()
}

fn extreme(units: &[Unit], geometric: &[usize], mu: &[f32], direction: &[f32], sign: f32) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for &i in geometric {
        let vector = units[i].vector.as_deref().unwrap();
        let projection: f32 = vector
            .iter()
            .zip(mu)
            .zip(direction)
            .map(|((x, mean), q)| (x - mean) * q)
            .sum();
        let score = units[i].quality * (sign * projection).max(0.0);
        if best.is_none_or(|(_, current)| score > current) {
            best = Some((i, score));
        }
    }
    best.and_then(|(i, score)| (score > 0.0).then_some(i))
}

fn add_transitions(units: &[Unit], turns: &[TurnInfo], budget: usize, pool: &mut CandidatePool) -> usize {
    if turns.len() < 2 {
        return 0;
    }
    let mut boundaries = Vec::new();
    for after in 0..turns.len() - 1 {
        let left_start = after.saturating_sub(1);
        let right_end = (after + 3).min(turns.len());
        let left = mean_vectors(
            turns[left_start..=after]
                .iter()
                .filter_map(|turn| turn.vector.as_deref()),
        );
        let right = mean_vectors(
            turns[after + 1..right_end]
                .iter()
                .filter_map(|turn| turn.vector.as_deref()),
        );
        if let (Some(left), Some(right)) = (left, right) {
            boundaries.push(Boundary {
                after_turn: after,
                score: (1.0 - dot(&left, &right)).max(0.0),
            });
        }
    }
    boundaries.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.after_turn.cmp(&b.after_turn))
    });
    let wanted = (budget / 2).clamp(1, 6);
    let mut chosen: Vec<Boundary> = Vec::new();
    for boundary in boundaries {
        if chosen
            .iter()
            .any(|other| other.after_turn.abs_diff(boundary.after_turn) <= 2)
        {
            continue;
        }
        chosen.push(boundary);
        if chosen.len() == wanted {
            break;
        }
    }
    for (rank, boundary) in chosen.iter().enumerate() {
        if let Some(i) = text_before(units, turns, boundary.after_turn) {
            pool.add(i, format!("transition_{}_left", rank + 1), false, boundary.score);
        }
        if let Some(i) = text_after(units, turns, boundary.after_turn + 1) {
            pool.add(i, format!("transition_{}_right", rank + 1), false, boundary.score);
        }
    }
    chosen.len()
}

fn mean_vectors<'a>(vectors: impl Iterator<Item = &'a [f32]>) -> Option<Vec<f32>> {
    weighted_mean(vectors.map(|vector| (1.0, vector)))
}

fn text_before(units: &[Unit], turns: &[TurnInfo], turn: usize) -> Option<usize> {
    turns[..=turn]
        .iter()
        .rev()
        .flat_map(|t| t.units.iter().rev())
        .copied()
        .find(|&i| units[i].block_type == "text")
}

fn text_after(units: &[Unit], turns: &[TurnInfo], turn: usize) -> Option<usize> {
    turns[turn..]
        .iter()
        .flat_map(|t| &t.units)
        .copied()
        .find(|&i| units[i].block_type == "text")
}

fn select_final(
    units: &[Unit],
    turns: &[TurnInfo],
    pool: &CandidatePool,
    budget: usize,
    char_budget: usize,
) -> Vec<usize> {
    let mut selected: Vec<usize> = pool
        .order
        .iter()
        .copied()
        .filter(|i| pool.by_unit[i].mandatory)
        .collect();
    selected.dedup();
    let mut selected_set: HashSet<usize> = selected.iter().copied().collect();
    let mut context: HashSet<usize> = context_for_selected(units, turns, &selected).into_iter().collect();
    let mut chars = context_chars(units, context.iter().copied());
    let mut covered = vec![0.0f32; units.len()];
    for &i in &selected {
        update_coverage(units, i, &mut covered);
    }
    let total_mass: f32 = units.iter().map(|unit| unit.mass).sum();
    let average_mass = total_mass / budget as f32;
    // `budget` is a ceiling, not a target. A weak candidate must explain at least a quarter of an
    // average selection slot; a strong chronological pivot can satisfy this via its bonus.
    let minimum_gain = 0.25 * average_mass;
    let max_transition = pool
        .by_unit
        .values()
        .map(|candidate| candidate.transition)
        .fold(0.0f32, f32::max);

    while selected.len() < budget {
        let mut best: Option<(usize, f32, f32, Vec<usize>, usize)> = None;
        for &candidate_idx in &pool.order {
            if selected_set.contains(&candidate_idx) {
                continue;
            }
            let envelope = envelope(units, turns, candidate_idx);
            let marginal: Vec<usize> = envelope.into_iter().filter(|i| !context.contains(i)).collect();
            let marginal_chars = context_chars(units, marginal.iter().copied());
            if chars.saturating_add(marginal_chars) > char_budget {
                continue;
            }
            let Some(vector) = units[candidate_idx].vector.as_deref() else {
                continue;
            };
            let mut gain = 0.0f32;
            for (i, unit) in units.iter().enumerate() {
                let Some(other) = unit.vector.as_deref() else {
                    continue;
                };
                let similarity = dot(vector, other).max(0.0);
                gain += unit.mass * (similarity - covered[i]).max(0.0);
            }
            let transition = if max_transition > 0.0 {
                pool.by_unit[&candidate_idx].transition / max_transition
            } else {
                0.0
            };
            let transition_bonus = 0.5 * average_mass * transition;
            let substantive_gain = gain + transition_bonus;
            let cost = (1.0 + marginal_chars as f32 / 4000.0).sqrt();
            let score = substantive_gain / cost;
            if best.as_ref().is_none_or(|(_, current, _, _, _)| score > *current) {
                best = Some((candidate_idx, score, substantive_gain, marginal, marginal_chars));
            }
        }
        let Some((chosen, score, substantive_gain, marginal, marginal_chars)) = best else {
            break;
        };
        if !score.is_finite() || substantive_gain < minimum_gain {
            break;
        }
        selected.push(chosen);
        selected_set.insert(chosen);
        context.extend(marginal);
        chars += marginal_chars;
        update_coverage(units, chosen, &mut covered);
    }
    selected
}

fn update_coverage(units: &[Unit], selected: usize, covered: &mut [f32]) {
    let Some(vector) = units[selected].vector.as_deref() else {
        return;
    };
    for (i, unit) in units.iter().enumerate() {
        if let Some(other) = unit.vector.as_deref() {
            covered[i] = covered[i].max(dot(vector, other).max(0.0));
        }
    }
}

fn context_for_selected(units: &[Unit], turns: &[TurnInfo], selected: &[usize]) -> Vec<usize> {
    let mut context = BTreeSet::new();
    for &i in selected {
        context.extend(envelope(units, turns, i));
    }
    let mut out: Vec<usize> = context.into_iter().collect();
    out.sort_by_key(|&i| unit_key(&units[i]));
    out
}

fn envelope(units: &[Unit], turns: &[TurnInfo], selected: usize) -> Vec<usize> {
    let Some(turn_idx) = turns.iter().position(|turn| turn.units.contains(&selected)) else {
        return vec![selected];
    };
    let mut included = BTreeSet::new();
    included.extend(turns[turn_idx].units.iter().copied());
    if let Some(previous) = turns[..turn_idx].iter().rposition(|turn| {
        turn.units
            .iter()
            .any(|&i| units[i].role == "user" && units[i].block_type == "text")
    }) {
        included.extend(turns[previous].units.iter().copied());
    }
    if let Some(relative) = turns[turn_idx + 1..].iter().position(|turn| {
        turn.units
            .iter()
            .any(|&i| units[i].role == "assistant" && units[i].block_type == "text")
    }) {
        included.extend(turns[turn_idx + 1 + relative].units.iter().copied());
    }
    included.into_iter().collect()
}

fn context_chars(units: &[Unit], indices: impl Iterator<Item = usize>) -> usize {
    indices
        .map(|i| {
            let (text, _) = display_text(&units[i]);
            text.chars().count() + 96
        })
        .sum()
}

fn display_text(unit: &Unit) -> (String, bool) {
    if unit.block_type != "tool_result" || unit.text.chars().count() <= TOOL_RESULT_PREVIEW {
        return (unit.text.clone(), false);
    }
    let head: String = unit.text.chars().take(1_350).collect();
    let mut tail: Vec<char> = unit.text.chars().rev().take(550).collect();
    tail.reverse();
    let tail: String = tail.into_iter().collect();
    (
        format!("{head}\n\n[… tool result shortened by session-sketch …]\n\n{tail}"),
        true,
    )
}

/// Render the review-oriented sketch view. It keeps exact turn provenance and selection/context
/// distinctions, while replacing selector-internal reason names with labels useful to a curator.
pub fn render_preview(sketch: &SessionSketch) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "session sketch · {} key blocks · {} with context · {} chars",
        sketch.selected_units.len(),
        sketch.evidence.len(),
        sketch.diagnostics.rendered_characters
    );
    let _ = writeln!(out, "* key evidence    ~ context");

    if let Some(note) = &sketch.diagnostics.fallback {
        let _ = writeln!(out, "note: {note}");
    }

    for evidence in &sketch.evidence {
        let marker = if evidence.selected { '*' } else { '~' };
        let kind = match &evidence.tool_name {
            Some(tool) => format!("{} ({tool})", evidence.block_type),
            None => evidence.block_type.clone(),
        };
        let labels = if evidence.selected {
            preview_reason_labels(&evidence.reasons)
        } else {
            Vec::new()
        };
        let labels = if labels.is_empty() {
            String::new()
        } else {
            format!(" · {}", labels.join(", "))
        };
        let shortened = if evidence.truncated { " · shortened" } else { "" };
        let _ = writeln!(
            out,
            "\n{marker} {} {kind} · seq {}{labels}{shortened}",
            evidence.role, evidence.seq
        );
        let _ = writeln!(out, "  turn {}", evidence.turn_uuid);
        let _ = writeln!(out, "{}", evidence.text);
    }
    out
}

fn preview_reason_labels(reasons: &[String]) -> Vec<&'static str> {
    let mut labels = BTreeSet::new();
    for reason in reasons {
        let label = match reason.as_str() {
            "opening_user" => "opening",
            "closing_assistant" => "outcome",
            "centroid_medoid" => "central",
            "all_evidence" => "complete session",
            reason if reason.starts_with("axis_") => "semantic extreme",
            reason if reason.starts_with("transition_") => "topic shift",
            _ => "selected",
        };
        labels.insert(label);
    }
    labels.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_row(seq: i64, text: &str) -> RawRow {
        RawRow {
            session_id: "session".into(),
            id: format!("id-{seq}"),
            text: text.into(),
            turn_uuid: format!("turn-{seq}"),
            seq,
            ts: "2026-07-22T00:00:00Z".into(),
            role: "user".into(),
            block_type: "text".into(),
            tool_name: String::new(),
            block_idx: 0,
            split_idx: 0,
            vector: Some(vec![1.0, 0.0]),
        }
    }

    fn cached_sketch_fixture() -> SessionSketch {
        SessionSketch {
            schema_version: SKETCH_SCHEMA_VERSION,
            selector_version: SELECTOR_VERSION.into(),
            memory: "local-memory".into(),
            session_id: "session".into(),
            source_fingerprint: "source".into(),
            embedding_fingerprint: "embedding".into(),
            source_chunks: 1,
            eligible_units: 1,
            selected_units: Vec::new(),
            evidence: Vec::new(),
            diagnostics: Diagnostics {
                axes: 0,
                transitions: 0,
                near_duplicate_groups: 0,
                duplicate_strategy: "exact".into(),
                duplicate_vector_comparisons: 0,
                candidates: 0,
                rendered_characters: 0,
                budget: DEFAULT_BUDGET,
                char_budget: DEFAULT_CHAR_BUDGET,
                elapsed_ms: 1,
                fallback: None,
            },
        }
    }

    fn unit(seq: i64, role: &str, vector: &[f32], text: &str) -> Unit {
        Unit {
            id: format!("id-{seq}-{role}"),
            text: text.to_string(),
            turn_uuid: format!("turn-{seq}"),
            seq,
            ts: String::new(),
            role: role.to_string(),
            block_type: "text".to_string(),
            tool_name: String::new(),
            block_idx: 0,
            vector: normalize(vector.to_vec()),
            quality: 1.0,
            mass: 1.0,
        }
    }

    #[test]
    fn overlap_is_reassembled_once() {
        assert_eq!(overlap_len("abcdef", "defghi"), 3);
        assert_eq!(stitch("abcdef", "defghi"), "abcdefghi");
        assert_eq!(stitch("abc", "xyz"), "abcxyz");
    }

    #[test]
    fn source_fingerprint_invalidates_growth_and_same_count_rewrites() {
        let original = vec![raw_row(1, "original")];
        let original_fingerprint = fingerprint(&original);

        let mut grown = original.clone();
        grown.push(raw_row(2, "new turn"));
        assert_ne!(fingerprint(&grown), original_fingerprint);

        let mut rewritten = original;
        rewritten[0].text = "rewritten".into();
        assert_ne!(fingerprint(&rewritten), original_fingerprint);
    }

    #[test]
    fn sketch_cache_round_trips_and_rejects_stale_keys() {
        let temp = tempfile::tempdir().unwrap();
        let path = cache_path(temp.path(), "local-memory", "session");
        let sketch = cached_sketch_fixture();
        let options = SketchOptions::default();
        store_cached_sketch(&path, &sketch).unwrap();

        assert!(load_cached_sketch(&path, "local-memory", "session", "source", "embedding", options,).is_some());
        assert!(load_cached_sketch(
            &path,
            "local-memory",
            "session",
            "rewritten-source",
            "embedding",
            options,
        )
        .is_none());
        assert!(load_cached_sketch(
            &path,
            "local-memory",
            "session",
            "source",
            "embedding",
            SketchOptions {
                budget: options.budget + 1,
                ..options
            },
        )
        .is_none());
    }

    #[test]
    fn tool_result_preview_is_unicode_safe() {
        let mut u = unit(1, "user", &[1.0, 0.0], &"🦀".repeat(2_100));
        u.block_type = "tool_result".to_string();
        let (text, truncated) = display_text(&u);
        assert!(truncated);
        assert!(text.contains("tool result shortened"));
        assert!(text.is_char_boundary(text.len()));
    }

    #[test]
    fn picker_preview_distinguishes_key_evidence_from_context() {
        let sketch = SessionSketch {
            schema_version: 1,
            selector_version: "test".into(),
            memory: "local".into(),
            session_id: "session".into(),
            source_fingerprint: "source".into(),
            embedding_fingerprint: "embedding".into(),
            source_chunks: 2,
            eligible_units: 2,
            selected_units: vec![SelectedUnit {
                id: "selected".into(),
                turn_uuid: "turn-selected".into(),
                seq: 1,
                block_idx: 0,
                reasons: vec!["opening_user".into()],
            }],
            evidence: vec![
                Evidence {
                    id: "selected".into(),
                    turn_uuid: "turn-selected".into(),
                    seq: 1,
                    block_idx: 0,
                    ts: String::new(),
                    role: "user".into(),
                    block_type: "text".into(),
                    tool_name: None,
                    selected: true,
                    reasons: vec!["opening_user".into(), "axis_1_positive".into()],
                    truncated: false,
                    text: "Build the sketch view".into(),
                },
                Evidence {
                    id: "context".into(),
                    turn_uuid: "turn-context".into(),
                    seq: 2,
                    block_idx: 0,
                    ts: String::new(),
                    role: "assistant".into(),
                    block_type: "text".into(),
                    tool_name: None,
                    selected: false,
                    reasons: Vec::new(),
                    truncated: false,
                    text: "Neighboring context".into(),
                },
            ],
            diagnostics: Diagnostics {
                axes: 1,
                transitions: 0,
                near_duplicate_groups: 0,
                duplicate_strategy: "exact".into(),
                duplicate_vector_comparisons: 1,
                candidates: 1,
                rendered_characters: 42,
                budget: 8,
                char_budget: 16_000,
                elapsed_ms: 1,
                fallback: None,
            },
        };

        let preview = render_preview(&sketch);
        assert!(preview.contains("* user text · seq 1 · opening, semantic extreme"));
        assert!(preview.contains("~ assistant text · seq 2"));
        assert!(preview.contains("turn turn-selected"));
        assert!(!preview.contains("axis_1_positive"));
    }

    #[test]
    fn axes_include_both_ends_of_a_contrast() {
        let units = vec![
            unit(0, "user", &[1.0, 0.0], "east"),
            unit(1, "assistant", &[-1.0, 0.0], "west"),
            unit(2, "assistant", &[0.0, 1.0], "north"),
            unit(3, "assistant", &[0.0, -1.0], "south"),
        ];
        let mut pool = CandidatePool::new();
        let axes = discover_axes(&units, 7, 3, &mut pool);
        assert_eq!(axes, 2);
        assert!(pool.order.len() >= 4, "pool={:?}", pool.order);
    }

    #[test]
    fn transition_finds_the_topic_change() {
        let units = vec![
            unit(0, "user", &[1.0, 0.0], "a0"),
            unit(1, "assistant", &[1.0, 0.0], "a1"),
            unit(2, "user", &[-1.0, 0.0], "b0"),
            unit(3, "assistant", &[-1.0, 0.0], "b1"),
        ];
        let turns = build_turns(&units);
        let mut pool = CandidatePool::new();
        let count = add_transitions(&units, &turns, 6, &mut pool);
        assert_eq!(count, 1);
        assert!(pool.by_unit.values().any(|c| c.transition > 1.5));
    }

    #[test]
    fn coverage_adds_the_unrepresented_cluster() {
        let units = vec![
            unit(0, "user", &[1.0, 0.0], "cluster a opening"),
            unit(1, "assistant", &[0.99, 0.01], "cluster a repeat"),
            unit(2, "assistant", &[0.0, 1.0], "cluster b"),
        ];
        let turns = build_turns(&units);
        let mut pool = CandidatePool::new();
        pool.add(0, "opening_user", true, 0.0);
        pool.add(1, "candidate_a", false, 0.0);
        pool.add(2, "candidate_b", false, 0.0);
        let selected = select_final(&units, &turns, &pool, 3, 20_000);
        assert!(selected.contains(&2), "selected={selected:?}");
    }

    #[test]
    fn duplicate_groups_share_mass() {
        let mut units = vec![
            unit(0, "user", &[1.0, 0.0], "same"),
            unit(1, "assistant", &[1.0, 0.0], "same"),
            unit(2, "assistant", &[0.0, 1.0], "different"),
        ];
        let stats = assign_duplicate_mass(&mut units);
        assert_eq!(stats.groups, 1);
        assert_eq!(stats.strategy, "exact-all-pairs");
        assert!((units[0].mass - 0.5).abs() < 1e-6);
        assert!((units[1].mass - 0.5).abs() < 1e-6);
        assert!((units[2].mass - 1.0).abs() < 1e-6);
    }

    #[test]
    fn exact_text_duplicates_without_vectors_share_mass() {
        let mut units = vec![
            unit(0, "user", &[1.0, 0.0], "same unembedded text"),
            unit(1, "assistant", &[0.0, 1.0], "same unembedded text"),
        ];
        units[0].vector = None;
        units[1].vector = None;
        let stats = assign_duplicate_mass(&mut units);
        assert_eq!(stats.groups, 1);
        assert_eq!(stats.vector_comparisons, 0);
        assert!((units[0].mass - 0.5).abs() < 1e-6);
        assert!((units[1].mass - 0.5).abs() < 1e-6);
    }

    #[test]
    fn bounded_duplicate_search_avoids_quadratic_comparisons() {
        let mut units: Vec<Unit> = (0..400)
            .map(|seq| unit(seq, "assistant", &[1.0, 0.0], &format!("repeat {seq}")))
            .collect();
        let stats = assign_duplicate_mass_with_limit(&mut units, 0);
        assert_eq!(stats.strategy, "simhash-8x10-bounded");
        assert_eq!(stats.groups, 1);
        assert!(
            stats.vector_comparisons < units.len() * 64,
            "comparisons={}",
            stats.vector_comparisons
        );
        for unit in units {
            assert!((unit.mass - 1.0 / 400.0).abs() < 1e-6);
        }
    }

    #[test]
    fn bounded_duplicate_search_finds_a_distant_near_neighbor() {
        let dimension = 128;
        let mut first = vec![0.0; dimension];
        first[0] = 1.0;
        let mut units = vec![unit(0, "assistant", &first, "first")];
        for component in 2..82 {
            let mut filler = vec![0.0; dimension];
            filler[component] = 1.0;
            units.push(unit(
                component as i64,
                "assistant",
                &filler,
                &format!("filler {component}"),
            ));
        }
        let mut near = vec![0.0; dimension];
        near[0] = 0.98;
        near[1] = (1.0f32 - 0.98f32.powi(2)).sqrt();
        units.push(unit(100, "assistant", &near, "near first"));

        let stats = assign_duplicate_mass_with_limit(&mut units, 0);
        assert_eq!(stats.groups, 1);
        assert!((units[0].mass - 0.5).abs() < 1e-6);
        assert!((units.last().unwrap().mass - 0.5).abs() < 1e-6);
    }

    #[test]
    fn centroid_anchor_prefers_text_over_tool_output() {
        let text = unit(0, "assistant", &[1.0, 0.0], "explanation");
        let mut tool = unit(1, "user", &[1.0, 0.0], "large output");
        tool.block_type = "tool_result".to_string();
        tool.quality = 0.2;
        tool.mass = 0.2;
        let mut pool = CandidatePool::new();
        add_anchors(&[text, tool], &mut pool);
        let medoid = pool
            .by_unit
            .iter()
            .find(|(_, candidate)| candidate.reasons.contains("centroid_medoid"))
            .map(|(&i, _)| i);
        assert_eq!(medoid, Some(0));
    }

    #[test]
    fn harness_interruption_is_noise() {
        assert!(is_harness_noise("[Request interrupted by user]"));
        assert!(!is_harness_noise("the user interrupted the request intentionally"));
    }
}
