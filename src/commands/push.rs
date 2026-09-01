//! `push`: publish the local memory's not-yet-remote chunks into a remote memory on the HF Hub.
//!
//! Streamed, never a full mirror. "What's already there" is the memory's own chunk ids, so the
//! delta is `local_ids − remote_ids` — the same primitive `index` uses. `push` is orchestration:
//! it computes the delta, holds back any row that still contains a secret (redaction happens at
//! index time; this is the egress backstop — the rows wait for `funes scrub`), and drives the HF
//! write operations in [`crate::memory::remote`], which own the atomic, parent-commit-guarded commits.
//!
//! - **First publish:** build the dataset locally (data + FTS/IVF indexes) and upload every file in
//!   one commit.
//! - **Append:** [`remote::append`] lands the new fragment + manifest + transaction in one
//!   guarded `create_commit`, retried against a fresh head if a concurrent push moved it. The new
//!   rows are left unindexed (a query still finds them by brute force).
//! - **Reindex:** a *separate* guarded commit ([`remote::reindex`]), kept off the data commit so
//!   the data commit stays small. `push` runs it after the data commit when the unindexed backlog
//!   crosses [`REINDEX_THRESHOLD`] (best-effort: a head-moved conflict is a warning, the next push
//!   retries), or eagerly with `--force-reindex` (retried until it lands).
//!
//! What a push ships is either everything the local memory holds that the remote doesn't, or —
//! with `--sessions` — exactly the sessions named. The list is the decision; nothing else gates it.

use crate::hub;
use crate::memory::card::{self, CardAction, CardCtx};
use crate::memory::dataset;
use crate::memory::lock;
use crate::memory::remote::{self, Appended, Reindexed};
use crate::memory::{Memory, MemoryState};
use crate::{chunk, scan};
use anyhow::{bail, Context, Result};
use arrow_array::{BooleanArray, RecordBatch, StringArray};
use arrow_select::filter::filter_record_batch;
use bytes::Bytes;
use chrono::Utc;
use hf_hub::{HFError, HFRepository, RepoTypeDataset};
use lance::Dataset;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Reindex the remote once this many appended rows are sitting unindexed (answered by a
/// brute-force scan until folded in). Bounds per-query cost, not push count, and is stateless —
/// [`remote::append`] reads it straight from Lance's index stats. Nonzero so tiny per-push
/// deltas don't pile up between compactions.
const REINDEX_THRESHOLD: u64 = 500;

/// Cap on CAS-conflict retries (the data append, and a forced reindex) when the branch head keeps
/// moving under us, so a busy remote can't spin forever.
const MAX_COMMIT_RETRIES: u32 = 10;

/// Every chunk id in a memory, or empty if it can't be opened (absent local index, not-yet-created
/// or inaccessible remote).
pub async fn memory_ids(memory: &Memory) -> HashSet<String> {
    match memory.open().await {
        Ok(ds) => all_ids(&ds).await.unwrap_or_default(),
        Err(_) => HashSet::new(),
    }
}

/// Whether a publish error is the Hub refusing the write — a 403/Forbidden, i.e. the token can't
/// write to this remote. Matches the typed [`HFError`] preserved in the error chain.
pub fn is_read_only(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| match cause.downcast_ref::<HFError>() {
        Some(HFError::Forbidden { .. }) => true,
        Some(HFError::Http { context }) => context.status.as_u16() == 403,
        _ => false,
    })
}

/// Every chunk id in a memory (a plain `id`-column scan; plain scans aren't limit-capped).
async fn all_ids(ds: &Dataset) -> Result<HashSet<String>> {
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

/// Per-remote local receipt of chunk ids this host has observed on that remote. It is deliberately
/// local: a shared remote's sessions from other hosts say nothing about this host's push backlog.
fn pushed_path(memory_uri: &str) -> PathBuf {
    dataset::funes_dir().join("pushed").join(sanitize(memory_uri))
}

/// The file a publish to `memory_uri` locks for its whole run — not the receipt's lock, which
/// [`record_pushed`] takes per write.
fn push_lock_path(memory_uri: &str) -> PathBuf {
    dataset::funes_dir()
        .join("pushed")
        .join(".locks")
        .join(format!("{}.push", sanitize(memory_uri)))
}

fn load_pushed_from(path: &Path) -> Option<HashSet<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn load_pushed(memory_uri: &str) -> Option<HashSet<String>> {
    load_pushed_from(&pushed_path(memory_uri))
}

fn record_pushed_at<'a>(path: &Path, ids: impl IntoIterator<Item = &'a String>) -> Result<()> {
    let dir = path.parent().context("push receipt has no parent directory")?;
    std::fs::create_dir_all(dir).context("creating the push receipt directory")?;

    // Lock a stable sibling rather than the receipt itself: the receipt is atomically replaced,
    // so locking its inode would let a second writer lock the replacement and race the first.
    let lock_dir = dir.join(".locks");
    std::fs::create_dir_all(&lock_dir).context("creating the push receipt lock directory")?;
    let lock_path = lock_dir.join(path.file_name().context("push receipt has no file name")?);
    let lock = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening push receipt lock at {}", lock_path.display()))?;
    lock.lock().context("locking the push receipt")?;

    let mut pushed = load_pushed_from(path).unwrap_or_default();
    let before = pushed.len();
    pushed.extend(ids.into_iter().cloned());
    if pushed.len() == before && path.is_file() {
        return Ok(());
    }

    let mut ids: Vec<_> = pushed.into_iter().collect();
    ids.sort_unstable();
    let mut temp = tempfile::NamedTempFile::new_in(dir).context("creating a push receipt")?;
    for id in ids {
        writeln!(temp, "{id}").context("writing the push receipt")?;
    }
    temp.flush().context("flushing the push receipt")?;
    temp.persist(path)
        .map_err(|e| anyhow::Error::new(e.error).context("saving the push receipt"))?;
    Ok(())
}

fn record_pushed<'a>(memory_uri: &str, ids: impl IntoIterator<Item = &'a String>) -> Result<()> {
    record_pushed_at(&pushed_path(memory_uri), ids)
}

/// This host's session-level push coverage for one remote. A session is fully pushed only when
/// every chunk it currently has is in the local receipt, so a session that later grows returns to
/// `pending` until its new chunks are pushed.
pub(crate) struct PushCoverage {
    pub total: usize,
    pub pending: usize,
    pub held: Option<Skipped>,
}

fn push_coverage(batches: &[RecordBatch], pushed_ids: &HashSet<String>) -> Option<PushCoverage> {
    let mut complete: HashMap<String, bool> = HashMap::new();
    for batch in batches {
        let ids = batch.column_by_name("id")?.as_any().downcast_ref::<StringArray>()?;
        let sessions = batch
            .column_by_name("session_id")?
            .as_any()
            .downcast_ref::<StringArray>()?;
        for i in 0..batch.num_rows() {
            let present = pushed_ids.contains(ids.value(i));
            complete
                .entry(sessions.value(i).to_string())
                .and_modify(|all| *all &= present)
                .or_insert(present);
        }
    }
    let pushed = complete.values().filter(|&&all| all).count();
    Some(PushCoverage {
        total: complete.len(),
        pending: complete.len() - pushed,
        held: None,
    })
}

pub(crate) async fn local_push_coverage(local: &Dataset, memory_uri: &str) -> Option<PushCoverage> {
    let pushed_ids = load_pushed(memory_uri)?;
    let batches = dataset::scan_rows(local, &["id", "session_id"], None, None)
        .await
        .ok()?;
    let mut coverage = push_coverage(&batches, &pushed_ids)?;
    if coverage.pending > 0 {
        let pending: HashSet<String> = ids_in_batches(&batches).difference(&pushed_ids).cloned().collect();
        coverage.held = held_among(local, &pending).await;
    }
    Some(coverage)
}

/// What the secret gate would hold back of the `pending` rows, scanned exactly as a push would.
async fn held_among(local: &Dataset, pending: &HashSet<String>) -> Option<Skipped> {
    if pending.is_empty() {
        return None;
    }
    let rows = rows_with_ids(local, pending).await.ok()?;
    let (_, skipped) = drop_secret_rows(rows).ok()?;
    (skipped.rows > 0).then_some(skipped)
}

fn ids_in_batches(batches: &[RecordBatch]) -> HashSet<String> {
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
    ids
}

/// Whole rows of the local memory — every column, since a publish ships the row as it stands.
async fn read_rows(local: &Dataset, filter: Option<&str>) -> Result<Vec<RecordBatch>> {
    dataset::scan_rows(local, &[], filter, None).await
}

/// Every row. Naming a whole memory's ids instead would mean a 150k-term `id IN (…)` predicate,
/// which costs far more than the scan it filters.
async fn all_rows(local: &Dataset) -> Result<Vec<RecordBatch>> {
    read_rows(local, None).await
}

/// The rows whose id is in `ids`.
async fn rows_with_ids(local: &Dataset, ids: &HashSet<String>) -> Result<Vec<RecordBatch>> {
    let list = ids.iter().map(|id| format!("'{id}'")).collect::<Vec<_>>().join(", ");
    read_rows(local, Some(&format!("id IN ({list})"))).await
}

/// The outcome of a [`run_push`]: the report to print, and `blocked` — true when the secret gate
/// held *everything* back so nothing was published. A caller (the CLI) can exit non-zero on
/// `blocked` so automation sees that secrets, not success, stopped the publish.
pub struct Pushed {
    pub report: String,
    pub blocked: bool,
}

/// A plain report (nothing blocked) — most return paths.
impl From<String> for Pushed {
    fn from(report: String) -> Self {
        Pushed { report, blocked: false }
    }
}

/// How a push handles a target the local index shares no chunks with.
pub enum Confirm {
    /// Proceed without asking (`--yes`, or a caller that has already established intent, e.g. tests).
    Yes,
    /// Ask before publishing. Called with the target label and the number of chunks to be pushed.
    Ask(fn(&str, usize) -> bool),
}

impl Confirm {
    /// Whether the push may proceed to a share-nothing target.
    fn proceed(self, label: &str, chunks: usize) -> bool {
        match self {
            Confirm::Yes => true,
            Confirm::Ask(ask) => ask(label, chunks),
        }
    }
}

/// Whether a push must be confirmed first: there are rows to publish and the local index shares
/// no chunk with the remote — a first publish, a new host of yours, or the wrong memory.
fn must_confirm(local: usize, to_push: usize) -> bool {
    to_push > 0 && to_push == local
}

/// A memory URI as one path-safe filename — the push receipt's key.
pub(crate) fn sanitize(uri: &str) -> String {
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

/// A memory's rows as session → chunk ids, the shape a selection is resolved against.
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

/// The chunk ids of exactly `sessions`, from a memory's rows grouped as `by_session`. Every named
/// session must be in the memory: a mistyped id would otherwise publish a subset silently, and a
/// publication is not a place to guess.
fn named_ids(by_session: &HashMap<String, Vec<String>>, sessions: &[String]) -> Result<HashSet<String>> {
    let unknown: Vec<&str> = sessions
        .iter()
        .filter(|s| !by_session.contains_key(*s))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        bail!(
            "not in your local memory: {} — `funes sessions` lists what is",
            unknown.join(", ")
        );
    }
    Ok(sessions.iter().flat_map(|s| by_session[s].iter().cloned()).collect())
}

/// Publish the local memory's new chunks to `target` (a remote memory on the HF Hub). With
/// `force_reindex`, refresh the remote index after the data commit (retrying until it lands) even
/// if the unindexed backlog is below [`REINDEX_THRESHOLD`]; with no new chunks pending it's a pure
/// index refresh. `confirm` gates a publish to a memory the local index shares no chunks with.
///
/// `sessions`, when non-empty, is the publication itself: exactly those sessions' chunks are
/// candidates. Empty publishes everything the local memory holds that the remote does not.
pub async fn run_push(target: Memory, force_reindex: bool, confirm: Confirm, sessions: &[String]) -> Result<Pushed> {
    let uri = match &target {
        Memory::Remote { uri } => uri.clone(),
        Memory::Local { .. } => {
            bail!("push target must be a remote `hf://` memory — it publishes your local index up to the Hub")
        }
    };

    // A push's delta is only valid against the head it read, so a concurrent one republishes those rows.
    let Some(_push_lock) = lock::try_lock_file(&push_lock_path(&uri))? else {
        return Ok(format!(
            "{}: another push to this memory is in flight — skipped\n",
            target.label()
        )
        .into());
    };

    // 1. What the remote *is*, and what's on it — asked before any local build or scan, so an
    // unreachable or missing remote fails fast. An unreadable dataset (a `check_compat` rejection
    // included) is a hard error, never "empty": treating it as a first publish would commit a fresh
    // dataset's files into a repo that already holds one.
    let state = target.state().await.map_err(|e| {
        e.context(format!(
            "{} exists but can't be read — refusing to treat it as empty",
            target.label()
        ))
    })?;
    let remote = match state {
        MemoryState::Offline => {
            bail!("{uri} is unreachable — can't push while offline (check your connection)")
        }
        MemoryState::Missing => return Err(target.missing_error()),
        MemoryState::Unauthorized => return Err(target.unauthorized_error()),
        MemoryState::Empty => None,
        MemoryState::Ready(ds) => Some(ds),
    };

    // Only `Empty` can land here: a local path has no Hub states, and an unreadable memory is an
    // `Err`. A host whose first index isn't built yet has nothing to publish — that is not a failure.
    let MemoryState::Ready(local) = Memory::local().state().await? else {
        return Ok(format!(
            "{}: nothing indexed on this machine yet — nothing to publish\n",
            target.label()
        )
        .into());
    };

    eprintln!("comparing local and remote indexes…");
    let remote_ids = match &remote {
        Some(ds) => all_ids(ds).await?,
        None => HashSet::new(),
    };
    let first_publish = remote.is_none();

    // 2. The local side. Named sessions are the selection outright: the caller has said what to
    // publish. Otherwise everything local is a candidate, and the remote's own ids decide what of
    // it is new.
    let candidates = if sessions.is_empty() {
        all_ids(&local).await?
    } else {
        eprintln!("publishing {} named session(s)", sessions.len());
        named_ids(&ids_by_session(&local).await?, sessions)?
    };
    let to_push: HashSet<String> = candidates.difference(&remote_ids).cloned().collect();

    // Bootstrap/refresh the local receipt from facts the push comparison has already established.
    // This makes a no-op push enough to initialize status for a legacy remote, with no extra scan.
    if remote.is_some() {
        record_pushed(&uri, candidates.intersection(&remote_ids))?;
    }

    // Nothing to push => done (no token needed), unless this is a forced reindex of an existing
    // remote, which is still work.
    if to_push.is_empty() && (first_publish || !force_reindex) {
        return Ok(format!("{}: already up to date ({} chunks)\n", target.label(), remote_ids.len()).into());
    }

    // 2. HF repo handle. Resolve the target and token before the confirmation, so a bad URI or a
    // missing token fails before we prompt for one.
    let (owner, name, prefix) = hub::parse_hf(&uri)?;
    let repo_id = format!("{owner}/{name}");
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to push")?;

    // When required, ask for confirmation before publishing.
    if must_confirm(candidates.len(), to_push.len()) && !confirm.proceed(&target.label(), to_push.len()) {
        bail!("push aborted");
    }

    let repo = hub::client(Some(token.as_str()), true)?.dataset(owner, name);
    // No revision pinning: always the `main` branch head.
    let rev = "main".to_string();
    let dataset_uri = format!("{uri}/{}.lance", dataset::TABLE);
    let opts = HashMap::from([("hf_token".to_string(), token), ("revision".to_string(), rev.clone())]);

    // 3. Forced reindex with no new data: just refresh the remote index and stop.
    if to_push.is_empty() {
        eprintln!("refreshing the remote index…");
        let note = reindex_forced(&repo, &dataset_uri, &opts, &rev).await?;
        return Ok(format!("{}: up to date ({} chunks)\n{note}", target.label(), remote_ids.len()).into());
    }

    // 4. Rows, then hold back any that still contain a secret. Re-stamp each batch with the local
    // dataset's schema so its metadata (the embedding-model id) rides along — scan-result batches
    // drop it, and on first publish that schema is what the new dataset persists.
    let schema: arrow_schema::SchemaRef = Arc::new(arrow_schema::Schema::from(local.schema()));
    // Only a first publish with nothing named ships the whole memory; anything else, `to_push`
    // is the answer — including a first publish of a selection.
    let rows = if first_publish && sessions.is_empty() {
        all_rows(&local).await?
    } else {
        rows_with_ids(&local, &to_push).await?
    };
    let batches: Vec<RecordBatch> = rows
        .into_iter()
        .map(|b| RecordBatch::try_new(schema.clone(), b.columns().to_vec()))
        .collect::<std::result::Result<_, _>>()?;

    // Drop any row whose text still holds a secret — hold it back from the Hub rather than block the
    // whole push. `funes scrub` redacts it in the local memory; the next push then ships it.
    let n_scanning: usize = batches.iter().map(|b| b.num_rows()).sum();
    eprintln!("scanning {n_scanning} chunk(s) for secrets…");
    let (batches, skipped) = drop_secret_rows(batches)?;
    let n_chunks: usize = batches.iter().map(|b| b.num_rows()).sum();
    if n_chunks == 0 {
        // Everything was held back: nothing reached the Hub. Mark `blocked` so the CLI exits non-zero
        // — automation must not read this as a successful publish.
        return Ok(Pushed {
            report: format!(
                "{}: nothing published — held back {} row(s) with secrets ({}); run `funes scrub`, then push again\n",
                target.label(),
                skipped.rows,
                skipped.summary
            ),
            blocked: true,
        });
    }
    let pushed_ids = ids_in_batches(&batches);

    // The dataset card rides the data commit — created when the repo has none, refreshed when it
    // carries the funes markers, a hand-written card left alone (see `card_file`). Root stores
    // only: the root README describes the whole repo, which a prefixed memory is only part of
    // (and two stores sharing a repo would fight over one card's stats) — under a prefix it's
    // the owner's. The chunk count is the post-push total — best-effort: rows another writer
    // lands concurrently are missed until the next push refreshes the card.
    let date = Utc::now().format("%Y-%m-%d").to_string();
    let ctx = CardCtx {
        repo: &repo_id,
        chunks: remote_ids.len() as u64 + n_chunks as u64,
        embedding_model: embedding_model(&schema),
        date: &date,
    };
    let (card_body, card_note) = if prefix.is_empty() {
        card_file(remote::fetch_readme(&repo, &rev).await, &ctx)
    } else {
        (None, String::new())
    };

    let message = format!("funes push: +{n_chunks} chunks");
    // The dataset card rides the same commit as the data, on a first publish or an append.
    let extra: BTreeMap<String, Bytes> = card_body
        .map(|body| BTreeMap::from([("README.md".to_string(), Bytes::from(body))]))
        .unwrap_or_default();

    // 5. First publish: `remote` builds the dataset locally (data + indexes) and uploads it in
    // one commit; the card rides along.
    if first_publish {
        eprintln!("building the dataset to publish…");
        let oid = remote::first_publish(
            &repo,
            &prefix,
            batches,
            schema.clone(),
            &rev,
            message,
            &extra,
            |phase| eprintln!("building {phase}…"),
        )
        .await?;
        let Some(oid) = oid else {
            return Ok(format!("{}: nothing new to upload\n", target.label()).into());
        };
        record_pushed(&uri, &pushed_ids)?;
        return Ok(format!(
            "{}: pushed {n_chunks} chunks (commit {oid})\n{card_note}{}",
            target.label(),
            skipped.warning()
        )
        .into());
    }

    // 6. Append the data and commit it, retrying against a fresh head if a concurrent push moved it
    // (each attempt re-appends onto the new manifest — the data commit is small, so this is cheap).
    eprintln!("uploading {n_chunks} chunk(s) to {}…", target.label());
    let mut attempts = 0u32;
    let (oid, unindexed) = loop {
        let attempt = remote::append(
            &repo,
            &dataset_uri,
            opts.clone(),
            &rev,
            message.clone(),
            batches.clone(),
            schema.clone(),
            &extra,
        )
        .await?;
        match attempt {
            Appended::Committed { oid, unindexed } => break (oid, unindexed),
            Appended::Conflict => {
                attempts += 1;
                if attempts > MAX_COMMIT_RETRIES {
                    bail!("data commit kept conflicting after {MAX_COMMIT_RETRIES} retries; re-run push");
                }
            }
        }
    };
    record_pushed(&uri, &pushed_ids)?;
    let mut out = format!("{}: pushed {n_chunks} chunks (commit {oid})\n", target.label());
    out.push_str(&card_note);

    // 7. Reindex as a separate commit: forced (retried until it lands) or, past the threshold,
    // best-effort (one shot, warn on a conflict — the next push retries).
    if force_reindex {
        eprintln!("refreshing the remote index…");
        out.push_str(&reindex_forced(&repo, &dataset_uri, &opts, &rev).await?);
    } else if unindexed > REINDEX_THRESHOLD {
        eprintln!("refreshing the remote index…");
        out.push_str(&reindex_auto(&repo, &dataset_uri, &opts, &rev).await);
    }
    out.push_str(&skipped.warning());
    Ok(out.into())
}

/// The memory's pinned embedding model, from the schema metadata `index` stamps. A pre-metadata
/// memory has no id to show.
fn embedding_model(schema: &arrow_schema::Schema) -> &str {
    schema
        .metadata()
        .get("embedding_model")
        .map(String::as_str)
        .unwrap_or("unknown")
}

/// What a push does about the dataset card, from the remote README fetch (`Err` = unreadable):
/// the content to commit as `README.md`, if any, and the report line. An unreadable README is
/// indistinguishable from a hand-written card, so it's left alone rather than risk clobbering
/// it — the next push retries.
fn card_file(remote_readme: Result<Option<String>>, ctx: &CardCtx) -> (Option<String>, String) {
    let existing = match remote_readme {
        Ok(existing) => existing,
        Err(e) => {
            return (
                None,
                format!("  note: dataset card left untouched (couldn't read the current one: {e})\n"),
            )
        }
    };
    match card::plan(existing.as_deref(), ctx) {
        CardAction::Create(text) => (Some(text), "  dataset card created\n".into()),
        CardAction::Refresh(text) => (Some(text), "  dataset card refreshed\n".into()),
        CardAction::LeaveAlone => (None, String::new()),
    }
}

/// Rows held back from a push because their text still contained a secret.
pub(crate) struct Skipped {
    pub rows: usize,
    /// Detectors that fired, e.g. `PrivateKey×1, AWS×2` — empty when nothing was held back.
    pub summary: String,
}

impl Skipped {
    /// A loud trailing line for the push output, or empty if nothing was held back.
    fn warning(&self) -> String {
        if self.rows == 0 {
            return String::new();
        }
        format!(
            "⚠ held back {} row(s) containing secrets ({}) — run `funes scrub` to redact them, then push again\n",
            self.rows, self.summary
        )
    }
}

/// Scan the to-push `batches` and hold back every row of any *block* that holds a secret, returning
/// the clean batches and what was held back. Detection works at block granularity: a block's chunks
/// are reconstructed into their contiguous text (so a secret `split` cut across chunks is whole and
/// detectable), scanned in one pass, and a finding is attributed to its block by line number — never
/// by matching the secret's value, which fails on text stored with escaped or quoted bytes. If any
/// chunk of a block is dirty, the whole block is held back (its other chunks carry the rest of the
/// secret). Fail-closed on the scanner — a push must scan before it uploads.
fn drop_secret_rows(batches: Vec<RecordBatch>) -> Result<(Vec<RecordBatch>, Skipped)> {
    let scanner = scan::Trufflehog::find()?;
    // Row order across batches matches `chunks_from_batches`, so a chunk's index is its global row.
    let chunks = chunk::chunks_from_batches(&batches);
    let blocks = chunk::reconstruct_blocks(&chunks);
    let texts: Vec<&str> = blocks.iter().map(|(_, text)| text.as_str()).collect();
    let found = scan::scan_blocks(&texts, &scanner)?;

    let mut dirty = vec![false; chunks.len()];
    let mut detectors: Vec<String> = Vec::new();
    for ((idxs, _), findings) in blocks.iter().zip(&found) {
        if findings.is_empty() {
            continue;
        }
        for &i in idxs {
            dirty[i] = true;
        }
        // One tally per (block, distinct detector), so the warning reads "PrivateKey×<blocks>".
        detectors.extend(scan::detectors(findings));
    }
    let dropped = dirty.iter().filter(|&&d| d).count();
    if dropped == 0 {
        return Ok((
            batches,
            Skipped {
                rows: 0,
                summary: String::new(),
            },
        ));
    }

    // Drop the dirty rows batch by batch, mapping each global row index back via a running offset.
    let mut clean = Vec::with_capacity(batches.len());
    let mut base = 0usize;
    for b in &batches {
        let mask: BooleanArray = (0..b.num_rows()).map(|i| !dirty[base + i]).collect();
        clean.push(filter_record_batch(b, &mask)?);
        base += b.num_rows();
    }
    let summary = scan::summary(detectors.iter().map(String::as_str));
    Ok((clean, Skipped { rows: dropped, summary }))
}

/// Forced reindex: ask [`remote::reindex`] to refresh and commit, retrying on a head-moved
/// conflict (it re-reads the head each call) until it lands or [`MAX_COMMIT_RETRIES`] is exceeded.
async fn reindex_forced(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    opts: &HashMap<String, String>,
    rev: &str,
) -> Result<String> {
    for _ in 0..=MAX_COMMIT_RETRIES {
        match remote::reindex(repo, dataset_uri, opts.clone(), rev, "funes push: reindex".to_string()).await? {
            Reindexed::Committed(oid) => return Ok(format!("  reindexed (commit {oid})\n")),
            Reindexed::AlreadyCurrent => return Ok("  index already current\n".to_string()),
            Reindexed::Conflict => continue,
        }
    }
    bail!("reindex still conflicting after {MAX_COMMIT_RETRIES} retries; re-run push --force-reindex")
}

/// Best-effort reindex during a normal push: one attempt, never retried. The data is already
/// committed, so any failure here is a warning — the next push past the threshold tries again.
async fn reindex_auto(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    opts: &HashMap<String, String>,
    rev: &str,
) -> String {
    match remote::reindex(repo, dataset_uri, opts.clone(), rev, "funes push: reindex".to_string()).await {
        Ok(Reindexed::Committed(oid)) => format!("  reindexed (commit {oid})\n"),
        Ok(Reindexed::AlreadyCurrent) => String::new(),
        Ok(Reindexed::Conflict) => {
            "  note: index not refreshed (remote head moved); will retry on a later push\n".to_string()
        }
        Err(e) => format!("  note: index not refreshed ({e}); will retry on a later push\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::RecordBatchIterator;
    use lance::dataset::WriteParams;

    #[test]
    fn must_confirm_only_when_overlap_is_empty_and_there_is_work() {
        // First publish / fully disjoint (every local chunk is new to the remote) → confirm.
        assert!(must_confirm(5, 5));
        assert!(must_confirm(1, 1));
        // Some overlap (fewer to push than the local total) → no prompt, it's a memory you add to.
        assert!(!must_confirm(5, 3));
        // Nothing to push (up to date, or a reindex-only run) → never prompt, even with 0 overlap.
        assert!(!must_confirm(5, 0));
        assert!(!must_confirm(0, 0));
    }

    #[test]
    fn is_read_only_matches_the_type_not_the_message() {
        // We match the typed HFError, not the rendered text — a plain error that merely mentions
        // 403/Forbidden is not a read-only signal. (HFError is #[non_exhaustive], so the positive
        // path can't be built here; it's exercised by the gated round-trip.)
        assert!(!is_read_only(&anyhow::anyhow!("server said 403 Forbidden")));
        assert!(!is_read_only(&anyhow::anyhow!("no HF token")));
    }

    /// Card context for the [`card_file`] tests.
    fn card_ctx(chunks: u64) -> CardCtx<'static> {
        CardCtx {
            repo: "acme/kb",
            chunks,
            embedding_model: "BAAI/bge-small-en-v1.5",
            date: "2026-07-16",
        }
    }

    #[test]
    fn card_file_creates_when_the_remote_has_none() {
        let (body, note) = card_file(Ok(None), &card_ctx(10));
        assert!(body.expect("a card").contains("--memory acme/kb"));
        assert_eq!(note, "  dataset card created\n");
    }

    #[test]
    fn card_file_refreshes_a_funes_card() {
        let CardAction::Create(current) = card::plan(None, &card_ctx(10)) else {
            panic!("expected Create");
        };
        let (body, note) = card_file(Ok(Some(current)), &card_ctx(999));
        assert!(body.expect("a refresh").contains("| Chunks | 999 |"));
        assert_eq!(note, "  dataset card refreshed\n");
    }

    #[test]
    fn card_file_never_touches_a_hand_written_card() {
        let (body, note) = card_file(Ok(Some("# my memory\n".into())), &card_ctx(10));
        assert!(body.is_none() && note.is_empty());
    }

    #[test]
    fn card_file_skips_when_the_remote_readme_is_unreadable() {
        let (body, note) = card_file(Err(anyhow::anyhow!("504")), &card_ctx(10));
        assert!(body.is_none());
        assert!(note.contains("left untouched"), "note: {note}");
    }

    use crate::traces::{Block, Turn};
    use std::process::Command;

    /// Mint a throwaway key (never committed, so funes ships no secret) of the given type, or None
    /// if keygen is unavailable.
    fn keygen(args: &[&str]) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let kf = dir.path().join("k");
        let ok = Command::new("ssh-keygen")
            .args(args)
            .args(["-N", "", "-q", "-f"])
            .arg(&kf)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then(|| std::fs::read_to_string(&kf).unwrap())
    }

    /// One turn per text, each text its own block — so distinct texts land in distinct blocks.
    fn turn(idx: i64, block_text: &str) -> Turn {
        Turn {
            session_id: "sess".into(),
            workdir: "proj".into(),
            turn_uuid: format!("turn{idx}"),
            parent_uuid: None,
            seq: idx,
            ts: "2026-01-01T00:00:00Z".into(),
            role: "assistant".into(),
            blocks: vec![Block {
                block_type: "text".into(),
                text: block_text.into(),
                tool_name: None,
                tool_use_id: None,
            }],
            source_path: "/x.jsonl".into(),
            harness: "claude_code".into(),
        }
    }

    /// Like [`turn`], but in a named session — for exercising a session-level selection.
    fn turn_sess(session: &str, idx: i64, block_text: &str) -> Turn {
        Turn {
            session_id: session.into(),
            ..turn(idx, block_text)
        }
    }

    /// Build a to-push batch the way the memory stores it: chunk the turns, stamp zero vectors.
    fn batch(turns: &[Turn]) -> (RecordBatch, Vec<chunk::Chunk>) {
        let chunks = chunk::chunks_from_turns(turns, &chunk::Tier::ALL, true);
        let vectors = vec![vec![0.0f32; dataset::DIM as usize]; chunks.len()];
        (dataset::build_batch(&chunks, &vectors).unwrap(), chunks)
    }

    #[test]
    fn drop_secret_rows_holds_back_a_single_chunk_secret() {
        if scan::Trufflehog::find().is_err() {
            eprintln!("skip: trufflehog not found");
            return;
        }
        let Some(key) = keygen(&["-t", "ed25519"]) else {
            eprintln!("skip: ssh-keygen unavailable");
            return;
        };
        let (b, _) = batch(&[turn(0, "just chatting about parsers"), turn(1, &key)]);

        let (clean, skipped) = drop_secret_rows(vec![b]).unwrap();
        assert_eq!(skipped.rows, 1, "the secret block should be held back");
        assert!(skipped.summary.contains("PrivateKey"), "summary: {}", skipped.summary);
        assert_eq!(
            clean.iter().map(|b| b.num_rows()).sum::<usize>(),
            1,
            "the clean row stays"
        );
        assert!(!skipped.warning().is_empty());
    }

    #[test]
    fn drop_secret_rows_holds_back_an_escaped_key() {
        // The exact shape that leaked: a key stored with escaped `\n` (literal backslash-n), as a
        // JSON-encoded transcript or a logged blob would hold it. trufflehog still detects it, but
        // its canonical `raw` (real newlines) is not a substring of the stored bytes — value
        // matching missed it and pushed it. Line-based location must hold it back.
        if scan::Trufflehog::find().is_err() {
            eprintln!("skip: trufflehog not found");
            return;
        }
        let Some(key) = keygen(&["-t", "ed25519"]) else {
            eprintln!("skip: ssh-keygen unavailable");
            return;
        };
        let escaped = key.replace('\n', "\\n"); // literal backslash-n, no real newline
        assert!(!escaped.contains('\n'), "precondition: no real newlines remain");
        let (b, _) = batch(&[turn(0, "clean note"), turn(1, &format!("deploy key: {escaped}"))]);

        let (clean, skipped) = drop_secret_rows(vec![b]).unwrap();
        assert_eq!(skipped.rows, 1, "the escaped key must be held back, not pushed");
        assert!(skipped.summary.contains("PrivateKey"), "summary: {}", skipped.summary);
        assert_eq!(
            clean.iter().map(|b| b.num_rows()).sum::<usize>(),
            1,
            "the clean row stays"
        );
    }

    #[test]
    fn drop_secret_rows_holds_back_a_secret_split_across_chunks() {
        // The regression that leaked: a key long enough to split across several chunks. No single
        // chunk is a detectable key, so per-chunk scanning misses it — block reconstruction does not.
        if scan::Trufflehog::find().is_err() {
            eprintln!("skip: trufflehog not found");
            return;
        }
        let Some(key) = keygen(&["-t", "rsa", "-b", "4096"]) else {
            eprintln!("skip: ssh-keygen unavailable");
            return;
        };
        let block_text = format!("Here is the deploy key we generated:\n{key}\nkeep it safe");
        let (b, chunks) = batch(&[turn(0, "clean preamble"), turn(1, &block_text)]);
        let key_chunks = chunks.iter().filter(|c| c.seq == 1).count();
        assert!(
            key_chunks > 1,
            "precondition: the key must split into multiple chunks (got {key_chunks})"
        );

        let (clean, skipped) = drop_secret_rows(vec![b]).unwrap();
        assert_eq!(
            skipped.rows, key_chunks,
            "every chunk of the secret block must be held back"
        );
        assert!(skipped.summary.contains("PrivateKey"), "summary: {}", skipped.summary);
        assert_eq!(
            clean.iter().map(|b| b.num_rows()).sum::<usize>(),
            1,
            "only the clean row stays"
        );
    }

    /// A two-session local dataset for the selection tests.
    async fn two_session_ds(dir: &std::path::Path) -> Dataset {
        let (b, _) = batch(&[
            turn_sess("reviewed", 0, "a decision we can share"),
            turn_sess("private", 1, "an internal discussion"),
        ]);
        let uri = dataset::table_uri(&dir.to_string_lossy());
        let schema = b.schema();
        let reader = RecordBatchIterator::new(vec![Ok(b)], schema);
        Dataset::write(reader, &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        dataset::open(&uri, HashMap::new()).await.unwrap()
    }

    #[tokio::test]
    async fn ids_by_session_groups_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let ds = two_session_ds(dir.path()).await;
        let by_session = ids_by_session(&ds).await.unwrap();
        assert_eq!(by_session.len(), 2, "one entry per session");
        assert!(by_session.contains_key("reviewed") && by_session.contains_key("private"));
        assert!(by_session.values().all(|ids| !ids.is_empty()));
    }

    #[tokio::test]
    async fn push_coverage_requires_every_current_chunk_of_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let ds = two_session_ds(dir.path()).await;
        let by_session = ids_by_session(&ds).await.unwrap();
        let pushed: HashSet<String> = by_session["reviewed"].iter().cloned().collect();
        let batches = dataset::scan_rows(&ds, &["id", "session_id"], None, None)
            .await
            .unwrap();
        let coverage = push_coverage(&batches, &pushed).unwrap();
        assert_eq!(coverage.total, 2);
        assert_eq!(coverage.pending, 1);
    }

    #[tokio::test]
    async fn held_among_reports_the_pending_rows_a_push_would_hold() {
        if scan::Trufflehog::find().is_err() {
            eprintln!("skip: trufflehog not found");
            return;
        }
        let Some(key) = keygen(&["-t", "ed25519"]) else {
            eprintln!("skip: ssh-keygen unavailable");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (b, _) = batch(&[turn_sess("clean", 0, "notes on parsers"), turn_sess("dirty", 1, &key)]);
        let uri = dataset::table_uri(&dir.path().to_string_lossy());
        let schema = b.schema();
        let reader = RecordBatchIterator::new(vec![Ok(b)], schema);
        Dataset::write(reader, &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        let ds = dataset::open(&uri, HashMap::new()).await.unwrap();
        let by_session = ids_by_session(&ds).await.unwrap();

        let all: HashSet<String> = by_session.values().flatten().cloned().collect();
        let held = held_among(&ds, &all)
            .await
            .expect("the dirty pending block should be reported");
        assert!(held.rows >= 1);
        assert!(held.summary.contains("PrivateKey"), "summary: {}", held.summary);

        let clean_only: HashSet<String> = by_session["clean"].iter().cloned().collect();
        assert!(
            held_among(&ds, &clean_only).await.is_none(),
            "clean pending rows are not held"
        );
        assert!(
            held_among(&ds, &HashSet::new()).await.is_none(),
            "nothing pending, nothing scanned"
        );
    }

    #[test]
    fn push_receipt_is_an_atomic_growing_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt");
        let first = ["b".to_string(), "a".to_string()];
        record_pushed_at(&path, &first).unwrap();
        let second = ["b".to_string(), "c".to_string()];
        record_pushed_at(&path, &second).unwrap();
        assert_eq!(
            load_pushed_from(&path).unwrap(),
            ["a", "b", "c"].into_iter().map(str::to_string).collect()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "a\nb\nc\n");
    }

    #[test]
    fn concurrent_push_receipt_updates_do_not_lose_ids() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("receipt"));
        let writers = 16;
        let barrier = Arc::new(Barrier::new(writers));
        let threads: Vec<_> = (0..writers)
            .map(|i| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let id = format!("id-{i}");
                    barrier.wait();
                    record_pushed_at(&path, [&id]).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let expected: HashSet<_> = (0..writers).map(|i| format!("id-{i}")).collect();
        assert_eq!(load_pushed_from(&path).unwrap(), expected);
    }

    #[tokio::test]
    async fn an_append_reads_only_the_included_delta() {
        // The append path reads exactly the ids in `to_push` — never the sibling session's rows.
        let dir = tempfile::tempdir().unwrap();
        let ds = two_session_ds(dir.path()).await;
        let reviewed = ids_by_session(&ds).await.unwrap()["reviewed"].clone();
        let to_push: HashSet<String> = reviewed.iter().cloned().collect();
        let rows = rows_with_ids(&ds, &to_push).await.unwrap();
        let pushed: usize = rows.iter().map(|b| b.num_rows()).sum();
        assert_eq!(pushed, reviewed.len(), "only that session's rows are read");
    }

    #[test]
    fn a_push_locks_its_own_file_per_remote() {
        let uri = "hf://datasets/acme/kb";
        let receipt = pushed_path(uri);
        let receipt_lock = receipt.parent().unwrap().join(".locks").join(sanitize(uri));
        assert_ne!(push_lock_path(uri), receipt);
        assert_ne!(push_lock_path(uri), receipt_lock);
        assert_ne!(push_lock_path(uri), push_lock_path("hf://datasets/acme/other"));
    }

    #[tokio::test]
    async fn named_sessions_are_the_selection() {
        // Naming a session publishes exactly its rows — the sibling stays local.
        let dir = tempfile::tempdir().unwrap();
        let ds = two_session_ds(dir.path()).await;
        let by_session = ids_by_session(&ds).await.unwrap();

        let selected = named_ids(&by_session, &["reviewed".to_string()]).unwrap();
        let reviewed: HashSet<String> = by_session["reviewed"].iter().cloned().collect();
        assert_eq!(selected, reviewed, "exactly the named session's chunks");
        assert!(
            by_session["private"].iter().all(|id| !selected.contains(id)),
            "the session that wasn't named stays local"
        );

        // Both named: the union, order-independent.
        let both = named_ids(&by_session, &["private".to_string(), "reviewed".to_string()]).unwrap();
        assert_eq!(both.len(), by_session["reviewed"].len() + by_session["private"].len());
    }

    #[tokio::test]
    async fn a_mistyped_session_fails_the_push() {
        // Silently publishing the subset that did resolve would be the worst outcome here: the
        // caller would read success and never learn what stayed behind.
        let dir = tempfile::tempdir().unwrap();
        let ds = two_session_ds(dir.path()).await;
        let by_session = ids_by_session(&ds).await.unwrap();
        let err = named_ids(&by_session, &["reviewed".to_string(), "reviewd".to_string()])
            .expect_err("an unknown session id must be an error");
        let msg = format!("{err}");
        assert!(msg.contains("reviewd"), "names the unknown id: {msg}");
        assert!(!msg.contains("reviewed,"), "doesn't blame the id that resolved: {msg}");
    }
}
