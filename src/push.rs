//! `push`: publish the local store's not-yet-remote chunks into a remote store on the HF Hub.
//!
//! Streamed, never a full mirror. "What's already there" is the store's own chunk ids, so the
//! delta is `local_ids − remote_ids` — the same primitive `index` uses. `push` is orchestration:
//! it computes the delta, holds back any row that still contains a secret (redaction happens at
//! index time; this is the egress backstop — the rows wait for `funes scrub`), and drives the HF
//! write operations in [`crate::hf_dataset`], which own the atomic, parent-commit-guarded commits.
//!
//! - **First publish:** build the dataset locally (data + FTS/IVF indexes) and upload every file in
//!   one commit.
//! - **Append:** [`hf_dataset::append`] lands the new fragment + manifest + transaction in one
//!   guarded `create_commit`, retried against a fresh head if a concurrent push moved it. The new
//!   rows are left unindexed (a query still finds them by brute force).
//! - **Reindex:** a *separate* guarded commit ([`hf_dataset::reindex`]), kept off the data commit so
//!   the data commit stays small. `push` runs it after the data commit when the unindexed backlog
//!   crosses [`REINDEX_THRESHOLD`] (best-effort: a head-moved conflict is a warning, the next push
//!   retries), or eagerly with `--force-reindex` (retried until it lands).

use crate::hf_dataset::{self, Appended, Reindexed};
use crate::hub::{self, Store};
use crate::{chunk, dataset, scan};
use anyhow::{bail, Context, Result};
use arrow_array::{BooleanArray, RecordBatch, RecordBatchIterator, StringArray};
use arrow_select::filter::filter_record_batch;
use hf_hub::repository::CommitOperation;
use hf_hub::{HFClient, HFError, HFRepository, RepoTypeDataset};
use lance::dataset::WriteParams;
use lance::Dataset;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Reindex the remote once this many appended rows are sitting unindexed (answered by a
/// brute-force scan until folded in). Bounds per-query cost, not push count, and is stateless —
/// [`hf_dataset::append`] reads it straight from Lance's index stats. Nonzero so tiny per-push
/// deltas don't pile up between compactions.
const REINDEX_THRESHOLD: u64 = 500;

/// Cap on CAS-conflict retries (the data append, and a forced reindex) when the branch head keeps
/// moving under us, so a busy remote can't spin forever.
const MAX_COMMIT_RETRIES: u32 = 10;

/// Every chunk id in a store, or empty if it can't be opened (absent local index, not-yet-created
/// or inaccessible remote).
pub async fn store_ids(store: &Store) -> HashSet<String> {
    match store.open().await {
        Ok(ds) => all_ids(&ds, None).await.unwrap_or_default(),
        Err(_) => HashSet::new(),
    }
}

/// Escape a value for inlining into a Lance SQL filter string.
fn esc(s: &str) -> String {
    s.replace('\'', "''")
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

/// Every chunk id in a store (a plain `id`-column scan; plain scans aren't limit-capped). With a
/// `filter` (e.g. `project = '…'`), only the ids matching it — the projection `--project` needs.
async fn all_ids(ds: &Dataset, filter: Option<&str>) -> Result<HashSet<String>> {
    let batches = dataset::scan_rows(ds, &["id"], filter, None).await?;
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

/// The to-push rows (all columns) from the local store. An append reads just the missing ids via an
/// `id IN (…)` predicate (already project-scoped, since `to_push` is). A first publish reads
/// everything — or, under `--project`, only that project's rows.
async fn rows_to_push(
    local: &Dataset,
    to_push: &HashSet<String>,
    first_publish: bool,
    project: Option<&str>,
) -> Result<Vec<RecordBatch>> {
    let filter = if first_publish {
        project.map(|p| format!("project = '{}'", esc(p)))
    } else {
        let list = to_push
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("id IN ({list})"))
    };
    dataset::scan_rows(local, &[], filter.as_deref(), None).await
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
/// no chunk with the remote — a first publish, a new host of yours, or the wrong store.
fn must_confirm(local: usize, to_push: usize) -> bool {
    to_push > 0 && to_push == local
}

/// Publish the local store's new chunks to `target` (a remote store on the HF Hub). With
/// `force_reindex`, refresh the remote index after the data commit (retrying until it lands) even
/// if the unindexed backlog is below [`REINDEX_THRESHOLD`]; with no new chunks pending it's a pure
/// index refresh. `confirm` gates a publish to a store the local index shares no chunks with.
pub async fn run_push(target: Store, project: Option<String>, force_reindex: bool, confirm: Confirm) -> Result<Pushed> {
    let uri = match &target {
        Store::Remote { uri } => uri.clone(),
        Store::Local { .. } => {
            bail!("push target must be a remote `hf://` store — it publishes your local index up to the Hub")
        }
    };

    // Fail fast, before any local build or scan: an unreachable or missing remote otherwise looks
    // like a first publish, so push scans + builds the whole dataset before the commit finally fails.
    match hub::remote_reachability(&uri).await {
        hub::Reachability::Offline => {
            bail!("{uri} is unreachable — can't push while offline (check your connection)")
        }
        hub::Reachability::Missing => return Err(hub::missing_remote(&uri)),
        hub::Reachability::Ok => {}
    }

    // 1. Delta: local_ids − remote_ids (remote absent => first publish). Under `--project`, the
    // local side is scoped to that project (a projection), so only its chunks are ever published;
    // the remote side stays unfiltered — a chunk id already on the remote is published, whatever
    // project it belongs to.
    let project_filter = project.as_deref().map(|p| format!("project = '{}'", esc(p)));
    if let Some(p) = &project {
        eprintln!("publishing only project {p}…");
    }
    eprintln!("comparing local and remote indexes…");
    let local = Store::local().open().await?;
    let local_ids = all_ids(&local, project_filter.as_deref()).await?;
    let remote_ids = match target.open().await {
        Ok(t) => all_ids(&t, None).await?,
        Err(_) => HashSet::new(),
    };
    let to_push: HashSet<String> = local_ids.difference(&remote_ids).cloned().collect();
    let first_publish = remote_ids.is_empty();

    // Nothing to push => done (no token needed), unless this is a forced reindex of an existing
    // remote, which is still work.
    if to_push.is_empty() && (first_publish || !force_reindex) {
        return Ok(format!("{}: already up to date ({} chunks)\n", target.label(), remote_ids.len()).into());
    }

    // 2. HF repo handle. Resolve the target and token before the confirmation, so a bad URI or a
    // missing token fails before we prompt for one.
    let (owner, name, prefix) = hub::parse_hf(&uri)?;
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to push")?;

    // When required, ask for confirmation before publishing.
    if must_confirm(local_ids.len(), to_push.len()) && !confirm.proceed(&target.label(), to_push.len()) {
        bail!("push aborted");
    }

    let client = HFClient::builder()
        .token(token.clone())
        .build()
        .context("building hf-hub client")?;
    let repo = client.dataset(owner, name);
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
    let batches: Vec<RecordBatch> = rows_to_push(&local, &to_push, first_publish, project.as_deref())
        .await?
        .into_iter()
        .map(|b| RecordBatch::try_new(schema.clone(), b.columns().to_vec()))
        .collect::<std::result::Result<_, _>>()?;

    // Drop any row whose text still holds a secret — hold it back from the Hub rather than block the
    // whole push. `funes scrub` redacts it in the local store; the next push then ships it.
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

    // 5. First publish: build the whole dataset locally (data + indexes) and push it in one commit.
    if first_publish {
        let staging = tempfile::tempdir()?;
        // Empty prefix = dataset at the repo root; joining "" would leave a stray trailing separator.
        let db_dir = if prefix.is_empty() {
            staging.path().to_path_buf()
        } else {
            staging.path().join(&prefix)
        };
        std::fs::create_dir_all(&db_dir)?;
        let table_uri = dataset::table_uri(&db_dir.to_string_lossy());
        eprintln!("building the dataset to publish…");
        let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema.clone());
        let mut ds = Dataset::write(reader, &table_uri, Some(WriteParams::default()))
            .await
            .context("building the dataset for first publish")?;
        dataset::build_indexes(&mut ds, |phase| eprintln!("building {phase}…")).await;

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
        if ops.is_empty() {
            return Ok(format!("{}: nothing new to upload\n", target.label()).into());
        }
        eprintln!("uploading {n_chunks} chunk(s) to {}…", target.label());
        let info = repo
            .create_commit()
            .operations(ops)
            .commit_message(format!("funes push: +{n_chunks} chunks"))
            .revision(rev.clone())
            .progress(hf_dataset::upload_progress())
            .send()
            .await
            .map_err(|e| anyhow::Error::new(e).context("create_commit failed"))?;
        return Ok(format!(
            "{}: pushed {n_chunks} chunks (commit {})\n{}",
            target.label(),
            info.commit_oid.as_deref().unwrap_or("?"),
            skipped.warning()
        )
        .into());
    }

    // 6. Append the data and commit it, retrying against a fresh head if a concurrent push moved it
    // (each attempt re-appends onto the new manifest — the data commit is small, so this is cheap).
    let message = format!("funes push: +{n_chunks} chunks");
    eprintln!("uploading {n_chunks} chunk(s) to {}…", target.label());
    let mut attempts = 0u32;
    let (oid, unindexed) = loop {
        let attempt = hf_dataset::append(
            &repo,
            &dataset_uri,
            opts.clone(),
            &rev,
            message.clone(),
            batches.clone(),
            schema.clone(),
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
    let mut out = format!("{}: pushed {n_chunks} chunks (commit {oid})\n", target.label());

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

/// Rows held back from a push because their text still contained a secret.
struct Skipped {
    rows: usize,
    /// Detectors that fired, e.g. `PrivateKey×1, AWS×2` — empty when nothing was held back.
    summary: String,
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

/// Forced reindex: ask [`hf_dataset::reindex`] to refresh and commit, retrying on a head-moved
/// conflict (it re-reads the head each call) until it lands or [`MAX_COMMIT_RETRIES`] is exceeded.
async fn reindex_forced(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    opts: &HashMap<String, String>,
    rev: &str,
) -> Result<String> {
    for _ in 0..=MAX_COMMIT_RETRIES {
        match hf_dataset::reindex(repo, dataset_uri, opts.clone(), rev, "funes push: reindex".to_string()).await? {
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
    match hf_dataset::reindex(repo, dataset_uri, opts.clone(), rev, "funes push: reindex".to_string()).await {
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
    use crate::index;

    #[test]
    fn must_confirm_only_when_overlap_is_empty_and_there_is_work() {
        // First publish / fully disjoint (every local chunk is new to the remote) → confirm.
        assert!(must_confirm(5, 5));
        assert!(must_confirm(1, 1));
        // Some overlap (fewer to push than the local total) → no prompt, it's a store you add to.
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

    use crate::trace::{Block, Turn};
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
        turn_in("proj", idx, block_text)
    }

    /// Like [`turn`], but in a named project — for exercising the `--project` projection.
    fn turn_in(project: &str, idx: i64, block_text: &str) -> Turn {
        Turn {
            session_id: "sess".into(),
            project: project.into(),
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

    /// Build a to-push batch the way the store stores it: chunk the turns, stamp zero vectors.
    fn batch(turns: &[Turn]) -> (RecordBatch, Vec<chunk::Chunk>) {
        let chunks = chunk::chunks_from_turns(turns, true);
        let vectors = vec![vec![0.0f32; index::DIM as usize]; chunks.len()];
        (index::build_batch(&chunks, &vectors).unwrap(), chunks)
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

    #[tokio::test]
    async fn project_projection_reads_only_that_project() {
        // A local store holding two projects; `--project` must publish only one, never its sibling.
        let (b, _) = batch(&[
            turn_in("alpha", 0, "alpha decision about parsers"),
            turn_in("beta", 1, "beta decision about chunking"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let uri = dataset::table_uri(&dir.path().to_string_lossy());
        let schema = b.schema();
        let reader = RecordBatchIterator::new(vec![Ok(b)], schema);
        Dataset::write(reader, &uri, Some(WriteParams::default()))
            .await
            .unwrap();
        let ds = dataset::open(&uri, HashMap::new()).await.unwrap();

        let all = all_ids(&ds, None).await.unwrap();
        let alpha = all_ids(&ds, Some("project = 'alpha'")).await.unwrap();
        assert!(
            !alpha.is_empty() && alpha.len() < all.len(),
            "alpha is a strict subset of the store"
        );

        // A first publish under `--project` reads exactly that project's rows — no sibling leak.
        let rows = rows_to_push(&ds, &HashSet::new(), true, Some("alpha")).await.unwrap();
        let pushed: usize = rows.iter().map(|b| b.num_rows()).sum();
        assert_eq!(
            pushed,
            alpha.len(),
            "first publish scoped to the project reads only its rows"
        );
    }
}
