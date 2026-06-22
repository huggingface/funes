//! `sync`: publish the local store's not-yet-remote chunks into a remote store on the HF Hub.
//!
//! Streamed, never a full mirror. "What's already there" is the store's own chunk ids, so the
//! delta is `local_ids − remote_ids` — the same primitive `index` uses. We shallow-stage the
//! remote (manifest + indices, not `data/`), add the missing rows, build/optimize the index, run
//! the pre-publish secret gate, then `create_commit` only the new files with a parent-commit CAS
//! guard: if the branch head moved, the commit fails and we say so (re-run sync). First publish
//! (remote absent) just builds the dataset fresh and uploads all of it.

use crate::db;
use crate::hub::{self, Store};
use crate::scan;
use anyhow::{bail, Context, Result};
use arrow_array::{RecordBatch, StringArray};
use futures::TryStreamExt;
use hf_hub::repository::{CommitOperation, RepoTreeEntry};
use hf_hub::HFClient;
use lance_index::optimize::OptimizeOptions;
use lancedb::index::scalar::FtsIndexBuilder;
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::index::Index;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::table::OptimizeAction;
use lancedb::Table;
use std::collections::HashSet;

// funes publishes by manipulating a Lance dataset's files directly — lancedb exposes no
// write-to-`hf://` or partial-fetch API — so sync couples to Lance's 7.x on-disk layout:
//   <db>/<table>.lance/{ data/ (immutable fragments), _versions/, _indices/, _transactions/ }
const TABLE_DIR_SUFFIX: &str = ".lance";
const DATA_DIR: &str = "data";

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

/// Publish the local store's new chunks to `target` (a remote store on the HF Hub).
pub async fn run_sync(target: Store) -> Result<String> {
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
    if to_push.is_empty() {
        return Ok(format!(
            "{}: already up to date ({} chunks)\n",
            target.label(),
            remote_ids.len()
        ));
    }
    let first_publish = remote_ids.is_empty();

    // 2. Rows + pre-publish secret gate. Re-stamp each batch with the local table's schema so its
    // metadata (the embedding-model id) rides along — query-result batches drop it, and on first
    // publish that schema is what create_table persists.
    let schema = local.schema().await?;
    let batches: Vec<RecordBatch> = rows_to_push(&local, &to_push, first_publish)
        .await?
        .into_iter()
        .map(|b| RecordBatch::try_new(schema.clone(), b.columns().to_vec()))
        .collect::<std::result::Result<_, _>>()?;
    scan::ensure_no_secrets(&texts(&batches))?;

    // 3. HF repo handle.
    let (owner, name, prefix) = parse_hf(&uri)?;
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to push")?;
    let client = HFClient::builder()
        .token(token)
        .build()
        .context("building hf-hub client")?;
    let repo = client.dataset(owner, name);
    let rev = revision.unwrap_or_else(|| "main".to_string());
    let table_prefix = format!("{prefix}/{}{TABLE_DIR_SUFFIX}", db::TABLE);

    // 4. Shallow-stage the existing remote (manifest + indices, not data/) and note its files.
    let staging = tempfile::tempdir()?;
    let db_dir = staging.path().join(&prefix);
    let mut remote_files: HashSet<String> = HashSet::new();
    let parent_commit = if first_publish {
        std::fs::create_dir_all(&db_dir)?;
        None
    } else {
        let mut tree = std::pin::pin!(repo.list_tree().recursive(true).send()?);
        while let Some(entry) = tree.try_next().await? {
            if let RepoTreeEntry::File { path, .. } = entry {
                if path.starts_with(&table_prefix) {
                    remote_files.insert(path);
                }
            }
        }
        if remote_files.is_empty() {
            bail!(
                "no files under {table_prefix} on the remote — wrong store URI, or lancedb's \
                 on-disk layout changed (sync assumes Lance 7.x; see tests/sync_round_trip.rs)"
            );
        }
        for f in &remote_files {
            let rel = f.strip_prefix(&format!("{table_prefix}/")).unwrap_or_default();
            if rel.starts_with(&format!("{DATA_DIR}/")) {
                continue; // shallow: skip the base fragments
            }
            repo.download_file()
                .filename(f.clone())
                .local_dir(staging.path().to_path_buf())
                .revision(rev.clone())
                .send()
                .await?;
        }
        let refs = repo.list_refs().send().await?;
        Some(
            refs.branches
                .iter()
                .find(|b| b.name == rev)
                .map(|b| b.target_commit.clone())
                .context("target branch not found on the remote")?,
        )
    };

    // 5. Build the new fragment(s) + index in staging.
    let conn = lancedb::connect(&db_dir.to_string_lossy()).execute().await?;
    if first_publish {
        let t = conn.create_table(db::TABLE, batches).execute().await?;
        let _ = t
            .create_index(&["text"], Index::FTS(FtsIndexBuilder::default()))
            .execute()
            .await;
        let _ = t
            .create_index(&["vector"], Index::IvfPq(IvfPqIndexBuilder::default()))
            .execute()
            .await;
    } else {
        let t = conn.open_table(db::TABLE).execute().await?;
        t.add(batches).execute().await?;
        let _ = t.optimize(OptimizeAction::Index(OptimizeOptions::default())).await;
    }

    // 6. Upload the new files (staging files the remote lacks); parent-commit CAS guards the head.
    let mut ops = Vec::new();
    for entry in walkdir::WalkDir::new(&db_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(staging.path()).unwrap_or(entry.path());
        let repo_path = rel.to_string_lossy().into_owned();
        if remote_files.contains(&repo_path) {
            continue;
        }
        ops.push(CommitOperation::add_file(repo_path, entry.path().to_path_buf()));
    }
    if ops.is_empty() {
        return Ok(format!("{}: nothing new to upload\n", target.label()));
    }
    let n_files = ops.len();
    let n_chunks = to_push.len();
    let message = format!("funes sync: +{n_chunks} chunks");

    let info = match parent_commit {
        Some(parent) => {
            repo.create_commit()
                .operations(ops)
                .commit_message(message)
                .parent_commit(parent)
                .revision(rev)
                .send()
                .await
        }
        None => {
            repo.create_commit()
                .operations(ops)
                .commit_message(message)
                .revision(rev)
                .send()
                .await
        }
    }
    .map_err(|e| anyhow::anyhow!("create_commit failed (if the remote head moved, re-run sync): {e}"))?;

    Ok(format!(
        "{}: pushed {n_chunks} chunks in {n_files} files (commit {})\n",
        target.label(),
        info.commit_oid.as_deref().unwrap_or("?")
    ))
}
