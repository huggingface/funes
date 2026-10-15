//! `push`: publish the local memory's not-yet-remote chunks into a remote memory on the HF Hub.
//!
//! Streamed, never a full mirror. "What's already there" is the memory's own chunk ids, so the
//! delta is `embedded_local_ids − remote_ids`. `push` is orchestration:
//! it computes the delta, holds back any row that still contains a secret (redaction happens at
//! index time; this is the egress backstop — the rows wait for `funes scrub`), and drives the HF
//! write operations in [`crate::memory::remote`], which own the atomic, parent-commit-guarded commits.
//!
//! - **First publish:** build the dataset locally (data + FTS/IVF indexes) and upload every file in
//!   one commit.
//! - **Append:** [`remote::append`] lands the new fragment + manifest + transaction in one
//!   guarded `create_commit`, retried against a fresh head if a concurrent push moved it. The new
//!   rows are left unindexed (a query still finds them by brute force).
//! - **Compaction:** separate commits ([`remote::compact`]), kept off the data commit so the
//!   data commit stays small. `push` runs it after the data commit when the unindexed backlog
//!   crosses [`COMPACT_THRESHOLD`] or the remote has no text index (best-effort: a head-moved
//!   conflict is a warning, the next push retries), or eagerly with `--compact` (retried until it
//!   lands).
//!
//! What a push ships is the embedded chunks the remote doesn't hold, optionally restricted to the
//! sessions named with `--sessions`. Rows awaiting embedding stay local until a later push.

use super::compact;
use crate::hub;
use crate::memory::card::{self, CardAction, CardCtx};
use crate::memory::dataset::{self, RecordBatch};
use crate::memory::lock;
use crate::memory::receipt;
use crate::memory::remote::{self, Appended, Compacted};
use crate::memory::{Memory, MemoryState};
use crate::ui::prompt::confirm;
use crate::{chunk, scan, ui};
use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use chrono::Utc;
use lance::Dataset;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;

/// Compact the remote once this many appended rows are sitting unindexed (answered by a
/// brute-force scan until folded in). Bounds per-query cost, not push count, and is stateless —
/// [`remote::append`] reads it straight from Lance's index stats. Nonzero so tiny per-push
/// deltas don't pile up between compactions.
const COMPACT_THRESHOLD: u64 = 500;

/// This host's session-level push coverage for one remote. A session is fully pushed only when
/// every chunk it currently has is in the local receipt, so a session that later grows returns to
/// `pending` until its new chunks are pushed.
pub(crate) struct PushCoverage {
    pub total: usize,
    pub pending: usize,
    pub held: Option<Skipped>,
}

fn push_coverage(by_session: &HashMap<String, Vec<String>>, pushed_ids: &HashSet<String>) -> PushCoverage {
    let complete = by_session
        .values()
        .filter(|ids| ids.iter().all(|id| pushed_ids.contains(id)))
        .count();
    PushCoverage {
        total: by_session.len(),
        pending: by_session.len() - complete,
        held: None,
    }
}

pub(crate) async fn local_push_coverage(local: &Dataset, memory_uri: &str) -> Option<PushCoverage> {
    let pushed_ids = receipt::load(memory_uri)?;
    let by_session = dataset::ids_by_session(local).await.ok()?;
    let mut coverage = push_coverage(&by_session, &pushed_ids);
    if coverage.pending > 0 {
        let pending: HashSet<String> = by_session
            .values()
            .flatten()
            .filter(|id| !pushed_ids.contains(*id))
            .cloned()
            .collect();
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

/// Every embedded row of the local memory, whole — a publish ships each row as it stands.
async fn all_rows(local: &Dataset) -> Result<Vec<RecordBatch>> {
    dataset::scan_rows(local, &[], Some(dataset::EMBEDDED), None).await
}

/// The ids of the local chunks that carry a vector — the only ones a push may publish.
async fn embedded_ids(local: &Dataset) -> Result<HashSet<String>> {
    dataset::ids(local, Some(dataset::EMBEDDED)).await
}

/// Don't make this a scan filter. On a memory that hasn't been pushed in a while, that filter lists
/// every pending id. Lance copies the whole filter into every fragment before it reads a row. RAM
/// use climbs with both the size of the backlog and the number of fragments.
async fn rows_with_ids(local: &Dataset, ids: &HashSet<String>) -> Result<Vec<RecordBatch>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut selected = Vec::new();
    for batch in dataset::scan_rows_with_ids(local, &["id"], None).await? {
        let (chunk_ids, row_ids) = (dataset::strs(&batch, "id"), dataset::row_ids(&batch)?);
        let matching: Vec<u64> = (0..batch.num_rows())
            .filter(|&i| ids.contains(chunk_ids.get(i)))
            .map(|i| row_ids.get(i))
            .collect();
        if !matching.is_empty() {
            selected.push(dataset::take_rows(local, &matching, &[]).await?);
        }
    }
    Ok(selected)
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

/// Why a target holds none of the chunks a push would publish, as far as the comparison can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disjoint {
    /// `--sessions` named sessions the target lacks entirely.
    Selection { sessions: usize },
    /// The target has no rows yet.
    FirstPublish,
    /// The target has rows, none from this host.
    NewHost { remote_chunks: usize },
    /// The target has rows and held chunks from this host before, yet shares nothing with it now.
    Unexplained { remote_chunks: usize },
}

/// How a push handles a target that holds none of the chunks it would publish.
pub enum Confirm {
    /// Proceed without asking (`--yes`, or a caller that has already established intent, e.g. tests).
    Yes,
    /// Ask before publishing. Called with the target label, the chunk count, and the [`Disjoint`] case.
    Ask(fn(&str, usize, Disjoint) -> bool),
}

impl Confirm {
    /// Whether the push may proceed to a share-nothing target.
    fn proceed(self, label: &str, chunks: usize, why: Disjoint) -> bool {
        match self {
            Confirm::Yes => true,
            Confirm::Ask(ask) => ask(label, chunks, why),
        }
    }
}

/// Whether a push must be confirmed first, and why: there are rows to publish and the remote holds
/// none of them.
fn disjoint(
    publishable: usize,
    to_push: usize,
    sessions: usize,
    remote_chunks: usize,
    known_host: bool,
) -> Option<Disjoint> {
    if to_push == 0 || to_push != publishable {
        return None;
    }
    Some(if sessions > 0 {
        Disjoint::Selection { sessions }
    } else if remote_chunks == 0 {
        Disjoint::FirstPublish
    } else if !known_host {
        Disjoint::NewHost { remote_chunks }
    } else {
        Disjoint::Unexplained { remote_chunks }
    })
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

/// `funes push`: publish to the memory `spec` names. Unless `yes`, a memory that holds none of the
/// chunks to publish is confirmed at a terminal first, and refused off one.
pub async fn run(spec: &str, compact: bool, yes: bool, sessions: &[String]) -> Result<Pushed> {
    let confirm = if yes {
        Confirm::Yes
    } else {
        Confirm::Ask(prompt_new_memory)
    };
    run_push(Memory::parse(spec), compact, confirm, sessions)
        .await
        .map_err(|e| {
            if hub::is_read_only(&e) {
                read_only_error(spec)
            } else {
                e
            }
        })
}

/// The error for a token the Hub lets read `label` but not write to.
pub(crate) fn read_only_error(label: &str) -> anyhow::Error {
    anyhow!("{label} is read-only for your token — recall can read it, but publishing needs write access (check your HF token)")
}

/// The push confirmation for a target holding none of the chunks to publish. Fails closed (returns
/// false) off a terminal, so an unattended push can't silently publish to the wrong memory — there it
/// must be re-run with `--yes`.
pub(crate) fn prompt_new_memory(label: &str, chunks: usize, why: Disjoint) -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!("{}", disjoint_refusal(label, chunks, why));
        return false;
    }
    confirm(&disjoint_prompt(label, chunks, why), false)
}

/// What the push found about the target and the chunks to publish, in the words of that case.
fn disjoint_finding(label: &str, chunks: usize, why: Disjoint) -> String {
    match why {
        Disjoint::Selection { sessions } => {
            format!("{label} holds none of the {chunks} chunk(s) of the {sessions} named session(s)")
        }
        Disjoint::FirstPublish => {
            format!("{label} is empty, so these {chunks} chunk(s) would be its first publish")
        }
        Disjoint::NewHost { remote_chunks } => {
            format!("{label} holds {remote_chunks} chunk(s), none from this host: a new host of yours")
        }
        Disjoint::Unexplained { remote_chunks } => format!(
            "{label} holds {remote_chunks} chunk(s) and none of the {chunks} to publish, although it held \
             chunks from this host before: check that it is the right memory"
        ),
    }
}

/// The question put to the user at a terminal: the finding, then a question that fits it.
fn disjoint_prompt(label: &str, chunks: usize, why: Disjoint) -> String {
    let question = match why {
        Disjoint::Selection { .. } => "Publish them?",
        Disjoint::FirstPublish => "Publish?",
        Disjoint::NewHost { .. } | Disjoint::Unexplained { .. } => "Publish anyway?",
    };
    format!("{}. {question} [y/N] ", disjoint_finding(label, chunks, why))
}

/// The refusal off a terminal, where no question can be asked.
fn disjoint_refusal(label: &str, chunks: usize, why: Disjoint) -> String {
    format!(
        "refusing to push: {}. Re-run with `--yes` to confirm.",
        disjoint_finding(label, chunks, why)
    )
}

/// Publish the local memory's new embedded chunks to `target` (a remote memory on the HF Hub). With
/// `compact`, compact the remote after the data commit (retrying until it goes through) even if
/// the unindexed backlog is below [`COMPACT_THRESHOLD`]. With no new chunks pending, it only
/// compacts, or gives a remote with no dataset yet the local memory's embedding model. `confirm`
/// gates a publish to a memory holding none of the chunks to publish.
///
/// `sessions`, when non-empty, restricts candidates to those sessions' embedded chunks.
/// Empty publishes all embedded chunks the local memory holds that the remote does not.
pub async fn run_push(target: Memory, compact: bool, confirm: Confirm, sessions: &[String]) -> Result<Pushed> {
    let uri = match &target {
        Memory::Remote { uri } => uri.clone(),
        Memory::Local { .. } => {
            bail!("push target must be a remote `hf://` memory — it publishes your local index up to the Hub")
        }
    };

    // A push's delta is only valid against the head it read, so a concurrent one republishes those rows.
    let Some(_push_lock) = lock::try_lock_file(&receipt::push_lock_path(&uri))? else {
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
        MemoryState::Offline => return Err(target.offline_error()),
        MemoryState::Missing => return Err(target.missing_error()),
        MemoryState::Unauthorized => return Err(target.unauthorized_error()),
        MemoryState::Empty => None,
        MemoryState::Ready(ds) => Some(ds),
    };

    // Only `Empty` can land here: a local path has no Hub states, and an unreadable memory is an
    // `Err`. A host whose first index isn't built yet has nothing to publish — that is not a failure.
    // A local memory with no rows still has a model, which a remote with no dataset takes, so the
    // next host to bind it embeds with the same.
    let local = match Memory::local().state().await? {
        MemoryState::Ready(ds) => Some(ds),
        MemoryState::Empty if remote.is_none() => Memory::local().dataset().await?,
        _ => None,
    };
    let Some(local) = local else {
        return Ok(format!(
            "{}: nothing indexed on this machine yet — nothing to publish\n",
            target.label()
        )
        .into());
    };
    if let Some(ds) = &remote {
        same_model(&local, ds, &target.label())?;
    }

    eprintln!("comparing local and remote indexes…");
    let remote_ids = match &remote {
        Some(ds) => dataset::ids(ds, None).await?,
        None => HashSet::new(),
    };
    let first_publish = remote.is_none();

    // 2. The local side. Named sessions are the selection outright: the caller has said what to
    // publish. Otherwise everything local is a candidate, and the remote's own ids decide what of
    // it is new.
    let candidates = if sessions.is_empty() {
        dataset::ids(&local, None).await?
    } else {
        eprintln!("publishing {} named session(s)", sessions.len());
        named_ids(&dataset::ids_by_session(&local).await?, sessions)?
    };
    let publishable: HashSet<String> = candidates.intersection(&embedded_ids(&local).await?).cloned().collect();
    let to_push: HashSet<String> = publishable.difference(&remote_ids).cloned().collect();

    // Read before the receipt write below, which can make this host look known.
    let known_host = receipt::held_before(&uri);

    // Bootstrap/refresh the local receipt from facts the push comparison has already established.
    // This makes a no-op push enough to initialize status for a legacy remote, with no extra scan.
    if remote.is_some() {
        receipt::record(&uri, candidates.intersection(&remote_ids))?;
    }

    // Nothing to push => done (no token needed), unless this is a first publish, which still gives
    // the remote its model, or a forced compaction of an existing remote, which is still work.
    if to_push.is_empty() && !first_publish && !compact {
        return Ok(format!("{}: already up to date ({} chunks)\n", target.label(), remote_ids.len()).into());
    }

    // 2. The write handle. Resolved before the confirmation, so a bad URI or a missing token fails
    // before we prompt for one.
    let w = remote::writable(&uri)?;

    // When required, ask for confirmation before publishing.
    if let Some(why) = disjoint(
        publishable.len(),
        to_push.len(),
        sessions.len(),
        remote_ids.len(),
        known_host,
    ) {
        if !confirm.proceed(&target.label(), to_push.len(), why) {
            bail!("push aborted");
        }
    }

    // 3. Forced compaction with no new data: just compact the remote and stop.
    if to_push.is_empty() && !first_publish {
        eprintln!("compacting the remote…");
        let note = compact::compact_remote(&w, "funes push: compact").await?;
        return Ok(format!("{}: up to date ({} chunks)\n{note}", target.label(), remote_ids.len()).into());
    }

    // 4. Rows, then hold back any that still contain a secret. Re-stamp each batch with the local
    // dataset's schema so its metadata (the embedding-model id) rides along — scan-result batches
    // drop it, and on first publish that schema is what the new dataset persists.
    let schema = dataset::schema_of(&local);
    // Only a first publish with nothing named ships the whole memory; anything else, `to_push`
    // is the answer — including a first publish of a selection.
    let rows = if first_publish && sessions.is_empty() {
        all_rows(&local).await?
    } else {
        rows_with_ids(&local, &to_push).await?
    };
    let batches = dataset::restamped(&schema, rows)?;

    // Drop any row whose text still holds a secret — hold it back from the Hub rather than block the
    // whole push. `funes scrub` redacts it in the local memory; the next push then ships it.
    let n_scanning: usize = batches.iter().map(|b| b.num_rows()).sum();
    eprintln!("scanning {n_scanning} chunk(s) for secrets…");
    let (batches, skipped) = drop_secret_rows(batches)?;
    let n_chunks: usize = batches.iter().map(|b| b.num_rows()).sum();
    if n_chunks == 0 && skipped.rows > 0 {
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
    let pushed_ids = dataset::ids_in(&batches);

    // The dataset card rides the data commit — created when the repo has none, refreshed when it
    // carries the funes markers, a hand-written card left alone (see `card_file`). Root stores
    // only: the root README describes the whole repo, which a prefixed memory is only part of
    // (and two stores sharing a repo would fight over one card's stats) — under a prefix it's
    // the owner's. The chunk count is the post-push total — best-effort: rows another writer
    // lands concurrently are missed until the next push refreshes the card.
    let model = dataset::embedding_model(&local)?;
    let date = Utc::now().format("%Y-%m-%d").to_string();
    let ctx = CardCtx {
        repo: &w.repo_id,
        chunks: remote_ids.len() as u64 + n_chunks as u64,
        embedding_model: model.id(),
        date: &date,
    };
    let (card_body, card_note) = if w.prefix.is_empty() {
        card_file(remote::fetch_readme(&w.repo, &w.rev).await, &ctx)
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
            &w.repo,
            &w.prefix,
            batches,
            schema.clone(),
            &w.rev,
            message,
            &extra,
            ui::index_progress,
        )
        .await?;
        let Some(oid) = oid else {
            return Ok(format!("{}: nothing new to upload\n", target.label()).into());
        };
        receipt::record(&uri, &pushed_ids)?;
        let pushed = match n_chunks {
            0 => format!("created with {}, no chunks yet", model.id()),
            n => format!("pushed {n} chunks"),
        };
        return Ok(format!(
            "{}: {pushed} (commit {oid})\n{card_note}{}",
            target.label(),
            skipped.warning()
        )
        .into());
    }

    // 6. Append the data and commit it, retrying against a fresh head if a concurrent push moved it
    // (each attempt re-appends onto the new manifest — the data commit is small, so this is cheap).
    eprintln!("uploading {n_chunks} chunk(s) to {}…", target.label());
    let mut attempts = 0u32;
    let (oid, unindexed, text_indexed) = loop {
        let attempt = remote::append(
            &w.repo,
            &w.dataset_uri,
            w.opts.clone(),
            &w.rev,
            message.clone(),
            batches.clone(),
            schema.clone(),
            &extra,
        )
        .await?;
        match attempt {
            Appended::Committed {
                oid,
                unindexed,
                text_indexed,
            } => break (oid, unindexed, text_indexed),
            Appended::Conflict => {
                attempts += 1;
                if attempts > remote::MAX_COMMIT_RETRIES {
                    bail!(
                        "data commit kept conflicting after {} retries; re-run push",
                        remote::MAX_COMMIT_RETRIES
                    );
                }
            }
        }
    };
    receipt::record(&uri, &pushed_ids)?;
    let mut out = format!("{}: pushed {n_chunks} chunks (commit {oid})\n", target.label());
    out.push_str(&card_note);

    // 7. Compact in separate commits: forced (retried until it lands), or best-effort (one shot,
    // warn on a conflict — the next push retries) past the threshold or when the remote has no text
    // index, which recall cannot do without.
    if compact {
        eprintln!("compacting the remote…");
        out.push_str(&compact::compact_remote(&w, "funes push: compact").await?);
    } else if unindexed >= COMPACT_THRESHOLD || !text_indexed {
        eprintln!("compacting the remote…");
        out.push_str(&compact_auto(&w).await);
    }
    out.push_str(&skipped.warning());
    Ok(out.into())
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
/// detectable), scanned in one pass, and the scanner says which block each finding came from — never
/// the secret's value, which fails on text stored with escaped or quoted bytes. If any
/// chunk of a block is dirty, the whole block is held back (its other chunks carry the rest of the
/// secret). Fail-closed on the scanner — a push must scan before it uploads.
fn drop_secret_rows(batches: Vec<RecordBatch>) -> Result<(Vec<RecordBatch>, Skipped)> {
    // Nothing to scan needs no scanner.
    if batches.iter().all(|b| b.num_rows() == 0) {
        return Ok((
            batches,
            Skipped {
                rows: 0,
                summary: String::new(),
            },
        ));
    }
    let scanner = scan::Trufflehog::find()?;
    // Row order across batches matches `chunks_from_batches`, so a chunk's index is its global row.
    let chunks = dataset::chunks(&batches);
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
        clean.push(dataset::keep_rows(b, |i| !dirty[base + i])?);
        base += b.num_rows();
    }
    let summary = scan::summary(detectors.iter().map(String::as_str));
    Ok((clean, Skipped { rows: dropped, summary }))
}

/// Best-effort compaction during a normal push: one attempt, never retried. The data is already
/// committed, so any failure here is a warning — the next push past the threshold tries again.
async fn compact_auto(w: &remote::Writable) -> String {
    match remote::compact(
        &w.repo,
        &w.dataset_uri,
        w.opts.clone(),
        &w.rev,
        "funes push: compact".to_string(),
        ui::index_progress,
    )
    .await
    {
        Ok(Compacted::Committed(oid)) => format!("  compacted (commit {oid})\n"),
        Ok(Compacted::Uncleaned(oid, e)) => format!(
            "  compacted (commit {oid})\n  note: old versions not deleted ({e:#}); \
             will retry on a later push\n"
        ),
        Ok(Compacted::AlreadyCompact) => String::new(),
        Ok(Compacted::Conflict) => {
            "  note: not compacted (remote head moved); will retry on a later push\n".to_string()
        }
        Err(e) => format!("  note: not compacted ({e:#}); will retry on a later push\n"),
    }
}

fn same_model(local: &Dataset, remote: &Dataset, label: &str) -> Result<()> {
    let (here, there) = (dataset::embedding_model(local)?, dataset::embedding_model(remote)?);
    if here != there {
        bail!(
            "{label} is embedded with {}, and the local memory with {}: a memory takes one embedding model",
            there.id(),
            here.id()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{RecordBatchIterator, StringArray, UInt64Array};
    use lance::dataset::WriteParams;
    use std::sync::Arc;

    #[test]
    fn the_push_prompt_says_which_case_it_found() {
        let label = "hf://datasets/acme/memory";
        assert_eq!(
            disjoint_prompt(label, 1339, Disjoint::Selection { sessions: 3 }),
            "hf://datasets/acme/memory holds none of the 1339 chunk(s) of the 3 named session(s). Publish them? [y/N] "
        );
        assert_eq!(
            disjoint_prompt(label, 40, Disjoint::FirstPublish),
            "hf://datasets/acme/memory is empty, so these 40 chunk(s) would be its first publish. Publish? [y/N] "
        );
        assert_eq!(
            disjoint_prompt(label, 40, Disjoint::NewHost { remote_chunks: 67339 }),
            "hf://datasets/acme/memory holds 67339 chunk(s), none from this host: a new host of yours. Publish anyway? [y/N] "
        );
        assert_eq!(
            disjoint_prompt(label, 40, Disjoint::Unexplained { remote_chunks: 67339 }),
            "hf://datasets/acme/memory holds 67339 chunk(s) and none of the 40 to publish, although it held chunks from this host before: check that it is the right memory. Publish anyway? [y/N] "
        );
        assert_eq!(
            disjoint_refusal(label, 1339, Disjoint::Selection { sessions: 3 }),
            "refusing to push: hf://datasets/acme/memory holds none of the 1339 chunk(s) of the 3 named session(s). Re-run with `--yes` to confirm."
        );
    }

    #[test]
    fn a_push_asks_only_when_the_overlap_is_empty_and_there_is_work() {
        // Some overlap (fewer to push than there are to publish): no prompt, it's a memory you add to.
        assert_eq!(disjoint(5, 3, 0, 100, true), None);
        // Nothing to push (up to date, or a compaction-only run): never a prompt, even with no overlap.
        assert_eq!(disjoint(5, 0, 0, 0, false), None);
        assert_eq!(disjoint(0, 0, 0, 0, false), None);
        // Every chunk to publish is new to the remote: a prompt, whatever the case.
        assert!(disjoint(5, 5, 0, 0, false).is_some());
        assert!(disjoint(1, 1, 0, 100, true).is_some());
    }

    #[test]
    fn disjoint_names_the_case_the_comparison_found() {
        // Named sessions the remote lacks: the selection is the reason, whatever the remote holds.
        assert_eq!(disjoint(5, 5, 3, 100, true), Some(Disjoint::Selection { sessions: 3 }));
        assert_eq!(disjoint(5, 5, 1, 0, false), Some(Disjoint::Selection { sessions: 1 }));
        // An empty remote: a first publish.
        assert_eq!(disjoint(5, 5, 0, 0, false), Some(Disjoint::FirstPublish));
        assert_eq!(disjoint(5, 5, 0, 0, true), Some(Disjoint::FirstPublish));
        // A remote with rows this host never compared against: a new host.
        assert_eq!(
            disjoint(5, 5, 0, 100, false),
            Some(Disjoint::NewHost { remote_chunks: 100 })
        );
        // A remote with rows that held chunks from this host before, sharing nothing: the one to check.
        assert_eq!(
            disjoint(5, 5, 0, 100, true),
            Some(Disjoint::Unexplained { remote_chunks: 100 })
        );
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

    use crate::traces::{Block, Turn, FORMAT_VERSION};
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
            format: FORMAT_VERSION,
            session_id: "sess".into(),
            cwd: None,
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
        (
            dataset::build_batch(dataset::model::ENGLISH, &chunks, Some(&vectors)).unwrap(),
            chunks,
        )
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
        let uri = dataset::table_uri(dir);
        let schema = b.schema();
        let reader = RecordBatchIterator::new(vec![Ok(b)], schema);
        Dataset::write(reader, &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        dataset::open(&uri, HashMap::new()).await.unwrap()
    }

    #[tokio::test]
    async fn a_memory_embedded_with_another_model_is_refused() {
        let (dir, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let bge = two_session_ds(dir.path()).await;
        let e5_model = crate::inference::EmbeddingModel::MultilingualE5Small;
        let chunks = chunk::chunks_from_turns(&[turn_sess("s", 0, "un passage")], &chunk::Tier::ALL, true);
        let b = dataset::build_batch(e5_model, &chunks, None).unwrap();
        let uri = dataset::table_uri(other.path());
        let e5 = Dataset::write(
            RecordBatchIterator::new(vec![Ok(b)], dataset::schema(e5_model)),
            &uri,
            None,
        )
        .await
        .unwrap();

        assert!(same_model(&bge, &bge, "acme/kb").is_ok());
        let err = same_model(&bge, &e5, "acme/kb").unwrap_err().to_string();
        assert!(
            err.contains(e5_model.id()) && err.contains(dataset::model::ENGLISH.id()),
            "{err}"
        );
    }

    #[tokio::test]
    async fn ids_by_session_groups_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let ds = two_session_ds(dir.path()).await;
        let by_session = dataset::ids_by_session(&ds).await.unwrap();
        assert_eq!(by_session.len(), 2, "one entry per session");
        assert!(by_session.contains_key("reviewed") && by_session.contains_key("private"));
        assert!(by_session.values().all(|ids| !ids.is_empty()));
    }

    #[tokio::test]
    async fn only_embedded_rows_can_publish() {
        let dir = tempfile::tempdir().unwrap();
        let chunks = chunk::chunks_from_turns(
            &[
                turn_sess("s", 0, "embedded"),
                turn_sess("s", 1, "still awaiting embedding"),
            ],
            &chunk::Tier::ALL,
            true,
        );
        let reader = RecordBatchIterator::new(
            vec![Ok(dataset::build_batch(dataset::model::ENGLISH, &chunks, None).unwrap())],
            dataset::schema(dataset::model::ENGLISH),
        );
        let uri = dataset::table_uri(dir.path());
        let ds = Dataset::write(reader, &uri, None).await.unwrap();
        let ds = dataset::fill_vectors(&ds, &[&chunks[0].id], &[vec![1.0; dataset::DIM as usize]])
            .await
            .unwrap();

        let embedded = HashSet::from([chunks[0].id.clone()]);
        assert_eq!(embedded_ids(&ds).await.unwrap(), embedded);
        assert_eq!(
            dataset::ids_in(&all_rows(&ds).await.unwrap()),
            embedded,
            "a first publish ships no nulls"
        );
    }

    #[tokio::test]
    async fn push_coverage_requires_every_current_chunk_of_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let ds = two_session_ds(dir.path()).await;
        let by_session = dataset::ids_by_session(&ds).await.unwrap();
        let pushed: HashSet<String> = by_session["reviewed"].iter().cloned().collect();
        let coverage = push_coverage(&by_session, &pushed);
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
        let uri = dataset::table_uri(dir.path());
        let schema = b.schema();
        let reader = RecordBatchIterator::new(vec![Ok(b)], schema);
        Dataset::write(reader, &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        let ds = dataset::open(&uri, HashMap::new()).await.unwrap();
        let by_session = dataset::ids_by_session(&ds).await.unwrap();

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

    #[tokio::test]
    async fn selected_rows_still_pass_through_the_secret_gate() {
        if scan::Trufflehog::find().is_err() {
            eprintln!("skip: trufflehog not found");
            return;
        }
        let Some(key) = keygen(&["-t", "rsa", "-b", "4096"]) else {
            eprintln!("skip: ssh-keygen unavailable");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (b, _) = batch(&[turn_sess("clean", 0, "notes on parsers"), turn_sess("dirty", 1, &key)]);
        assert!(b.num_rows() > 2, "the secret must span multiple chunks");
        let uri = dataset::table_uri(dir.path());
        let schema = b.schema();
        let reader = RecordBatchIterator::new(vec![Ok(b.slice(0, 1))], schema.clone());
        let mut ds = Dataset::write(reader, &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        // One row per fragment, so the key's chunks come back from different take calls and the
        // gate must still see the whole key.
        for i in 1..b.num_rows() {
            ds.append(RecordBatchIterator::new(vec![Ok(b.slice(i, 1))], schema.clone()), None)
                .await
                .unwrap();
        }
        let by_session = dataset::ids_by_session(&ds).await.unwrap();

        let all: HashSet<String> = by_session.values().flatten().cloned().collect();
        let rows = rows_with_ids(&ds, &all).await.unwrap();
        let (clean, held) = drop_secret_rows(rows).unwrap();
        assert_eq!(held.rows, by_session["dirty"].len(), "every secret split is held");
        assert!(held.summary.contains("PrivateKey"), "summary: {}", held.summary);

        let clean_only: HashSet<String> = by_session["clean"].iter().cloned().collect();
        assert_eq!(dataset::ids_in(&clean), clean_only, "unrelated clean rows survive");
        let rows = rows_with_ids(&ds, &clean_only).await.unwrap();
        assert_eq!(drop_secret_rows(rows).unwrap().1.rows, 0, "clean rows are not held");
    }

    #[tokio::test]
    async fn an_append_returns_only_the_included_delta() {
        let dir = tempfile::tempdir().unwrap();
        let ds = two_session_ds(dir.path()).await;
        let reviewed = dataset::ids_by_session(&ds).await.unwrap()["reviewed"].clone();
        let to_push: HashSet<String> = reviewed.iter().cloned().collect();
        let rows = rows_with_ids(&ds, &to_push).await.unwrap();
        let pushed: usize = rows.iter().map(|b| b.num_rows()).sum();
        assert_eq!(pushed, reviewed.len(), "only that session's rows are returned");
    }

    #[tokio::test]
    async fn large_selection_across_many_fragments_preserves_rows() {
        use arrow_schema::{DataType, Field, Schema};

        // Many fragments and many ids: the shape whose `id IN (…)` filter blew up, so the selection
        // must stay correct where it matters most.
        let dir = tempfile::tempdir().unwrap();
        let uri = dataset::table_uri(dir.path());
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("payload", DataType::UInt64, false),
        ]));
        let fragments = 256;
        let per_fragment = 512;
        let reader = |range: std::ops::Range<u64>| {
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(StringArray::from_iter_values(
                        range.clone().map(|n| format!("{n:016x}")),
                    )),
                    Arc::new(UInt64Array::from_iter_values(range)),
                ],
            )
            .unwrap();
            RecordBatchIterator::new(vec![Ok(batch)], schema.clone())
        };
        let mut ds = Dataset::write(reader(0..per_fragment), &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        for fragment in 1..fragments {
            let start = fragment * per_fragment;
            ds.append(reader(start..start + per_fragment), None).await.unwrap();
        }
        assert_eq!(ds.get_fragments().len(), fragments as usize);
        // Repeated IDs are valid input. Selection must preserve each stored occurrence.
        ds.append(reader(1..2), None).await.unwrap();
        let wanted: HashSet<_> = (0..fragments * per_fragment)
            .filter(|n| n % 3 != 0)
            .map(|n| format!("{n:016x}"))
            .collect();
        let rows = rows_with_ids(&ds, &wanted).await.unwrap();
        assert_eq!(rows.iter().map(|b| b.num_rows()).sum::<usize>(), wanted.len() + 1);
        assert_eq!(dataset::ids_in(&rows), wanted);
        for batch in rows {
            assert_eq!(batch.schema().fields(), schema.fields());
            let ids = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
            let values = batch.column(1).as_any().downcast_ref::<UInt64Array>().unwrap();
            for i in 0..batch.num_rows() {
                assert_eq!(ids.value(i), format!("{:016x}", values.value(i)));
                assert_ne!(values.value(i) % 3, 0);
            }
        }
        assert!(rows_with_ids(&ds, &HashSet::new()).await.unwrap().is_empty());
        let absent = HashSet::from(["not-present".to_string()]);
        assert!(rows_with_ids(&ds, &absent).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn named_sessions_are_the_selection() {
        // Naming a session publishes exactly its rows — the sibling stays local.
        let dir = tempfile::tempdir().unwrap();
        let ds = two_session_ds(dir.path()).await;
        let by_session = dataset::ids_by_session(&ds).await.unwrap();

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
        let by_session = dataset::ids_by_session(&ds).await.unwrap();
        let err = named_ids(&by_session, &["reviewed".to_string(), "reviewd".to_string()])
            .expect_err("an unknown session id must be an error");
        let msg = format!("{err}");
        assert!(msg.contains("reviewd"), "names the unknown id: {msg}");
        assert!(!msg.contains("reviewed,"), "doesn't blame the id that resolved: {msg}");
    }
}
