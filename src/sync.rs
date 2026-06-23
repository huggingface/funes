//! `sync`: publish the local store's not-yet-remote chunks into a remote store on the HF Hub.
//!
//! Streamed, never a full mirror. "What's already there" is the store's own chunk ids, so the
//! delta is `local_ids − remote_ids` — the same primitive `index` uses. `sync` is orchestration:
//! it computes the delta, runs the pre-publish secret gate, and drives the HF write operations in
//! [`crate::hf_dataset`], which own the atomic, parent-commit-guarded commits.
//!
//! - **First publish:** build the dataset locally (data + FTS/IVF indexes) and upload every file in
//!   one commit.
//! - **Append:** [`hf_dataset::append`] lands the new fragment + manifest + transaction in one
//!   guarded `create_commit`, retried against a fresh head if a concurrent push moved it. The new
//!   rows are left unindexed (a query still finds them by brute force).
//! - **Reindex:** a *separate* guarded commit ([`hf_dataset::reindex`]), kept off the data commit so
//!   the data commit stays small. `sync` runs it after the data commit when the unindexed backlog
//!   crosses [`REINDEX_THRESHOLD`] (best-effort: a head-moved conflict is a warning, the next sync
//!   retries), or eagerly with `--force-reindex` (retried until it lands).

use crate::db;
use crate::hf_dataset::{self, Appended, Reindexed};
use crate::hub::{self, Store};
use crate::scan;
use anyhow::{bail, Context, Result};
use arrow_array::{RecordBatch, StringArray};
use futures::TryStreamExt;
use hf_hub::repository::CommitOperation;
use hf_hub::{HFClient, HFRepository, RepoTypeDataset};
use lancedb::index::scalar::FtsIndexBuilder;
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::index::Index;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::Table;
use std::collections::{HashMap, HashSet};

/// Reindex the remote once this many appended rows are sitting unindexed (answered by a
/// brute-force scan until folded in). Bounds per-query cost, not sync count, and is stateless —
/// [`hf_dataset::append`] reads it straight from Lance's index stats.
const REINDEX_THRESHOLD: u64 = 2_000;

/// Cap on CAS-conflict retries (the data append, and a forced reindex) when the branch head keeps
/// moving under us, so a busy remote can't spin forever.
const MAX_COMMIT_RETRIES: u32 = 10;

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
        let note = reindex_forced(&repo, &dataset_uri, &opts, &rev).await?;
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
        let info = repo
            .create_commit()
            .operations(ops)
            .commit_message(format!("funes sync: +{n_chunks} chunks"))
            .revision(rev.clone())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("create_commit failed: {e}"))?;
        return Ok(format!(
            "{}: pushed {n_chunks} chunks (commit {})\n",
            target.label(),
            info.commit_oid.as_deref().unwrap_or("?")
        ));
    }

    // 6. Append the data and commit it, retrying against a fresh head if a concurrent push moved it
    // (each attempt re-appends onto the new manifest — the data commit is small, so this is cheap).
    let message = format!("funes sync: +{n_chunks} chunks");
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
                    bail!("data commit kept conflicting after {MAX_COMMIT_RETRIES} retries; re-run sync");
                }
            }
        }
    };
    let mut out = format!("{}: pushed {n_chunks} chunks (commit {oid})\n", target.label());

    // 7. Reindex as a separate commit: forced (retried until it lands) or, past the threshold,
    // best-effort (one shot, warn on a conflict — the next sync retries).
    if force_reindex {
        out.push_str(&reindex_forced(&repo, &dataset_uri, &opts, &rev).await?);
    } else if unindexed > REINDEX_THRESHOLD {
        out.push_str(&reindex_auto(&repo, &dataset_uri, &opts, &rev).await);
    }
    Ok(out)
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
        match hf_dataset::reindex(repo, dataset_uri, opts.clone(), rev, "funes sync: reindex".to_string()).await? {
            Reindexed::Committed(oid) => return Ok(format!("  reindexed (commit {oid})\n")),
            Reindexed::AlreadyCurrent => return Ok("  index already current\n".to_string()),
            Reindexed::Conflict => continue,
        }
    }
    bail!("reindex still conflicting after {MAX_COMMIT_RETRIES} retries; re-run sync --force-reindex")
}

/// Best-effort reindex during a normal sync: one attempt, never retried. The data is already
/// committed, so any failure here is a warning — the next sync past the threshold tries again.
async fn reindex_auto(
    repo: &HFRepository<RepoTypeDataset>,
    dataset_uri: &str,
    opts: &HashMap<String, String>,
    rev: &str,
) -> String {
    match hf_dataset::reindex(repo, dataset_uri, opts.clone(), rev, "funes sync: reindex".to_string()).await {
        Ok(Reindexed::Committed(oid)) => format!("  reindexed (commit {oid})\n"),
        Ok(Reindexed::AlreadyCurrent) => String::new(),
        Ok(Reindexed::Conflict) => {
            "  note: index not refreshed (remote head moved); will retry on a later sync\n".to_string()
        }
        Err(e) => format!("  note: index not refreshed ({e}); will retry on a later sync\n"),
    }
}
