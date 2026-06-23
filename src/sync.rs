//! `sync`: publish the local store's not-yet-remote chunks into a remote store on the HF Hub.
//!
//! Streamed, never a full mirror. "What's already there" is the store's own chunk ids, so the
//! delta is `local_ids − remote_ids` — the same primitive `index` uses.
//!
//! - **First publish:** build the dataset locally (data + FTS/IVF indexes) and upload every file.
//! - **Append:** run Lance's *native* append against the remote dataset over `hf://`, through a
//!   write-capturing object store ([`crate::capture`]). The append writes only data — a new
//!   fragment, a manifest, a transaction — captured and shipped as one `create_commit` guarded by
//!   the current branch head. The new rows are left unindexed (a query still finds them by brute
//!   force).
//! - **Reindex:** folding those rows into the index is a *separate* commit, kept off the data
//!   commit so the data commit stays small and cheap to retry. `sync` runs it after the data
//!   commit when the unindexed backlog crosses [`REINDEX_THRESHOLD`] (best-effort: a head-moved
//!   conflict is a warning, not a failure — the next sync retries), or eagerly with
//!   `--force-reindex` (retried until it lands).
//!
//! Why one `create_commit` rather than letting Lance write straight to `hf://`: the hf object
//! store *can* write (OpenDAL, token-gated), but one Hub commit per file — non-atomic, no CAS. A
//! single `create_commit(parent_commit=…)` gives atomicity and a fail-loud guard: if the head
//! moved, the commit fails and we say so (re-run sync). Capturing Lance's own writes also frees
//! `sync` from knowing the on-disk layout — it ships whatever Lance wrote, by Lance's own paths.

use crate::capture;
use crate::db;
use crate::hub::{self, Store};
use crate::scan;
use anyhow::{bail, Context, Result};
use arrow_array::{RecordBatch, StringArray};
use bytes::Bytes;
use futures::TryStreamExt;
use hf_hub::repository::{CommitInfo, CommitOperation};
use hf_hub::{HFClient, HFError, HFRepository, RepoTypeDataset};
use lancedb::index::scalar::FtsIndexBuilder;
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::index::Index;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::Table;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Reindex the remote once this many appended rows are sitting unindexed (answered by a
/// brute-force scan until folded in). Bounds per-query cost, not sync count, and is stateless —
/// [`capture::capture_append`] reads it straight from Lance's index stats.
const REINDEX_THRESHOLD: u64 = 2_000;

/// Cap on `--force-reindex` retries when the branch head keeps moving under it, so a busy remote
/// can't spin forever.
const REINDEX_MAX_RETRIES: u32 = 10;

/// Parse `hf://datasets/<owner>/<name>/<prefix…>` into (owner, name, prefix-within-repo).
fn parse_hf(uri: &str) -> Result<(String, String, String)> {
    let rest = uri.strip_prefix("hf://").context("remote store must be an hf:// URI")?;
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        ["datasets", owner, name, prefix @ ..] if !prefix.is_empty() => {
            Ok((owner.to_string(), name.to_string(), prefix.join("/")))
        }
        _ => bail!("expected hf://datasets/<owner>/<name>/<path>, got {uri}"),
    }
}

/// Every chunk id in a store (a plain `id`-column scan; plain scans aren't limit-capped).
async fn all_ids(table: &Table) -> Result<HashSet<String>> {
    let mut stream = table.query().select(Select::columns(&["id"])).execute().await?;
    let mut ids = HashSet::new();
    while let Some(batch) = stream.try_next().await? {
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

/// The to-push rows from the local store. First publish reads everything; an append reads just
/// the missing ids via an `id IN (…)` predicate.
async fn rows_to_push(local: &Table, to_push: &HashSet<String>, first_publish: bool) -> Result<Vec<RecordBatch>> {
    let mut q = local.query();
    if !first_publish {
        let list = to_push
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(", ");
        q = q.only_if(format!("id IN ({list})"));
    }
    let mut stream = q.execute().await?;
    let mut batches = Vec::new();
    while let Some(batch) = stream.try_next().await? {
        batches.push(batch);
    }
    Ok(batches)
}

/// Chunk `text` values across the batches, for the pre-publish secret scan.
fn texts(batches: &[RecordBatch]) -> Vec<String> {
    let mut out = Vec::new();
    for b in batches {
        if let Some(col) = b
            .column_by_name("text")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        {
            for i in 0..b.num_rows() {
                out.push(col.value(i).to_string());
            }
        }
    }
    out
}

/// Publish the local store's new chunks to `target` (a remote store on the HF Hub). With
/// `force_reindex`, refresh the remote index after the data commit (retrying until it lands) even
/// if the unindexed backlog is below [`REINDEX_THRESHOLD`]; with no new chunks pending it's a pure
/// index refresh.
pub async fn run_sync(target: Store, force_reindex: bool) -> Result<String> {
    let (uri, revision) = match &target {
        Store::Remote { uri, revision } => (uri.clone(), revision.clone()),
        Store::Local { .. } => {
            bail!(
                "sync needs a remote `hf://` target; the configured store is local (pass --store or set $FUNES_STORE)"
            )
        }
    };

    // 1. Delta: local_ids − remote_ids (remote absent => first publish).
    let local = Store::local().open().await?;
    let local_ids = all_ids(&local).await?;
    let remote_ids = match target.open().await {
        Ok(t) => all_ids(&t).await?,
        Err(_) => HashSet::new(),
    };
    let to_push: HashSet<String> = local_ids.difference(&remote_ids).cloned().collect();
    let first_publish = remote_ids.is_empty();

    // Nothing to push => done (no token needed), unless this is a forced reindex of an existing
    // remote, which is still work.
    if to_push.is_empty() && (first_publish || !force_reindex) {
        return Ok(format!(
            "{}: already up to date ({} chunks)\n",
            target.label(),
            remote_ids.len()
        ));
    }

    // 2. HF repo handle (every write path below needs it).
    let (owner, name, prefix) = parse_hf(&uri)?;
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to push")?;
    let client = HFClient::builder()
        .token(token.clone())
        .build()
        .context("building hf-hub client")?;
    let repo = client.dataset(owner, name);
    let rev = revision.unwrap_or_else(|| "main".to_string());
    let dataset_uri = format!("{uri}/{}.lance", db::TABLE);
    let opts = HashMap::from([("hf_token".to_string(), token), ("revision".to_string(), rev.clone())]);

    // 3. Forced reindex with no new data: just refresh the remote index and stop.
    if to_push.is_empty() {
        let head = head_oid(&repo, &rev).await?;
        let note = reindex_forced(&repo, &dataset_uri, &opts, &rev, Some(head)).await?;
        return Ok(format!(
            "{}: up to date ({} chunks)\n{note}",
            target.label(),
            remote_ids.len()
        ));
    }

    // 4. Rows + pre-publish secret gate. Re-stamp each batch with the local table's schema so its
    // metadata (the embedding-model id) rides along — query-result batches drop it, and on first
    // publish that schema is what create_table persists.
    let schema = local.schema().await?;
    let batches: Vec<RecordBatch> = rows_to_push(&local, &to_push, first_publish)
        .await?
        .into_iter()
        .map(|b| RecordBatch::try_new(schema.clone(), b.columns().to_vec()))
        .collect::<std::result::Result<_, _>>()?;
    scan::ensure_no_secrets(&texts(&batches))?;
    let n_chunks = to_push.len();

    // 5. First publish: build the whole dataset locally (data + indexes) and push it in one commit.
    if first_publish {
        let staging = tempfile::tempdir()?;
        let db_dir = staging.path().join(&prefix);
        std::fs::create_dir_all(&db_dir)?;
        let conn = lancedb::connect(&db_dir.to_string_lossy()).execute().await?;
        let t = conn.create_table(db::TABLE, batches).execute().await?;
        let _ = t
            .create_index(&["text"], Index::FTS(FtsIndexBuilder::default()))
            .execute()
            .await;
        let _ = t
            .create_index(&["vector"], Index::IvfPq(IvfPqIndexBuilder::default()))
            .execute()
            .await;

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
            return Ok(format!("{}: nothing new to upload\n", target.label()));
        }
        let info = send_commit(&repo, ops, None, &rev, format!("funes sync: +{n_chunks} chunks"))
            .await
            .map_err(|e| anyhow::anyhow!("create_commit failed: {e}"))?;
        return Ok(format!(
            "{}: pushed {n_chunks} chunks (commit {})\n",
            target.label(),
            info.commit_oid.as_deref().unwrap_or("?")
        ));
    }

    // 6. Append: capture Lance's native data append over hf:// (data only — no index), guarded by
    // the current head so a concurrent push trips the parent-commit CAS.
    let parent = head_oid(&repo, &rev).await?;
    let (data_files, unindexed) = capture::capture_append(&dataset_uri, opts.clone(), batches, schema).await?;
    if data_files.is_empty() {
        return Ok(format!("{}: nothing new to upload\n", target.label()));
    }
    let (ops, _dir) = write_ops(&data_files)?;
    let info = send_commit(
        &repo,
        ops,
        Some(parent),
        &rev,
        format!("funes sync: +{n_chunks} chunks"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("data commit failed (if the remote head moved, re-run sync): {e}"))?;
    let mut out = format!(
        "{}: pushed {n_chunks} chunks (commit {})\n",
        target.label(),
        info.commit_oid.as_deref().unwrap_or("?")
    );

    // 7. Reindex as a *separate* commit, so the cheap data commit above stays retry-friendly:
    // forced (retried until it lands) or, past the threshold, best-effort (one shot, warn on a
    // conflict — the next sync retries).
    let after_data = info.commit_oid.clone();
    if force_reindex {
        out.push_str(&reindex_forced(&repo, &dataset_uri, &opts, &rev, after_data).await?);
    } else if unindexed > REINDEX_THRESHOLD {
        out.push_str(&reindex_auto(&repo, &dataset_uri, &opts, &rev, after_data).await);
    }
    Ok(out)
}

/// Read the commit at the tip of branch `rev` (the parent-commit guard for the next commit).
async fn head_oid(repo: &HFRepository<RepoTypeDataset>, rev: &str) -> Result<String> {
    let refs = repo.list_refs().send().await.context("listing remote refs")?;
    refs.branches
        .iter()
        .find(|b| b.name == rev)
        .map(|b| b.target_commit.clone())
        .context("target branch not found on the remote")
}

/// Write captured Lance files (path → bytes) to a scratch dir and turn them into add-file commit
/// operations — hf-hub uploads from local paths. The returned `TempDir` must outlive the commit.
fn write_ops(files: &BTreeMap<String, Bytes>) -> Result<(Vec<CommitOperation>, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let mut ops = Vec::with_capacity(files.len());
    for (i, (repo_path, body)) in files.iter().enumerate() {
        let local = dir.path().join(format!("f{i}"));
        std::fs::write(&local, body)?;
        ops.push(CommitOperation::add_file(repo_path.clone(), local));
    }
    Ok((ops, dir))
}

/// One `create_commit` of `ops` on branch `rev`. `parent` guards the head; `None` skips the guard
/// (first publish). Returns the raw hf-hub result so callers can tell a head-moved
/// [`HFError::Conflict`] from other failures.
async fn send_commit(
    repo: &HFRepository<RepoTypeDataset>,
    ops: Vec<CommitOperation>,
    parent: Option<String>,
    rev: &str,
    message: String,
) -> std::result::Result<CommitInfo, HFError> {
    match parent {
        Some(p) => {
            repo.create_commit()
                .operations(ops)
                .commit_message(message)
                .parent_commit(p)
                .revision(rev.to_string())
                .send()
                .await
        }
        None => {
            repo.create_commit()
                .operations(ops)
                .commit_message(message)
                .revision(rev.to_string())
                .send()
                .await
        }
    }
}

/// Forced reindex: optimize the remote index and commit it, retrying on a head-moved conflict
/// (re-reading the head each time) until it lands or [`REINDEX_MAX_RETRIES`] is exceeded. The data
/// is already committed, so this only ever redoes the cheap index step.
async fn reindex_forced(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    opts: &HashMap<String, String>,
    rev: &str,
    mut parent: Option<String>,
) -> Result<String> {
    let mut attempts = 0u32;
    loop {
        let files = capture::capture_reindex(dataset_uri, opts.clone()).await?;
        if files.is_empty() {
            return Ok("  index already current\n".to_string());
        }
        let (ops, _dir) = write_ops(&files)?;
        match send_commit(repo, ops, parent.clone(), rev, "funes sync: reindex".to_string()).await {
            Ok(info) => {
                return Ok(format!(
                    "  reindexed (commit {})\n",
                    info.commit_oid.as_deref().unwrap_or("?")
                ))
            }
            Err(HFError::Conflict { .. }) => {
                attempts += 1;
                if attempts > REINDEX_MAX_RETRIES {
                    bail!("reindex still conflicting after {REINDEX_MAX_RETRIES} retries; re-run sync --force-reindex");
                }
                parent = Some(head_oid(repo, rev).await?);
            }
            Err(e) => return Err(anyhow::anyhow!("reindex commit failed: {e}")),
        }
    }
}

/// Best-effort reindex during a normal sync: one attempt, never retried. The data is already
/// safely committed, so any failure here is a warning — the next sync past the threshold tries
/// again.
async fn reindex_auto(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    opts: &HashMap<String, String>,
    rev: &str,
    parent: Option<String>,
) -> String {
    let files = match capture::capture_reindex(dataset_uri, opts.clone()).await {
        Ok(f) if f.is_empty() => return String::new(),
        Ok(f) => f,
        Err(e) => return format!("  note: index not refreshed ({e}); will retry on a later sync\n"),
    };
    let (ops, _dir) = match write_ops(&files) {
        Ok(x) => x,
        Err(e) => return format!("  note: index not refreshed ({e})\n"),
    };
    match send_commit(repo, ops, parent, rev, "funes sync: reindex".to_string()).await {
        Ok(info) => format!("  reindexed (commit {})\n", info.commit_oid.as_deref().unwrap_or("?")),
        Err(e) => format!("  note: index not refreshed ({e}); will retry on a later sync\n"),
    }
}
