//! `push`: publish the local store's not-yet-remote chunks into a remote store on the HF Hub.
//!
//! Streamed, never a full mirror. "What's already there" is the store's own chunk ids, so the
//! delta is `local_ids − remote_ids` — the same primitive `index` uses. `push` is orchestration:
//! it computes the delta, runs the pre-publish secret gate, and drives the HF write operations in
//! [`crate::hf_dataset`], which own the atomic, parent-commit-guarded commits.
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

use crate::dataset;
use crate::hf_dataset::{self, Appended, Reindexed};
use crate::hub::{self, Store};
use crate::scan;
use anyhow::{bail, Context, Result};
use arrow_array::{RecordBatch, RecordBatchIterator, StringArray};
use hf_hub::repository::CommitOperation;
use hf_hub::{HFClient, HFError, HFRepository, RepoTypeDataset};
use lance::dataset::WriteParams;
use lance::Dataset;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Reindex the remote once this many appended rows are sitting unindexed (answered by a
/// brute-force scan until folded in). Bounds per-query cost, not push count, and is stateless —
/// [`hf_dataset::append`] reads it straight from Lance's index stats.
const REINDEX_THRESHOLD: u64 = 2_000;

/// Cap on CAS-conflict retries (the data append, and a forced reindex) when the branch head keeps
/// moving under us, so a busy remote can't spin forever.
const MAX_COMMIT_RETRIES: u32 = 10;

/// Every chunk id in a store, or empty if it can't be opened (absent local index, not-yet-created
/// or inaccessible remote).
pub async fn store_ids(store: &Store) -> HashSet<String> {
    match store.open().await {
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

/// Every chunk id in a store (a plain `id`-column scan; plain scans aren't limit-capped).
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

/// The to-push rows (all columns) from the local store. First publish reads everything; an append
/// reads just the missing ids via an `id IN (…)` predicate.
async fn rows_to_push(local: &Dataset, to_push: &HashSet<String>, first_publish: bool) -> Result<Vec<RecordBatch>> {
    let filter = (!first_publish).then(|| {
        let list = to_push
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("id IN ({list})")
    });
    dataset::scan_rows(local, &[], filter.as_deref(), None).await
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
pub async fn run_push(target: Store, force_reindex: bool) -> Result<String> {
    let uri = match &target {
        Store::Remote { uri } => uri.clone(),
        Store::Local { .. } => {
            bail!("push target must be a remote `hf://` store — it publishes your local index up to the Hub")
        }
    };

    // Fail fast when offline. Otherwise the remote read below sees an empty set, mistakes the
    // unreachable remote for a first publish, and builds the whole dataset before the commit fails.
    if !hub::remote_reachable(&uri).await {
        bail!("{uri} is unreachable — can't push while offline (check your connection)");
    }

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
    let (owner, name, prefix) = hub::parse_hf(&uri)?;
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to push")?;
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
        let note = reindex_forced(&repo, &dataset_uri, &opts, &rev).await?;
        return Ok(format!(
            "{}: up to date ({} chunks)\n{note}",
            target.label(),
            remote_ids.len()
        ));
    }

    // 4. Rows + pre-publish secret gate. Re-stamp each batch with the local dataset's schema so its
    // metadata (the embedding-model id) rides along — scan-result batches drop it, and on first
    // publish that schema is what the new dataset persists.
    let schema: arrow_schema::SchemaRef = Arc::new(arrow_schema::Schema::from(local.schema()));
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
        // Empty prefix = dataset at the repo root; joining "" would leave a stray trailing separator.
        let db_dir = if prefix.is_empty() {
            staging.path().to_path_buf()
        } else {
            staging.path().join(&prefix)
        };
        std::fs::create_dir_all(&db_dir)?;
        let table_uri = dataset::table_uri(&db_dir.to_string_lossy());
        let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema.clone());
        let mut ds = Dataset::write(reader, &table_uri, Some(WriteParams::default()))
            .await
            .context("building the dataset for first publish")?;
        dataset::build_indexes(&mut ds).await;

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
            .commit_message(format!("funes push: +{n_chunks} chunks"))
            .revision(rev.clone())
            .send()
            .await
            .map_err(|e| anyhow::Error::new(e).context("create_commit failed"))?;
        return Ok(format!(
            "{}: pushed {n_chunks} chunks (commit {})\n",
            target.label(),
            info.commit_oid.as_deref().unwrap_or("?")
        ));
    }

    // 6. Append the data and commit it, retrying against a fresh head if a concurrent push moved it
    // (each attempt re-appends onto the new manifest — the data commit is small, so this is cheap).
    let message = format!("funes push: +{n_chunks} chunks");
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

    #[test]
    fn is_read_only_matches_the_type_not_the_message() {
        // We match the typed HFError, not the rendered text — a plain error that merely mentions
        // 403/Forbidden is not a read-only signal. (HFError is #[non_exhaustive], so the positive
        // path can't be built here; it's exercised by the gated round-trip.)
        assert!(!is_read_only(&anyhow::anyhow!("server said 403 Forbidden")));
        assert!(!is_read_only(&anyhow::anyhow!("no HF token")));
    }
}
