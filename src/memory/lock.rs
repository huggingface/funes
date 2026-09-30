//! Serializes writers to the local memory: concurrent writers can lose, duplicate, or orphan rows
//! (Lance's commit guard doesn't prevent it on a local dataset), so at most one mutates at a time. It
//! lives in the binary, not a launcher script, so the rule holds for every writer however it started.
//! An advisory `flock`, released on drop and on process death; contention fails loudly, never blocks.
//! Readers take no lock — Lance gives each a consistent snapshot. [`try_lock_file`] exposes the same
//! `flock` on any path, for the binary's other advisory locks.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::dataset;

/// An exclusive advisory lock on the local memory, released on drop (and on process death).
#[derive(Debug)]
pub struct MemoryLock(#[allow(dead_code)] FileLock);

/// An exclusive advisory lock on a file, released on drop.
#[derive(Debug)]
pub(crate) struct FileLock(File);

impl Drop for FileLock {
    fn drop(&mut self) {
        // A child spawned meanwhile shares the open file until it execs, so closing alone won't release it.
        let _ = self.0.unlock();
    }
}

/// Take an exclusive advisory lock on `path`, creating the file and its directory if needed. `None`
/// if another holder has it — including this process, which `flock` treats no differently. Never
/// blocks; released when the returned handle drops, and on process death.
pub(crate) fn try_lock_file(path: &Path) -> Result<Option<FileLock>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let f = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening the lock at {}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(FileLock(f))),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

fn lockfile_path() -> PathBuf {
    // A sibling of memory/, not inside the Lance dataset, so version cleanup never reaps it. The
    // filename stays `store.lock` (the pre-rename name) deliberately: during a `funes update` a
    // pre-rename binary may still be writing under this home, and both must contend on the *same*
    // lock file — renaming it would let old and new writers proceed concurrently and corrupt rows.
    dataset::funes_dir().join("store.lock")
}

impl MemoryLock {
    /// Try to take the lock without blocking: `Some` if acquired, `None` if another operation holds
    /// it. A caller that wants to wait retries this itself.
    pub fn try_acquire() -> Result<Option<Self>> {
        Ok(try_lock_file(&lockfile_path())?.map(Self))
    }

    /// Take the lock, or fail if another memory operation holds it. Never blocks.
    pub fn acquire() -> Result<Self> {
        Self::try_acquire()?
            .ok_or_else(|| anyhow!("another funes memory operation is in progress; retry once it finishes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locked_file_is_taken_until_its_holder_drops_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("a.lock");
        let held = try_lock_file(&path).unwrap().expect("a free lock is taken");
        assert!(try_lock_file(&path).unwrap().is_none());
        assert!(try_lock_file(&dir.path().join("b.lock")).unwrap().is_some());
        drop(held);
        assert!(try_lock_file(&path).unwrap().is_some());
    }

    #[test]
    fn a_dropped_lock_is_released_while_a_copy_of_its_file_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.lock");
        let held = try_lock_file(&path).unwrap().expect("a free lock is taken");
        // What a child spawned while the lock is held inherits until it execs.
        let inherited = held.0.try_clone().unwrap();
        drop(held);
        assert!(try_lock_file(&path).unwrap().is_some());
        drop(inherited);
    }
}
