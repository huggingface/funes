//! What this host pushed to each remote memory: the chunk ids it has seen there, one receipt per
//! remote under funes's home. Kept on this host only: a shared remote's rows from other hosts say
//! nothing about what this host still owes it.

use super::dataset;
use super::{Memory, MemoryState};
use crate::home;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The receipt for `memory_uri`.
fn path(memory_uri: &str) -> PathBuf {
    home::pushed_dir().join(sanitize(memory_uri))
}

/// The file a push to `memory_uri` locks for its whole run — not the receipt's lock, which
/// [`record`] takes per write.
pub(crate) fn push_lock_path(memory_uri: &str) -> PathBuf {
    home::pushed_dir()
        .join(".locks")
        .join(format!("{}.push", sanitize(memory_uri)))
}

fn load_from(path: &Path) -> Option<HashSet<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// The chunk ids this host has seen on `memory_uri`; `None` when it never pushed there.
pub(crate) fn load(memory_uri: &str) -> Option<HashSet<String>> {
    load_from(&path(memory_uri))
}

/// [`held_before`], from the receipt at `path`.
fn held_before_at(path: &Path) -> bool {
    load_from(path).is_some_and(|pushed| !pushed.is_empty())
}

/// Whether `memory_uri` has held chunks from this host before: its receipt exists and is not empty.
pub(crate) fn held_before(memory_uri: &str) -> bool {
    held_before_at(&path(memory_uri))
}

fn record_at<'a>(path: &Path, ids: impl IntoIterator<Item = &'a String>) -> Result<()> {
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

    let mut pushed = load_from(path).unwrap_or_default();
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

/// Add `ids` to the receipt of `memory_uri`.
pub(crate) fn record<'a>(memory_uri: &str, ids: impl IntoIterator<Item = &'a String>) -> Result<()> {
    record_at(&path(memory_uri), ids)
}

/// The local sessions holding rows `memory_uri` lacks, by this host's receipt.
/// Empty when nothing is owed, including when this host never pushed there or has nothing indexed.
pub(crate) async fn owed_sessions(memory_uri: &str) -> Result<HashSet<String>> {
    let Some(pushed_ids) = load(memory_uri) else {
        return Ok(HashSet::new());
    };
    let MemoryState::Ready(local) = Memory::local().state().await? else {
        return Ok(HashSet::new());
    };
    Ok(dataset::ids_by_session(&local)
        .await?
        .into_iter()
        .filter(|(_, ids)| ids.iter().any(|id| !pushed_ids.contains(id)))
        .map(|(session, _)| session)
        .collect())
}

/// Whether this host still owes `memory_uri` rows of `session_id` — reads that one session's ids,
/// not the whole local memory.
pub(crate) async fn owes_session(memory_uri: &str, session_id: &str) -> Result<bool> {
    let Some(pushed_ids) = load(memory_uri) else {
        return Ok(false);
    };
    let MemoryState::Ready(local) = Memory::local().state().await? else {
        return Ok(false);
    };
    let ids = dataset::ids(&local, Some(&dataset::eq("session_id", session_id))).await?;
    Ok(!ids.is_subset(&pushed_ids))
}

/// A memory URI as one path-safe file name.
fn sanitize(uri: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_receipt_is_an_atomic_growing_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt");
        let first = ["b".to_string(), "a".to_string()];
        record_at(&path, &first).unwrap();
        let second = ["b".to_string(), "c".to_string()];
        record_at(&path, &second).unwrap();
        assert_eq!(
            load_from(&path).unwrap(),
            ["a", "b", "c"].into_iter().map(str::to_string).collect()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "a\nb\nc\n");
    }

    #[test]
    fn an_empty_receipt_does_not_make_a_known_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt");
        assert!(!held_before_at(&path));
        // The receipt bootstrap of a push that then stops at the prompt writes an empty file.
        record_at(&path, std::iter::empty()).unwrap();
        assert!(path.is_file());
        assert!(!held_before_at(&path));
        record_at(&path, &["a".to_string()]).unwrap();
        assert!(held_before_at(&path));
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
                    record_at(&path, [&id]).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let expected: HashSet<_> = (0..writers).map(|i| format!("id-{i}")).collect();
        assert_eq!(load_from(&path).unwrap(), expected);
    }

    #[test]
    fn a_push_locks_its_own_file_per_remote() {
        let uri = "hf://datasets/acme/kb";
        let receipt = path(uri);
        let receipt_lock = receipt.parent().unwrap().join(".locks").join(sanitize(uri));
        assert_ne!(push_lock_path(uri), receipt);
        assert_ne!(push_lock_path(uri), receipt_lock);
        assert_ne!(push_lock_path(uri), push_lock_path("hf://datasets/acme/other"));
    }
}
