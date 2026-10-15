//! `compact`: merge a memory's small fragments, add its unindexed rows to the search indexes and
//! delete its old versions, local or remote, without indexing or pushing anything.

use crate::hub;
use crate::memory::dataset;
use crate::memory::lock;
use crate::memory::remote::{self, Compacted};
use crate::memory::{Memory, MemoryState};
use crate::ui;
use anyhow::{anyhow, bail, Result};
use lance::Dataset;

/// Compact `target`, local or remote, and report the outcome.
pub async fn run(target: Memory) -> Result<String> {
    let label = target.label();
    compact(target).await.map_err(|e| {
        if hub::is_read_only(&e) {
            anyhow!("{label} is read-only for your token — compacting it needs write access (check your HF token)")
        } else {
            e
        }
    })
}

async fn compact(target: Memory) -> Result<String> {
    match target.state().await? {
        MemoryState::Offline => return Err(target.offline_error()),
        MemoryState::Missing => return Err(target.missing_error()),
        MemoryState::Unauthorized => return Err(target.unauthorized_error()),
        MemoryState::Empty => return Ok(format!("{}: nothing to compact\n", target.label())),
        MemoryState::Ready(_) => {}
    }
    let uri = match &target {
        Memory::Local { path } => {
            // A local compaction writes the memory, so it holds that memory's lock, as an index run
            // does, and reopens it under the lock to start from its latest version.
            let _lock = lock::MemoryLock::acquire_in(path)?;
            compact_local(&mut target.open().await?).await?;
            return Ok(format!("{}: compacted\n", target.label()));
        }
        Memory::Remote { uri } => uri,
    };

    let w = remote::writable(uri)?;
    eprintln!("compacting the remote…");
    let note = compact_remote(&w, "funes compact").await?;
    Ok(format!("{}:\n{note}", target.label()))
}

/// Forced compaction: ask [`remote::compact`] to compact and commit under `message`, retrying on a
/// head-moved conflict (it re-reads the head each call) until it lands or [`remote::MAX_COMMIT_RETRIES`] is
/// exceeded.
pub(crate) async fn compact_remote(w: &remote::Writable, message: &str) -> Result<String> {
    for _ in 0..=remote::MAX_COMMIT_RETRIES {
        match remote::compact(
            &w.repo,
            &w.dataset_uri,
            w.opts.clone(),
            &w.rev,
            message.to_string(),
            ui::index_progress,
        )
        .await?
        {
            Compacted::Committed(oid) => return Ok(format!("  compacted (commit {oid})\n")),
            Compacted::Uncleaned(oid, e) => {
                return Ok(format!(
                    "  compacted (commit {oid})\n  note: old versions not deleted ({e:#}); \
                     compact it again\n"
                ))
            }
            Compacted::AlreadyCompact => return Ok("  already compact\n".to_string()),
            Compacted::Conflict => continue,
        }
    }
    bail!(
        "compaction still conflicting after {} retries; compact it again",
        remote::MAX_COMMIT_RETRIES
    )
}

/// Compact a local memory: merge the fragments no index covers yet, refresh the indexes, and delete
/// the versions older than ten minutes, so a read in flight isn't cut off. The caller holds the
/// memory lock.
pub(crate) async fn compact_local(ds: &mut Dataset) -> Result<()> {
    eprintln!("compacting the memory…");
    dataset::compact_fragments(ds, ui::index_progress).await?;
    dataset::build_indexes(ds, ui::index_progress).await?;

    // Best-effort: a failed cleanup waits for the next compaction.
    match dataset::delete_versions_before(ds, chrono::Duration::minutes(10)).await {
        Ok(freed) if freed.bytes > 0 => eprintln!(
            "reclaimed {:.1} MB from {} old version(s)",
            freed.bytes as f64 / 1e6,
            freed.versions
        ),
        Ok(_) => {}
        Err(e) => eprintln!("note: version cleanup skipped — {e}"),
    }
    Ok(())
}
