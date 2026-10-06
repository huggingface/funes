//! `compact`: merge a memory's small fragments, add its unindexed rows to the search indexes and
//! delete its old versions, local or remote, without indexing or pushing anything.

use super::push;
use crate::hub;
use crate::memory::dataset;
use crate::memory::lock;
use crate::memory::{Memory, MemoryState};
use crate::ui;
use anyhow::{bail, Context, Result};
use lance::Dataset;
use std::collections::HashMap;

/// Compact `target`, local or remote, and report the outcome.
pub async fn run(target: Memory) -> Result<String> {
    match target.state().await? {
        MemoryState::Offline => bail!(
            "{} is unreachable — can't compact it while offline (check your connection)",
            target.label()
        ),
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

    let (owner, name, _) = hub::parse_hf(uri)?;
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to compact a remote memory")?;
    // No HTTP retries, as `remote::compact` requires.
    let repo = hub::client(Some(token.as_str()), false)?.dataset(owner, name);
    let rev = "main".to_string();
    let dataset_uri = format!("{uri}/{}.lance", dataset::TABLE);
    let opts = HashMap::from([("hf_token".to_string(), token), ("revision".to_string(), rev.clone())]);
    eprintln!("compacting the remote…");
    let note = push::compact_remote(&repo, &dataset_uri, &opts, &rev, "funes compact").await?;
    Ok(format!("{}:\n{note}", target.label()))
}

/// Compact a local memory: merge the fragments no index covers yet, refresh the indexes, and delete
/// the versions older than ten minutes, so a read in flight isn't cut off. The caller holds the
/// memory lock.
pub(crate) async fn compact_local(ds: &mut Dataset) -> Result<()> {
    eprintln!("compacting the memory…");
    dataset::compact_fragments(ds, ui::index_progress).await?;
    dataset::build_indexes(ds, ui::index_progress).await?;

    // Best-effort: a failed cleanup waits for the next compaction.
    match ds.cleanup_old_versions(chrono::Duration::minutes(10), None, None).await {
        Ok(stats) if stats.bytes_removed > 0 => eprintln!(
            "reclaimed {:.1} MB from {} old version(s)",
            stats.bytes_removed as f64 / 1e6,
            stats.old_versions
        ),
        Ok(_) => {}
        Err(e) => eprintln!("note: version cleanup skipped — {e}"),
    }
    Ok(())
}
