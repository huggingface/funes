//! Project memories, and the session curation that feeds them. A memory is **named** the project
//! memory of a `project` — a repo identity like `huggingface/funes`, or a bare label — carried
//! in the dataset's own schema metadata beside the `embedding_model` pin. (The per-chunk
//! `workdir` column — the munged session working directory — is provenance, not policy.)
//! Pushing to a project memory ships only the sessions this host has marked `include`; a
//! session with no decision yet stays local until it's reviewed, so neither a new session nor
//! an early push can leak one. Any other memory is your **personal memory**, hosted: it
//! receives everything, as ever.
//!
//! Decisions are per-host facts (each host can only ever publish its own sessions), recorded
//! in `<funes-home>/curation/<memory>`, one per line, `#` for comments:
//!
//! ```text
//! include adc42fa9-8b40-4f84-9a4e-035d2188c15f   # add-memory-hooks review
//! exclude d93c7cc5-c165-4198-af8e-bd5cdc397120   # internal pitch — stays private
//! ```
//!
//! Human- and agent-editable. A decision flipped to `exclude` later does not retract what
//! already shipped — the remote is append-only; curation prevents, it does not undo.

use crate::hub::{self, Memory};
use crate::{dataset, hf_dataset, index, jsonl};
use anyhow::{bail, Context, Result};
use arrow_array::{Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::Schema;
use hf_hub::repository::CommitOperation;
use lance::dataset::WriteParams;
use lance::Dataset;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The project this memory is *of* — the `project` schema-metadata key, carried beside the
/// `embedding_model` pin: a repo identity (`huggingface/funes`) or a bare label. Its presence
/// marks a *project* memory; without it, a *personal* one — which receives everything, as ever.
/// Distinct from the per-chunk `workdir` column (provenance, not policy).
pub fn project(ds: &Dataset) -> Option<String> {
    ds.schema().metadata.get("project").cloned()
}

/// The funes chunk schema with the project stamped beside the embedding-model pin.
fn project_schema(project: &str) -> Arc<Schema> {
    let base = index::schema();
    let mut metadata = base.metadata().clone();
    metadata.insert("project".to_string(), project.to_string());
    Arc::new(Schema::new_with_metadata(base.fields().clone(), metadata))
}

/// Outcome of [`name_project`].
pub enum Named {
    /// The dataset didn't exist: created empty (schema + embedder pin + project, zero rows).
    /// Every later push appends into it.
    Created,
    /// An existing personal memory: the project was stamped onto it; its history stays.
    Promoted,
    /// Already the memory of this same project — nothing to do.
    Unchanged,
}

/// Name `target` the project memory of `project`. The repo itself must already exist — funes
/// creates repos only on explicit interactive consent, which is the CLI's job. Refuses a memory
/// already named for a different project (widening is a future, explicit affordance), and never
/// mistakes an unreadable dataset for an absent one.
pub async fn name_project(target: &Memory, project: &str) -> Result<Named> {
    let uri = match target {
        Memory::Remote { uri } => uri.clone(),
        Memory::Local { .. } => {
            bail!("a project memory must be remote — pass `<org>/<repo>` or an `hf://…` URI")
        }
    };
    match hub::remote_reachability(&uri).await {
        hub::Reachability::Offline => bail!("{uri} is unreachable — can't curate it while offline"),
        hub::Reachability::Missing => return Err(hub::missing_remote(&uri)),
        hub::Reachability::Ok => {}
    }

    match target.open().await {
        Ok(ds) => match self::project(&ds) {
            Some(existing) if existing == project => Ok(Named::Unchanged),
            Some(existing) => bail!(
                "{} is already the project memory of {existing} — refusing to rename it to {project}",
                target.label()
            ),
            None => {
                promote(&uri, project).await?;
                Ok(Named::Promoted)
            }
        },
        Err(e) if hub::dataset_absent(&e) => {
            create_memory(&uri, project).await?;
            Ok(Named::Created)
        }
        Err(e) => Err(e.context(format!(
            "can't read {} to curate it — not treating an unreadable memory as absent",
            target.label()
        ))),
    }
}

/// Create the project memory empty in one commit: it exists as a Hub artifact from the moment
/// it's named, and every later push is an append.
async fn create_memory(uri: &str, project: &str) -> Result<()> {
    let (owner, name, prefix) = hub::parse_hf(uri)?;
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to curate")?;
    let repo = hub::client(Some(&token), true)?.dataset(owner, name);

    let staging = tempfile::tempdir()?;
    let db_dir = if prefix.is_empty() {
        staging.path().to_path_buf()
    } else {
        staging.path().join(&prefix)
    };
    std::fs::create_dir_all(&db_dir)?;
    let schema = project_schema(project);
    let reader = RecordBatchIterator::new(Vec::<Result<RecordBatch, arrow_schema::ArrowError>>::new(), schema);
    Dataset::write(
        reader,
        &dataset::table_uri(&db_dir.to_string_lossy()),
        Some(WriteParams::default()),
    )
    .await
    .context("creating the project memory")?;

    let mut ops = Vec::new();
    for entry in walkdir::WalkDir::new(&db_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(staging.path()).unwrap_or(entry.path());
        ops.push(CommitOperation::add_file(
            rel.to_string_lossy().into_owned(),
            entry.path().to_path_buf(),
        ));
    }
    repo.create_commit()
        .operations(ops)
        .commit_message(format!("funes curate: project memory of {project}"))
        .revision("main".to_string())
        .send()
        .await
        .map_err(|e| anyhow::Error::new(e).context("committing the project memory"))?;
    Ok(())
}

/// Stamp the project onto an existing personal memory in one guarded commit.
async fn promote(uri: &str, project: &str) -> Result<()> {
    let (owner, name, _) = hub::parse_hf(uri)?;
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to curate")?;
    let repo = hub::client(Some(&token), true)?.dataset(owner, name);
    let rev = "main".to_string();
    let dataset_uri = format!("{uri}/{}.lance", dataset::TABLE);
    let opts = HashMap::from([("hf_token".to_string(), token), ("revision".to_string(), rev.clone())]);
    let updates = HashMap::from([("project".to_string(), project.to_string())]);
    hf_dataset::amend_schema_metadata(
        &repo,
        &dataset_uri,
        opts,
        &rev,
        format!("funes curate: project memory of {project}"),
        updates,
    )
    .await
}

/// A memory's curation decisions.
#[derive(Debug, Default)]
pub struct Curation {
    pub include: HashSet<String>,
    pub exclude: HashSet<String>,
    /// The local chunk count an `include` was last reviewed at, for the sessions whose line carries
    /// one. A session absent here — a legacy `include` with no count — has no watermark and never
    /// re-flags; one present re-flags once the session grows past it (see [`Curation::is_stale`]).
    reviewed: HashMap<String, usize>,
}

impl Curation {
    /// Whether `session` has a decision, either way.
    pub fn decided(&self, session: &str) -> bool {
        self.include.contains(session) || self.exclude.contains(session)
    }

    /// Whether an included session has grown past the chunk count it was reviewed at: the decision
    /// no longer covers the session's current content, so its new chunks must not ship until it's
    /// reviewed again — a review dismissed by new commits landing on the PR.
    pub fn is_stale(&self, session: &str, current_chunks: usize) -> bool {
        self.reviewed.get(session).is_some_and(|&n| current_chunks > n)
    }
}

/// The curation file for `memory_uri`: `<funes-home>/curation/<sanitized-uri>`.
pub fn file_for(memory_uri: &str) -> PathBuf {
    dataset::funes_dir().join("curation").join(sanitize(memory_uri))
}

/// A memory URI as a filename — every path-hostile byte becomes `_`, deterministically, so the
/// canonical `hf://…` URI always maps to the same file.
fn sanitize(uri: &str) -> String {
    uri.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The memory's curation, or None when it has no file (nothing decided — push holds everything).
pub fn load(memory_uri: &str) -> Result<Option<Curation>> {
    let path = file_for(memory_uri);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(Some(parse(&text)))
}

/// Parse decision lines. Blank, comment-only, and unrecognizable lines are skipped (with a note
/// for the latter) — skipping is fail-safe both ways: an unparsed decision leaves its session
/// pending, which is held back, never shipped.
fn parse(text: &str) -> Curation {
    let mut curation = Curation::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("include"), Some(id)) => {
                curation.include.insert(id.to_string());
                // A trailing integer is the reviewed-at chunk count (the growth watermark); its
                // absence (a legacy line) leaves the session without one.
                if let Some(n) = words.next().and_then(|w| w.parse::<usize>().ok()) {
                    curation.reviewed.insert(id.to_string(), n);
                }
            }
            (Some("exclude"), Some(id)) => {
                curation.exclude.insert(id.to_string());
            }
            _ => eprintln!("note: skipped an unrecognized curation line: {line}"),
        }
    }
    curation
}

/// Split the local candidates (session → chunk ids) by decision: the ids allowed to ship, and
/// the *pending* sessions — no decision, and at least one chunk not on the remote yet — that a
/// push holds back and reports. An `exclude`d session is dropped silently: that state is
/// deliberate, not actionable.
pub fn partition(
    by_session: &HashMap<String, Vec<String>>,
    curation: &Curation,
    remote_ids: &HashSet<String>,
) -> (HashSet<String>, Vec<String>) {
    let mut shipped = HashSet::new();
    let mut pending = Vec::new();
    for (session, ids) in by_session {
        if curation.include.contains(session) {
            // An included session that grew past its reviewed count is stale — its new chunks are
            // held for a fresh review, like a PR review dismissed by new commits.
            if curation.is_stale(session, ids.len()) {
                pending.push(session.clone());
            } else {
                shipped.extend(ids.iter().cloned());
            }
        } else if !curation.decided(session) && ids.iter().any(|id| !remote_ids.contains(id)) {
            pending.push(session.clone());
        }
    }
    pending.sort();
    (shipped, pending)
}

/// The local rows as session → chunk ids — the shape [`partition`] splits by decision.
pub(crate) async fn ids_by_session(ds: &Dataset) -> Result<HashMap<String, Vec<String>>> {
    let batches = dataset::scan_rows(ds, &["id", "session_id"], None, None).await?;
    let mut by_session: HashMap<String, Vec<String>> = HashMap::new();
    for batch in batches {
        let ids = batch
            .column_by_name("id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let sessions = batch
            .column_by_name("session_id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let (Some(ids), Some(sessions)) = (ids, sessions) else {
            continue;
        };
        for i in 0..batch.num_rows() {
            by_session
                .entry(sessions.value(i).to_string())
                .or_default()
                .push(ids.value(i).to_string());
        }
    }
    Ok(by_session)
}

const APPROVAL_HISTORY: u8 = 1;
const APPROVAL_START: u8 = 2;
const APPROVAL_END: u8 = 4;

/// Structural signals left in already-indexed Codex guardian transcripts. The exact header plus
/// both delimiters is deliberately stricter than matching `APPROVAL REQUEST END` alone: a primary
/// session may discuss that phrase while diagnosing this very issue.
fn approval_review_signal(text: &str) -> u8 {
    let text = text.trim();
    let mut signal = 0;
    if text.starts_with("The following is the Codex agent history whose request action you are assessing.")
        || text.starts_with("The following is the Codex agent history added since your last approval assessment.")
    {
        signal |= APPROVAL_HISTORY;
    }
    if text == ">>> APPROVAL REQUEST START" {
        signal |= APPROVAL_START;
    }
    if text == ">>> APPROVAL REQUEST END" {
        signal |= APPROVAL_END;
    }
    signal
}

fn is_approval_review(signals: u8) -> bool {
    signals & (APPROVAL_HISTORY | APPROVAL_START | APPROVAL_END) == APPROVAL_HISTORY | APPROVAL_START | APPROVAL_END
}

/// The local sessions curation may offer — local rows minus child-agent sessions. Claude exposes
/// those through its `agent-<id>` filename; Codex records the fact only in the source JSONL's
/// `session_meta`, so inspect each distinct Codex source path as well. The approval-wrapper
/// fingerprint covers guardian rows already indexed on a host where the original JSONL is gone.
/// Project memories and the review both draw from this; a personal memory's `all_ids` push is
/// unaffected (it keeps everything).
pub(crate) async fn candidate_sessions(local: &Dataset) -> Result<HashMap<String, Vec<String>>> {
    let cols = [
        "id",
        "session_id",
        "source_path",
        "harness",
        "role",
        "block_type",
        "text",
    ];
    let batches = dataset::scan_rows(local, &cols, None, None).await?;
    let mut by_session: HashMap<String, Vec<String>> = HashMap::new();
    let mut subagents = HashSet::new();
    let mut approval_signals: HashMap<String, u8> = HashMap::new();
    let mut codex_source_kind: HashMap<String, bool> = HashMap::new();
    for batch in batches {
        let col = |name: &str| {
            batch
                .column_by_name(name)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        };
        let (Some(ids), Some(sessions)) = (col("id"), col("session_id")) else {
            continue;
        };
        let (source_paths, harnesses, roles, block_types, texts) = (
            col("source_path"),
            col("harness"),
            col("role"),
            col("block_type"),
            col("text"),
        );
        for i in 0..batch.num_rows() {
            let session = sessions.value(i);
            by_session
                .entry(session.to_string())
                .or_default()
                .push(ids.value(i).to_string());
            if jsonl::is_subagent(session) {
                subagents.insert(session.to_string());
                continue;
            }

            if harnesses.is_some_and(|h| h.value(i) == "codex") {
                let source_path = source_paths.map(|paths| paths.value(i)).unwrap_or("");
                if !source_path.is_empty()
                    && *codex_source_kind
                        .entry(source_path.to_string())
                        .or_insert_with(|| crate::codex_traces::is_subagent_file(Path::new(source_path)))
                {
                    subagents.insert(session.to_string());
                }
            }

            if roles.is_some_and(|role| role.value(i) == "user")
                && block_types.is_some_and(|kind| kind.value(i) == "text")
            {
                if let Some(texts) = texts {
                    *approval_signals.entry(session.to_string()).or_default() |= approval_review_signal(texts.value(i));
                }
            }
        }
    }
    by_session.retain(|session, _| {
        !subagents.contains(session) && !approval_signals.get(session).copied().is_some_and(is_approval_review)
    });
    Ok(by_session)
}

/// Whether a session's stored `repo` (space-joined `owner/name` idents) names `project` — the one
/// rule for "this session belongs to the project's memory", shared by [`discover`] and
/// [`project_sessions`] so the review and push never disagree on the scope.
pub(crate) fn repo_names_project(repo: &str, project: &str) -> bool {
    repo.split_whitespace().any(|i| i == project)
}

/// The session ids whose stored `repo` names `project` — the review's matched set. A project
/// memory's pending-review report is scoped to these, so sessions that belong to another repo (or
/// have no resolvable checkout) aren't miscounted as this memory's to-do.
pub(crate) async fn project_sessions(local: &Dataset, project: &str) -> Result<HashSet<String>> {
    let batches = dataset::scan_rows(local, &["session_id", "repo"], None, None).await?;
    let mut matched = HashSet::new();
    for batch in batches {
        let sessions = batch
            .column_by_name("session_id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let repos = batch
            .column_by_name("repo")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let (Some(sessions), Some(repos)) = (sessions, repos) else {
            continue;
        };
        for i in 0..batch.num_rows() {
            if repo_names_project(repos.value(i), project) {
                matched.insert(sessions.value(i).to_string());
            }
        }
    }
    Ok(matched)
}

/// A curation decision a review records for a session.
#[derive(Clone, Copy)]
pub enum Decision {
    Include,
    Exclude,
}

/// One curation line. An `include` carries the reviewed-at chunk count (`chunks`) as its growth
/// watermark; `exclude` needs none (it never ships, so growth is moot). The comment, when present,
/// follows a `#`.
fn decision_line(decision: Decision, session: &str, chunks: usize, comment: &str) -> String {
    let head = match decision {
        Decision::Include => format!("include {session} {chunks}"),
        Decision::Exclude => format!("exclude {session}"),
    };
    match comment.trim() {
        "" => format!("{head}\n"),
        comment => format!("{head}   # {comment}\n"),
    }
}

/// Rewrite the memory's curation file so `session` carries exactly `decision` (None = pending):
/// every existing line naming `session` is dropped, then the new decision line (with `comment`, and
/// `chunks` as the reviewed-at watermark for an `include`) is appended if any. Other sessions'
/// lines — comments and all — are preserved verbatim. Rewriting rather than appending lets a
/// decision be cleared, flipped, or (for an `include`) refreshed to a new watermark.
pub fn set_decision(
    memory_uri: &str,
    session: &str,
    decision: Option<Decision>,
    chunks: usize,
    comment: &str,
) -> Result<()> {
    let path = file_for(memory_uri);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut out = String::new();
    for line in text.lines() {
        let body = line.split('#').next().unwrap_or("").trim();
        let mut words = body.split_whitespace();
        let names_session =
            matches!((words.next(), words.next()), (Some("include") | Some("exclude"), Some(id)) if id == session);
        if !names_session {
            let _ = writeln!(out, "{line}");
        }
    }
    if let Some(d) = decision {
        out.push_str(&decision_line(d, session, chunks, comment));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Whether a user text block's start is injected scaffolding rather than the human's words. Harness
/// wrappers are XML-ish tags (`<ide_opened_file>`, `<command-name>`, `<environment_context>`,
/// `<system-reminder>`, …); codex/pi agent-notes open with a markdown heading (`# AGENTS.md
/// instructions…`); a Claude skill loads with the `Base directory for this skill:` preamble; a
/// compacted session replays its machine-written recap as a user turn opening `This session is being
/// continued…`. All are recognizable from the block's start (a mid-block split can begin anywhere, so
/// test split 0).
pub fn is_scaffolding(block_start: &str) -> bool {
    let t = block_start.trim_start();
    t.starts_with('<')
        || t.starts_with('#')
        || t.starts_with("Base directory for this skill")
        || t.starts_with("This session is being continued from a previous conversation")
}

/// A session as the review lists it, with its opening real prompt (injected scaffolding skipped)
/// for the one-line summary.
pub struct SessionSummary {
    pub session_id: String,
    pub chunks: usize,
    pub last_ts: String,
    pub workdir: String,
    pub first_prompt: String,
    /// The session's source repo(s) as `owner/name`, space-joined; empty when unresolvable.
    pub repo: String,
    /// How much of this local session is already in the memory being curated. Plain [`summaries`]
    /// have no target-memory context and leave this as [`Publication::Local`]; [`candidates`]
    /// fills it in.
    pub publication: Publication,
}

impl SessionSummary {
    /// The `YYYY-MM-DD` of the session's latest activity, for a decision's auto-comment.
    pub fn date(&self) -> &str {
        self.last_ts.get(..10).unwrap_or(&self.last_ts)
    }
}

/// A local session's publication state relative to the project memory being curated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Publication {
    /// None of the local chunks are in the project memory yet.
    #[default]
    Local,
    /// Some chunks are published, but the local session has since grown.
    Partial,
    /// Every local chunk is already in the project memory. Its picker row is browse-only because
    /// the append-only remote cannot honor a later exclusion.
    Published,
}

impl Publication {
    pub fn is_read_only(self) -> bool {
        self == Self::Published
    }
}

fn publication(ids: &[String], remote_ids: &HashSet<String>) -> Publication {
    let published = ids.iter().filter(|id| remote_ids.contains(*id)).count();
    match published {
        0 => Publication::Local,
        n if n == ids.len() => Publication::Published,
        _ => Publication::Partial,
    }
}

/// Per-session summaries from `ds`, newest first. `only`, when given, keeps just those sessions.
/// The prompt shown is the session's earliest user text block that isn't injected scaffolding (see
/// [`is_scaffolding`]) — the opening real prompt. Blank if a session is scaffolding only.
pub async fn summaries(ds: &Dataset, only: Option<&HashSet<String>>) -> Result<Vec<SessionSummary>> {
    let cols = [
        "session_id",
        "workdir",
        "ts",
        "role",
        "block_type",
        "text",
        "seq",
        "repo",
        "block_idx",
        "split_idx",
    ];
    let batches = dataset::scan_rows(ds, &cols, None, None).await?;
    struct Acc {
        chunks: usize,
        last_ts: String,
        workdir: String,
        first_prompt: String,
        /// (seq, block_idx) of the chosen opening block — earliest wins.
        best_key: (i64, i64),
        repo: String,
    }
    let mut map: HashMap<String, Acc> = HashMap::new();
    for batch in batches {
        let col = |n: &str| {
            batch
                .column_by_name(n)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        };
        let icol = |n: &str| {
            batch
                .column_by_name(n)
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
        };
        let (Some(sid), Some(wd), Some(ts), Some(role), Some(bt), Some(text)) = (
            col("session_id"),
            col("workdir"),
            col("ts"),
            col("role"),
            col("block_type"),
            col("text"),
        ) else {
            continue;
        };
        let (seqs, block_idx, split_idx) = (icol("seq"), icol("block_idx"), icol("split_idx"));
        for i in 0..batch.num_rows() {
            let session = sid.value(i).to_string();
            if only.is_some_and(|o| !o.contains(&session)) {
                continue;
            }
            let seq = seqs.map(|s| s.value(i)).unwrap_or(0);
            let acc = map.entry(session).or_insert_with(|| Acc {
                chunks: 0,
                last_ts: String::new(),
                workdir: wd.value(i).to_string(),
                first_prompt: String::new(),
                best_key: (i64::MAX, i64::MAX),
                repo: col("repo").map(|c| c.value(i).to_string()).unwrap_or_default(),
            });
            acc.chunks += 1;
            if ts.value(i) > acc.last_ts.as_str() {
                acc.last_ts = ts.value(i).to_string();
            }
            // Only a block start (split 0) shows a block's real beginning; take the earliest
            // (seq, block_idx) user text block that isn't scaffolding.
            let bi = block_idx.map(|c| c.value(i)).unwrap_or(0);
            let si = split_idx.map(|c| c.value(i)).unwrap_or(0);
            if role.value(i) == "user" && bt.value(i) == "text" && si == 0 && (seq, bi) < acc.best_key {
                let t = text.value(i);
                if !is_scaffolding(t) {
                    acc.best_key = (seq, bi);
                    // Collapse whitespace to a single line: a newline here would spill the review
                    // row and corrupt the one-line decision comment written to the curation file.
                    acc.first_prompt = t
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(200)
                        .collect();
                }
            }
        }
    }
    let mut out: Vec<SessionSummary> = map
        .into_iter()
        .map(|(session_id, a)| SessionSummary {
            session_id,
            chunks: a.chunks,
            last_ts: a.last_ts,
            workdir: a.workdir,
            first_prompt: a.first_prompt,
            repo: a.repo,
            publication: Publication::Local,
        })
        .collect();
    out.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
    Ok(out)
}

/// Every chunk id in `ds`, flattened from the session grouping.
async fn flat_ids(ds: &Dataset) -> Result<HashSet<String>> {
    Ok(ids_by_session(ds).await?.into_values().flatten().collect())
}

/// This machine's count of sessions pending review for the project memory at `memory_uri` (its
/// already-open `remote` dataset). The caller has confirmed the memory is a project memory.
pub async fn pending_count(remote: &Dataset, memory_uri: &str) -> Result<usize> {
    let Ok(local) = Memory::local().open().await else {
        return Ok(0);
    };
    let by_session = candidate_sessions(&local).await?;
    let decisions = load(memory_uri)?.unwrap_or_default();
    let remote_ids = flat_ids(remote).await?;
    Ok(partition(&by_session, &decisions, &remote_ids).1.len())
}

/// Sessions grouped by their stored repo attribution against the project.
pub struct Discovered {
    /// Repo names the project — the review pre-selects these.
    pub matched: Vec<SessionSummary>,
    /// Repo names some *other* repo, so they belong to that repo's memory — collapsed to a count
    /// and left undecided.
    pub other: Vec<SessionSummary>,
    /// No repo stored — checkout gone at index time, non-git, or a non-local source.
    pub unresolvable: Vec<SessionSummary>,
}

/// Group `sessions` by their stored `repo` (space-joined `owner/name` identities) against
/// `project`.
pub fn discover(sessions: Vec<SessionSummary>, project: &str) -> Discovered {
    let mut found = Discovered {
        matched: Vec::new(),
        other: Vec::new(),
        unresolvable: Vec::new(),
    };
    for s in sessions {
        if repo_names_project(&s.repo, project) {
            found.matched.push(s);
        } else if s.repo.split_whitespace().next().is_none() {
            found.unresolvable.push(s);
        } else {
            found.other.push(s);
        }
    }
    found
}

/// What curating `memory` for a project would entail — resolved without touching the memory, so a
/// review can run against local sessions before the memory is created.
pub enum Prepared {
    /// The memory is already the project memory of `project` — ready to review and record.
    Ready { uri: String, project: String },
    /// The memory doesn't exist. Creating it as the project memory of `project` is deferred until
    /// there's something to publish (an interactive review with an include).
    Absent { uri: String, project: String },
    /// The memory is a personal memory. Promoting it to the project memory of `project` is deferred
    /// likewise.
    Personal { uri: String, project: String },
}

impl Prepared {
    pub fn uri(&self) -> &str {
        match self {
            Prepared::Ready { uri, .. } | Prepared::Absent { uri, .. } | Prepared::Personal { uri, .. } => uri,
        }
    }
    pub fn project(&self) -> &str {
        match self {
            Prepared::Ready { project, .. } | Prepared::Absent { project, .. } | Prepared::Personal { project, .. } => {
                project
            }
        }
    }
}

/// Resolve what curating `memory` for `project` entails — reading its state, creating nothing.
/// A given `project` must match the one already recorded, or names the one to assign; omitting it
/// requires `memory` to already be a project memory.
pub async fn prepare(memory: &Memory, project: Option<&str>) -> Result<Prepared> {
    let uri = match memory {
        Memory::Remote { uri } => uri.clone(),
        Memory::Local { .. } => {
            bail!("a project memory must be remote — pass `<org>/<repo>` or an `hf://…` URI")
        }
    };
    match memory.open().await {
        Ok(ds) => match (self::project(&ds), project) {
            (Some(existing), Some(given)) if existing != given => bail!(
                "{} is already the project memory of {existing} — refusing to rename it to {given}",
                memory.label()
            ),
            (Some(existing), _) => Ok(Prepared::Ready { uri, project: existing }),
            (None, Some(given)) => Ok(Prepared::Personal { uri, project: given.to_string() }),
            (None, None) => bail!(
                "{0} isn't a project memory (push sends it everything) — name its project: `funes curate {0} <project>`",
                memory.label()
            ),
        },
        Err(e) if hub::dataset_absent(&e) => match project {
            Some(given) => Ok(Prepared::Absent { uri, project: given.to_string() }),
            None => bail!("{} doesn't exist", memory.label()),
        },
        Err(e) => Err(e.context(format!("can't read {}", memory.label()))),
    }
}

/// Local repo identities whose name (the segment after `/`) is `label` — to suggest the owner when
/// a project is given as a bare name (`transformers` → `huggingface/transformers`). Distinct,
/// sorted; empty when there's no local memory or no match.
pub async fn projects_named(label: &str) -> Result<Vec<String>> {
    let Ok(local) = Memory::local().open().await else {
        return Ok(Vec::new());
    };
    let suffix = format!("/{label}");
    let batches = dataset::scan_rows(&local, &["repo"], None, None).await?;
    let mut found = BTreeSet::new();
    for batch in batches {
        let Some(repo) = batch
            .column_by_name("repo")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        else {
            continue;
        };
        for i in 0..batch.num_rows() {
            for ident in repo.value(i).split_whitespace().filter(|id| id.ends_with(&suffix)) {
                found.insert(ident.to_string());
            }
        }
    }
    Ok(found.into_iter().collect())
}

/// The sessions a review may offer for `project`, grouped by matching each session's stored repo
/// against it (matched / other / unresolvable). `all_reviewable` chooses the set: `true` — every
/// local session, including sessions already fully published, so the interactive picker is also a
/// browser over this host's project history; `false` — only the pending (undecided) ones, the to-do
/// the text listing reports. Empty when there's no local memory or the set is empty.
pub async fn candidates(memory: &Memory, uri: &str, project: &str, all_reviewable: bool) -> Result<Discovered> {
    let empty = || Discovered {
        matched: Vec::new(),
        other: Vec::new(),
        unresolvable: Vec::new(),
    };
    let Ok(local) = Memory::local().open().await else {
        return Ok(empty());
    };
    let by_session = candidate_sessions(&local).await?;
    let remote_ids = match memory.open().await {
        Ok(ds) => flat_ids(&ds).await?,
        Err(_) => HashSet::new(),
    };
    let set: HashSet<String> = if all_reviewable {
        by_session.keys().cloned().collect()
    } else {
        let decisions = load(uri)?.unwrap_or_default();
        partition(&by_session, &decisions, &remote_ids).1.into_iter().collect()
    };
    if set.is_empty() {
        return Ok(empty());
    }
    let mut sums = summaries(&local, Some(&set)).await?;
    for summary in &mut sums {
        if let Some(ids) = by_session.get(&summary.session_id) {
            summary.publication = publication(ids, &remote_ids);
        }
    }
    Ok(discover(sums, project))
}

/// Record `include`/`exclude` decisions for `sessions`, auto-commenting each with its session's
/// date and first real prompt (pulled from the local memory). Returns the `recorded N include,
/// M exclude` report line.
pub async fn record_decisions(uri: &str, project: &str, include: &[String], exclude: &[String]) -> Result<String> {
    let local = Memory::local().open().await?;
    let touched: HashSet<String> = include.iter().chain(exclude).cloned().collect();
    let by_id: HashMap<String, SessionSummary> = summaries(&local, Some(&touched))
        .await?
        .into_iter()
        .map(|s| (s.session_id.clone(), s))
        .collect();
    // Each decision is written through `set_decision`, which rewrites the session's line — so
    // re-including a grown session refreshes its watermark (a plain append would skip it as an
    // existing include and leave the session stale).
    let comment_and_chunks = |id: &str| {
        let s = by_id.get(id);
        let comment = s
            .map(|s| format!("{} {}", s.date(), s.first_prompt).trim().to_string())
            .unwrap_or_default();
        (comment, s.map(|s| s.chunks).unwrap_or(0))
    };
    for id in include {
        let (comment, chunks) = comment_and_chunks(id);
        set_decision(uri, id, Some(Decision::Include), chunks, &comment)?;
    }
    for id in exclude {
        let (comment, chunks) = comment_and_chunks(id);
        set_decision(uri, id, Some(Decision::Exclude), chunks, &comment)?;
    }
    Ok(format!(
        "project memory of {project} — recorded {} include, {} exclude\n",
        include.len(),
        exclude.len()
    ))
}

/// Non-interactive `funes curate`: record `--include`/`--exclude` decisions, or list the sessions
/// pending review. The text path never creates one — standing up a project memory needs the
/// interactive review's consent — so `memory` must already be one. The interactive review wraps
/// these same pieces (and deferred creation) in the CLI layer.
pub async fn run(memory: &Memory, project: Option<&str>, include: &[String], exclude: &[String]) -> Result<String> {
    let (uri, project) = match prepare(memory, project).await? {
        Prepared::Ready { uri, project } => (uri, project),
        Prepared::Absent { .. } => bail!("{} doesn't exist", memory.label()),
        Prepared::Personal { .. } => {
            bail!(
                "{} is a personal memory, not the project memory of a repo",
                memory.label()
            )
        }
    };
    let mut out = String::new();

    // Recording decisions is a complete action — record, report, and stop.
    if !include.is_empty() || !exclude.is_empty() {
        out.push_str(&record_decisions(&uri, &project, include, exclude).await?);
        return Ok(out);
    }

    // Otherwise list the sessions pending review.
    let found = candidates(memory, &uri, &project, false).await?;
    let skipped = found.other.len() + found.unresolvable.len();
    let line = |out: &mut String, s: &SessionSummary| {
        let sid = &s.session_id[..s.session_id.len().min(8)];
        let _ = writeln!(
            out,
            "  {} {sid}  {}  chunks={}  {}",
            s.date(),
            s.workdir,
            s.chunks,
            s.first_prompt
        );
    };

    if found.matched.is_empty() {
        if skipped > 0 {
            let _ = writeln!(
                out,
                "project memory of {project} — no local session resolves to {project}"
            );
            let _ = writeln!(
                out,
                "  ({skipped} session(s) resolve to other repos or have no resolvable checkout)"
            );
        } else {
            let _ = writeln!(out, "project memory of {project} — nothing new to review");
        }
        return Ok(out);
    }

    let _ = writeln!(
        out,
        "project memory of {project} — {} session(s) resolve to it:",
        found.matched.len()
    );
    for s in &found.matched {
        line(&mut out, s);
    }
    if skipped > 0 {
        let _ = writeln!(
            out,
            "\n{skipped} more skipped ({} in other repos, {} with no resolvable checkout).",
            found.other.len(),
            found.unresolvable.len()
        );
    }
    let _ = writeln!(
        out,
        "\nmark with: funes curate {0} --include <session>…   (or --exclude)",
        memory.label()
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk;

    #[test]
    fn parse_reads_decisions_and_skips_noise() {
        let text = "\
# a header comment
include aaa   # reviewed
exclude bbb
included ccc
include
   \n\
exclude ddd # later";
        let curation = parse(text);
        assert!(curation.include.contains("aaa"));
        assert!(curation.exclude.contains("bbb") && curation.exclude.contains("ddd"));
        // The malformed lines ship nothing: `ccc` has no decision, so it stays pending.
        assert!(!curation.decided("ccc"));
        assert_eq!(curation.include.len(), 1);
        assert_eq!(curation.exclude.len(), 2);
    }

    #[test]
    fn sanitize_is_deterministic_and_path_safe() {
        assert_eq!(sanitize("hf://datasets/acme/kb"), "hf___datasets_acme_kb");
        assert!(!sanitize("a/../b").contains('/'));
    }

    fn by_session(entries: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(s, ids)| (s.to_string(), ids.iter().map(|i| i.to_string()).collect()))
            .collect()
    }

    #[test]
    fn partition_ships_included_holds_pending_drops_excluded() {
        let sessions = by_session(&[
            ("reviewed", &["c1", "c2"] as &[&str]),
            ("secret", &["c3"]),
            ("fresh", &["c4"]),
        ]);
        let curation = parse("include reviewed\nexclude secret\n");
        let (shipped, pending) = partition(&sessions, &curation, &HashSet::new());
        assert_eq!(shipped, HashSet::from(["c1".to_string(), "c2".to_string()]));
        assert_eq!(pending, vec!["fresh".to_string()]);
    }

    #[test]
    fn a_grown_included_session_is_held_for_re_review() {
        // `grown` was included at 2 chunks and now has 3 — past its watermark, so it's stale and
        // held (pending) rather than shipped, like a review dismissed by new commits. `steady` was
        // included at 2 and still has 2, so it ships. `legacy` (no watermark) always ships.
        let sessions = by_session(&[
            ("grown", &["a1", "a2", "a3"] as &[&str]),
            ("steady", &["b1", "b2"]),
            ("legacy", &["c1", "c2"]),
        ]);
        let curation = parse("include grown 2\ninclude steady 2\ninclude legacy\n");
        let (shipped, pending) = partition(&sessions, &curation, &HashSet::new());
        assert_eq!(
            shipped,
            HashSet::from(["b1".into(), "b2".into(), "c1".into(), "c2".into()]),
            "the unchanged and legacy includes ship; the grown one does not"
        );
        assert_eq!(
            pending,
            vec!["grown".to_string()],
            "the grown include is held for re-review"
        );
    }

    #[test]
    fn a_fully_pushed_unfiled_session_is_not_pending() {
        // Its chunks are all on the remote already (e.g. pushed before the file existed):
        // nothing is held back by this push, so there is nothing to report.
        let sessions = by_session(&[("old", &["c1"] as &[&str]), ("new", &["c2"])]);
        let remote = HashSet::from(["c1".to_string()]);
        let (shipped, pending) = partition(&sessions, &Curation::default(), &remote);
        assert!(shipped.is_empty());
        assert_eq!(pending, vec!["new".to_string()]);
    }

    #[test]
    fn publication_distinguishes_local_updated_and_fully_published_sessions() {
        let ids = vec!["a".to_string(), "b".to_string()];
        assert_eq!(publication(&ids, &HashSet::new()), Publication::Local);
        assert_eq!(
            publication(&ids, &HashSet::from(["a".to_string()])),
            Publication::Partial
        );
        assert_eq!(
            publication(&ids, &HashSet::from(["a".to_string(), "b".to_string()])),
            Publication::Published
        );
    }

    #[test]
    fn project_schema_stamps_beside_the_embedder_pin() {
        let schema = project_schema("huggingface/funes");
        assert_eq!(
            schema.metadata().get("project").map(String::as_str),
            Some("huggingface/funes")
        );
        assert!(schema.metadata().contains_key("embedding_model"), "the pin survives");
        assert_eq!(schema.fields(), index::schema().fields(), "fields unchanged");
    }

    /// An empty local dataset written with `schema`, opened back.
    async fn empty_ds(dir: &std::path::Path, schema: Arc<Schema>) -> Dataset {
        let uri = dataset::table_uri(&dir.to_string_lossy());
        let reader = RecordBatchIterator::new(Vec::<Result<RecordBatch, arrow_schema::ArrowError>>::new(), schema);
        Dataset::write(reader, &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        dataset::open(&uri, HashMap::new()).await.unwrap()
    }

    #[tokio::test]
    async fn project_reads_the_stamp_and_a_personal_memory_has_none() {
        let named = tempfile::tempdir().unwrap();
        let ds = empty_ds(named.path(), project_schema("huggingface/funes")).await;
        assert_eq!(project(&ds).as_deref(), Some("huggingface/funes"));

        let personal = tempfile::tempdir().unwrap();
        let ds = empty_ds(personal.path(), index::schema()).await;
        assert!(project(&ds).is_none(), "a plain memory is a personal memory");
    }

    #[test]
    fn an_empty_curation_file_holds_everything_back() {
        let sessions = by_session(&[("a", &["c1"] as &[&str]), ("b", &["c2"])]);
        let (shipped, pending) = partition(&sessions, &parse(""), &HashSet::new());
        assert!(shipped.is_empty());
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn decision_line_carries_the_watermark_and_round_trips() {
        assert_eq!(
            decision_line(Decision::Include, "bbb", 7, "2026-07-15 fix the parser"),
            "include bbb 7   # 2026-07-15 fix the parser\n"
        );
        assert_eq!(
            decision_line(Decision::Include, "ccc", 5, "  "),
            "include ccc 5\n",
            "blank comment → no #"
        );
        assert_eq!(
            decision_line(Decision::Exclude, "ddd", 9, "private"),
            "exclude ddd   # private\n",
            "exclude carries no count"
        );
        // Round-trips through the parser the file is read back with, watermark and all.
        let text = format!(
            "{}{}",
            decision_line(Decision::Include, "bbb", 7, ""),
            decision_line(Decision::Exclude, "ddd", 9, "")
        );
        let parsed = parse(&text);
        assert!(parsed.include.contains("bbb") && parsed.exclude.contains("ddd"));
        assert!(
            !parsed.is_stale("bbb", 7) && parsed.is_stale("bbb", 8),
            "the reviewed-at count round-trips"
        );
    }

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
    fn approval_review_requires_the_wrapper_structure() {
        let signals = [
            "The following is the Codex agent history whose request action you are assessing.",
            ">>> APPROVAL REQUEST START",
            ">>> APPROVAL REQUEST END",
        ]
        .into_iter()
        .fold(0, |signals, text| signals | approval_review_signal(text));
        assert!(is_approval_review(signals));
        assert!(!is_approval_review(approval_review_signal(
            "It has incredibly long text ending with APPROVAL REQUEST END"
        )));
    }

    #[tokio::test]
    async fn candidate_sessions_hide_codex_subagents_and_indexed_guardians() {
        let root = tempfile::tempdir().unwrap();
        let guardian_path = root.path().join("rollout-guardian.jsonl");
        std::fs::write(
            &guardian_path,
            br#"{"type":"session_meta","payload":{"id":"guardian","thread_source":"subagent","source":{"subagent":{"other":"guardian"}}}}
"#,
        )
        .unwrap();
        let primary_path = root.path().join("rollout-primary.jsonl");
        std::fs::write(
            &primary_path,
            br#"{"type":"session_meta","payload":{"id":"primary","thread_source":"cli"}}
"#,
        )
        .unwrap();

        let rows = [
            (
                "guardian",
                guardian_path.to_string_lossy().into_owned(),
                "wrapped transcript",
            ),
            ("primary", primary_path.to_string_lossy().into_owned(), "real work"),
            (
                "orphaned-guardian",
                "/gone/guardian.jsonl".into(),
                "The following is the Codex agent history whose request action you are assessing.",
            ),
            (
                "orphaned-guardian",
                "/gone/guardian.jsonl".into(),
                ">>> APPROVAL REQUEST START",
            ),
            (
                "orphaned-guardian",
                "/gone/guardian.jsonl".into(),
                ">>> APPROVAL REQUEST END",
            ),
            (
                "discussion",
                primary_path.to_string_lossy().into_owned(),
                "This preview ends with APPROVAL REQUEST END",
            ),
        ];
        let chunks: Vec<chunk::Chunk> = rows
            .iter()
            .enumerate()
            .map(|(i, (session, source_path, text))| chunk::Chunk {
                id: format!("id{i}"),
                text: (*text).into(),
                session_id: (*session).into(),
                workdir: "w".into(),
                turn_uuid: format!("u{i}"),
                parent_uuid: None,
                seq: i as i64,
                ts: "2026-07-22T00:00:00Z".into(),
                role: "user".into(),
                block_type: "text".into(),
                tool_name: None,
                source_path: source_path.clone(),
                block_idx: 0,
                split_idx: 0,
                harness: "codex".into(),
                repo: "huggingface/funes".into(),
            })
            .collect();
        let vectors = vec![vec![0.0f32; index::DIM as usize]; chunks.len()];
        let batch = index::build_batch(&chunks, &vectors).unwrap();
        let memory_dir = root.path().join("memory");
        let uri = dataset::table_uri(&memory_dir.to_string_lossy());
        let reader = RecordBatchIterator::new(vec![Ok(batch.clone())], batch.schema());
        Dataset::write(reader, &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        let ds = dataset::open(&uri, HashMap::new()).await.unwrap();

        let candidates = candidate_sessions(&ds).await.unwrap();
        assert_eq!(
            candidates.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from(["discussion".to_string(), "primary".to_string()])
        );
    }

    #[test]
    fn discover_groups_matched_other_and_unresolvable() {
        let mk = |sid: &str, repo: &str| SessionSummary {
            session_id: sid.to_string(),
            chunks: 1,
            last_ts: "2026-07-15T00:00:00Z".into(),
            workdir: String::new(),
            first_prompt: String::new(),
            repo: repo.to_string(),
            publication: Publication::Local,
        };
        let sessions = vec![
            mk("matched", "acme/widget"),
            mk("matched-fork", "acme/widget upstream/widget"), // any of the space-joined ids counts
            mk("elsewhere", "acme/other"),
            mk("gone", ""),
        ];
        let found = discover(sessions, "acme/widget");
        let ids = |v: &[SessionSummary]| v.iter().map(|s| s.session_id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&found.matched), ["matched", "matched-fork"]);
        assert_eq!(ids(&found.other), ["elsewhere"], "repo names a different owner/name");
        assert_eq!(ids(&found.unresolvable), ["gone"], "no repo stored");
    }

    /// A local dataset of one chunk per `(session_id, repo)` row — enough to exercise repo-scoped
    /// selection.
    async fn ds_with_repos(dir: &std::path::Path, rows: &[(&str, &str)]) -> Dataset {
        let chunks: Vec<chunk::Chunk> = rows
            .iter()
            .enumerate()
            .map(|(i, (session, repo))| chunk::Chunk {
                id: format!("id{i}"),
                text: "t".into(),
                session_id: (*session).into(),
                workdir: "w".into(),
                turn_uuid: format!("u{i}"),
                parent_uuid: None,
                seq: i as i64,
                ts: "2026-01-01T00:00:00Z".into(),
                role: "user".into(),
                block_type: "text".into(),
                tool_name: None,
                source_path: "/x.jsonl".into(),
                block_idx: 0,
                split_idx: 0,
                harness: "claude_code".into(),
                repo: (*repo).into(),
            })
            .collect();
        let vectors = vec![vec![0.0f32; index::DIM as usize]; chunks.len()];
        let batch = index::build_batch(&chunks, &vectors).unwrap();
        let schema = batch.schema();
        let uri = dataset::table_uri(&dir.to_string_lossy());
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        Dataset::write(reader, &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        dataset::open(&uri, HashMap::new()).await.unwrap()
    }

    #[tokio::test]
    async fn project_sessions_selects_only_repo_matches() {
        let dir = tempfile::tempdir().unwrap();
        let ds = ds_with_repos(
            dir.path(),
            &[
                ("mine", "huggingface/funes"),
                ("fork", "acme/x huggingface/funes"), // any space-joined id counts
                ("other", "acme/other"),
                ("nogit", ""),
            ],
        )
        .await;
        let matched = project_sessions(&ds, "huggingface/funes").await.unwrap();
        assert_eq!(
            matched,
            HashSet::from(["mine".to_string(), "fork".to_string()]),
            "only sessions whose repo names the project are scoped in — others aren't its pending"
        );
    }
}
