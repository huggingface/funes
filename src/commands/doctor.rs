//! The `doctor` command: report what a memory has wrong with it and repair what the caller
//! confirms. Every fault it knows is one funes has produced — duplicate rows a raced publish
//! landed, index bookkeeping that outlived the transcripts it describes, lock files that guard
//! nothing, disk a deleted row still occupies. Nothing is repaired unasked: each finding is
//! printed, then offered, and a declined offer leaves the memory exactly as it was.
//!
//! Two surfaces. The memory's own table, and funes's home — the local bookkeeping, lock files, and
//! disk, which only the default local memory has. A remote memory's table is reported and never
//! rewritten: a delete writes its deletion file past the seam a guarded Hub commit is built from
//! (pinned by a test in [`crate::memory::remote`]), so committing one would publish a dataset that
//! no longer opens. Removing a published row is a republish, not a repair.

use crate::memory::dataset::{self, delete_rowids};
use crate::memory::remote;
use crate::memory::{lock, Memory, MemoryState};

use anyhow::{Context, Result};
use arrow_array::{StringArray, UInt64Array};
use futures::TryStreamExt;
use lance::dataset::cleanup::{cleanup_old_versions, CleanupPolicyBuilder};
use lance::dataset::optimize::{compact_files, CompactionOptions};
use lance::dataset::{Dataset, ROW_ID};
use std::collections::HashMap;
use std::fs::File;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};

/// Dataset versions kept when disk is reclaimed. Lance addresses a read at a version, so the recent
/// ones are what a reader that opened just before a repair still resolves.
const RETAIN_VERSIONS: usize = 2;

pub async fn run(memory: Memory, yes: bool) -> Result<()> {
    println!("memory: {}", memory.label());
    match memory.state().await? {
        MemoryState::Ready(ds) => check_table(&memory, ds, yes).await?,
        MemoryState::Empty => println!("  nothing indexed here yet — no table to check"),
        MemoryState::Missing => println!("  {}", memory.missing_error()),
        MemoryState::Unauthorized => println!("  {}", memory.unauthorized_error()),
        MemoryState::Offline => println!("  {} is unreachable — nothing to check while offline", memory.label()),
    }
    // The home holds one local memory's bookkeeping, so it is only this memory's to repair when
    // this memory is that one.
    if memory.is_default_local() {
        check_home(yes).await?;
    }
    Ok(())
}

/// The faults visible in the table itself: rows stored more than once, and a stale index.
async fn check_table(memory: &Memory, ds: Dataset, yes: bool) -> Result<()> {
    let rows = ds.count_rows(None).await?;
    println!("chunks: {rows}");

    let extra = extra_rowids(&id_rows(&ds).await?);
    if extra.is_empty() {
        println!("  duplicate rows: none");
    } else {
        println!(
            "  duplicate rows: {} of {rows} — the same chunk stored more than once (a duplicate \
             both ranks and renders twice)",
            extra.len()
        );
        match memory {
            Memory::Local { path } => {
                if offer("  drop the extra copies?", yes) {
                    let _lock = lock::MemoryLock::acquire()?;
                    let uri = dataset::table_uri(&path.to_string_lossy());
                    let mut ds = dataset::open(&uri, HashMap::new()).await?;
                    let dropped = delete_rowids(&mut ds, &extra).await?;
                    println!("  dropped {dropped} row(s)");
                }
            }
            // Publishing the deduplicated table is the only way to drop these, and that is a
            // republish of the whole memory rather than something to offer mid-report.
            Memory::Remote { .. } => println!(
                "    a published row can only go by republishing the memory — \
                 `funes doctor` repairs the local memory that pushes here"
            ),
        }
    }

    // Rows outside the FTS/IVF indexes are still found, by a brute-force scan — a cost, not a
    // fault, so the finding says what it costs rather than calling the memory broken.
    let unindexed = remote::max_unindexed_rows(&ds).await;
    if unindexed == 0 {
        println!("  indexes: current");
    } else {
        println!("  indexes: {unindexed} row(s) not folded in (answered by a brute-force scan)");
        match memory {
            Memory::Local { path } => {
                if offer("  rebuild them?", yes) {
                    let _lock = lock::MemoryLock::acquire()?;
                    let uri = dataset::table_uri(&path.to_string_lossy());
                    let mut ds = dataset::open(&uri, HashMap::new()).await?;
                    dataset::build_indexes(&mut ds, |phase| println!("  building {phase}…")).await;
                    println!("  rebuilt");
                }
            }
            Memory::Remote { .. } => println!("    refresh it with `funes push {} --force-reindex`", memory.label()),
        }
    }
    Ok(())
}

/// The bookkeeping files doctor prunes: the file, the object member holding the keys (`None` when
/// the document's own keys are them), and what a stale entry costs. Only a run that enumerates a
/// unit again clears its entry, so a transcript that is gone leaves one behind for good.
const BOOKKEEPING: &[(&str, Option<&str>, &str)] = &[
    (
        "state.json",
        None,
        "they only take up room, since a transcript that comes back is read again anyway",
    ),
    (
        "index-coverage.json",
        Some("pending"),
        "`funes status` counts them as indexing still owed",
    ),
];

/// The faults in funes's home: bookkeeping whose transcripts are gone, lock files that guard
/// nothing, and disk no live version needs.
async fn check_home(yes: bool) -> Result<()> {
    let dir = dataset::funes_dir();
    println!("funes home: {}", dir.display());

    // Every write under the home is a writer's, so hold the memory lock for all of them: the
    // bookkeeping files are rewritten whole, and a concurrent indexing run would lose its update.
    let mut held: Option<lock::MemoryLock> = None;
    let mut lock_or_skip = |what: &str| -> Result<bool> {
        if held.is_none() {
            match lock::MemoryLock::try_acquire()? {
                Some(l) => held = Some(l),
                None => {
                    println!("  another funes memory operation is in progress — leaving {what} alone");
                    return Ok(false);
                }
            }
        }
        Ok(true)
    };

    for (file, field, cost) in BOOKKEEPING {
        let path = dir.join(file);
        let Some(doc) = read_json(&path) else {
            continue;
        };
        let (total, stale) = stale_keys(&doc, *field);
        if stale.is_empty() {
            println!("  {file}: {total} entr{}, none stale", plural_y(total));
            continue;
        }
        println!(
            "  {file}: {} of {total} entr{} name a transcript that is gone — {cost}",
            stale.len(),
            plural_y(total)
        );
        if offer("  drop them?", yes) && lock_or_skip(file)? {
            // Re-read under the lock: an indexing run that finished while the offer stood wrote
            // its own update, and rewriting the copy read before it would undo that.
            let doc = read_json(&path).unwrap_or(doc);
            let pruned = without_keys(doc, *field, &stale);
            write_json(&path, &pruned)?;
            println!("  dropped {} entr{}", stale.len(), plural_y(stale.len()));
        }
    }

    let orphans = orphan_receipt_locks(&dir);
    if orphans.is_empty() {
        println!("  lock files: none left over");
    } else {
        println!(
            "  lock files: {} push-receipt lock(s) whose receipt is gone",
            orphans.len()
        );
        if offer("  remove them?", yes) {
            let mut removed = 0;
            for path in &orphans {
                // Take the lock before unlinking it: a lock file another process holds is one it is
                // still serializing publishes on, whatever its receipt looks like from here.
                match File::options().write(true).open(path).map(|f| f.try_lock()) {
                    Ok(Ok(())) => {
                        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
                        removed += 1;
                    }
                    _ => println!("  {} is held — left alone", path.display()),
                }
            }
            println!("  removed {removed} lock file(s)");
        }
    }

    // The pre-rename memory location. funes renames it into place only when there is no `memory/`
    // yet, so a home that had both keeps a table nothing reads.
    let legacy = dir.join("store");
    if legacy.join(format!("{}.lance", dataset::TABLE)).is_dir() {
        println!(
            "  {}: a table nothing reads — the local memory is {}",
            legacy.display(),
            dataset::local_memory_dir()
        );
        if offer("  delete it?", yes) && lock_or_skip("it")? {
            std::fs::remove_dir_all(&legacy).with_context(|| format!("removing {}", legacy.display()))?;
            println!("  deleted");
        }
    }

    // Disk last: it reclaims what the repairs above made unreferenced.
    let uri = dataset::table_uri(&dataset::local_memory_dir());
    if let Ok(ds) = dataset::open(&uri, HashMap::new()).await {
        let versions = ds.versions().await?.len();
        println!("  dataset versions: {versions} (a read still resolves each one)");
        if versions > RETAIN_VERSIONS
            && offer("  compact the table and drop the old versions?", yes)
            && lock_or_skip("the table")?
        {
            println!("  {}", reclaim(&uri).await?);
        }
    }
    Ok(())
}

/// Compact the table, then drop every version but the most recent [`RETAIN_VERSIONS`]. Compaction
/// is what turns a deleted row back into free space — a delete only writes a deletion file, and the
/// row's bytes stay in the fragment until it is rewritten. The cleanup keeps lance's default
/// verification, which spares any file young enough to belong to a write still in flight.
async fn reclaim(uri: &str) -> Result<String> {
    let mut ds = dataset::open(uri, HashMap::new()).await?;
    let metrics = compact_files(&mut ds, CompactionOptions::default(), None).await?;
    let policy = CleanupPolicyBuilder::default()
        .retain_n_versions(&ds, RETAIN_VERSIONS)
        .await?
        .build();
    let stats = cleanup_old_versions(&ds, policy).await?;
    Ok(format!(
        "compacted {} fragment(s) into {}; removed {} version(s), {}",
        metrics.fragments_removed,
        metrics.fragments_added,
        stats.old_versions,
        remote::human_bytes(stats.bytes_removed)
    ))
}

/// Every row's chunk id and row id, in scan order. Both columns are required: a repair that guessed
/// at a missing one would delete the wrong rows.
async fn id_rows(ds: &Dataset) -> Result<Vec<(String, u64)>> {
    let mut scan = ds.scan();
    scan.project(&["id"])?;
    scan.with_row_id();
    let mut stream = scan.try_into_stream().await?;
    let mut out = Vec::new();
    while let Some(batch) = stream.try_next().await? {
        let ids = batch
            .column_by_name("id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .context("the memory's rows carry no readable `id`")?;
        let rowids = batch
            .column_by_name(ROW_ID)
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            .context("the scan returned no row addresses")?;
        for i in 0..batch.num_rows() {
            out.push((ids.value(i).to_string(), rowids.value(i)));
        }
    }
    Ok(out)
}

/// The row ids to drop so each chunk id keeps one row: every copy but the last the scan reports. A
/// chunk id hashes the coordinates its row was built from, not the text, so a re-split of a rewritten
/// turn can reuse an id with different bytes — the last copy is the one written most recently.
fn extra_rowids(rows: &[(String, u64)]) -> Vec<u64> {
    let mut keep: HashMap<&str, u64> = HashMap::with_capacity(rows.len());
    for (id, rowid) in rows {
        keep.insert(id.as_str(), *rowid);
    }
    rows.iter()
        .filter(|(id, rowid)| keep.get(id.as_str()) != Some(rowid))
        .map(|(_, rowid)| *rowid)
        .collect()
}

/// A bookkeeping file's entry count and the keys among them that name a transcript no longer on
/// disk. `field` is the object member holding the keys, or `None` when the document's own keys are
/// them. A key that isn't an absolute path is another harness's addressing (a bare session id, say),
/// which the filesystem can say nothing about — those are never stale.
fn stale_keys(doc: &serde_json::Value, field: Option<&str>) -> (usize, Vec<String>) {
    let keys: Vec<String> = match field {
        None => doc.as_object().map(|o| o.keys().cloned().collect()),
        Some(f) => doc
            .get(f)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()),
    }
    .unwrap_or_default();
    let stale = keys
        .iter()
        .filter(|k| {
            let p = Path::new(k.as_str());
            p.is_absolute() && !p.exists()
        })
        .cloned()
        .collect();
    (keys.len(), stale)
}

/// `doc` without `drop`: removed from the object's own keys, or from the array at `field`.
fn without_keys(mut doc: serde_json::Value, field: Option<&str>, drop: &[String]) -> serde_json::Value {
    match field {
        None => {
            if let Some(obj) = doc.as_object_mut() {
                for key in drop {
                    obj.remove(key);
                }
            }
        }
        Some(f) => {
            if let Some(arr) = doc.get_mut(f).and_then(|v| v.as_array_mut()) {
                arr.retain(|v| !v.as_str().is_some_and(|s| drop.iter().any(|d| d == s)));
            }
        }
    }
    doc
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_json(path: &Path, doc: &serde_json::Value) -> Result<()> {
    std::fs::write(path, serde_json::to_string(doc)?).with_context(|| format!("writing {}", path.display()))
}

/// Push-receipt lock files whose receipt is gone — a memory this host no longer publishes to. The
/// memory lock's own file is never a candidate: it is the file writers contend on, and an unheld one
/// is the normal resting state, since the kernel releases an `flock` when its process exits.
fn orphan_receipt_locks(home: &Path) -> Vec<PathBuf> {
    let receipts = home.join("pushed");
    let Ok(entries) = std::fs::read_dir(receipts.join(".locks")) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.file_name().is_some_and(|name| !receipts.join(name).is_file()))
        .collect()
}

/// Offer a repair. `--yes` takes every offer; with no terminal there is nobody to ask, so the run
/// reports the finding and changes nothing.
fn offer(prompt: &str, yes: bool) -> bool {
    if yes {
        eprintln!("{prompt} yes");
        return true;
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("{prompt} (run with --yes to apply)");
        return false;
    }
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    match std::io::stdin().read_line(&mut answer) {
        Ok(n) if n > 0 => matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        _ => false,
    }
}

/// `y`/`ies`, for "entry"/"entries".
fn plural_y(n: usize) -> &'static str {
    if n == 1 {
        "y"
    } else {
        "ies"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_rowids_keeps_the_last_copy_of_each_chunk() {
        let rows = vec![
            ("a".to_string(), 10),
            ("b".to_string(), 11),
            ("a".to_string(), 12),
            ("a".to_string(), 13),
        ];
        assert_eq!(extra_rowids(&rows), vec![10, 12]);
    }

    #[test]
    fn extra_rowids_finds_nothing_in_a_clean_memory() {
        let rows = vec![("a".to_string(), 1), ("b".to_string(), 2)];
        assert!(extra_rowids(&rows).is_empty());
    }

    #[test]
    fn stale_keys_spares_a_live_path_and_a_key_that_is_no_path() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live.jsonl");
        std::fs::write(&live, "{}").unwrap();
        let gone = dir.path().join("gone.jsonl");
        let doc = serde_json::json!({
            live.to_string_lossy(): {"sig": "1:2"},
            gone.to_string_lossy(): {"sig": "3:4"},
            // A harness that keys its units by its own session id — nothing on disk to probe.
            "20260507_180845_20f0ac": {"sig": "5:6"},
        });
        let (total, stale) = stale_keys(&doc, None);
        assert_eq!(total, 3);
        assert_eq!(stale, vec![gone.to_string_lossy().to_string()]);
    }

    #[test]
    fn stale_keys_reads_the_pending_member() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone.jsonl").to_string_lossy().to_string();
        let doc = serde_json::json!({ "pending": [gone.clone(), "bare-session-id"] });
        let (total, stale) = stale_keys(&doc, Some("pending"));
        assert_eq!((total, stale), (2, vec![gone.clone()]));

        let pruned = without_keys(doc, Some("pending"), &[gone]);
        assert_eq!(pruned["pending"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn orphan_receipt_locks_are_the_ones_whose_receipt_is_gone() {
        let home = tempfile::tempdir().unwrap();
        let pushed = home.path().join("pushed");
        std::fs::create_dir_all(pushed.join(".locks")).unwrap();
        std::fs::write(pushed.join("hf___datasets_o_live"), "id\n").unwrap();
        std::fs::write(pushed.join(".locks").join("hf___datasets_o_live"), "").unwrap();
        std::fs::write(pushed.join(".locks").join("hf___datasets_o_gone"), "").unwrap();

        let orphans = orphan_receipt_locks(home.path());
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].ends_with("hf___datasets_o_gone"));
    }
}
