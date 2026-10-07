//! Serializes writers to a local memory: concurrent writers can lose, duplicate, or orphan rows
//! (Lance's commit guard doesn't prevent it on a local dataset), so at most one mutates at a time. It
//! lives in the binary, not a launcher script, so the rule holds for every writer however it started.
//! An advisory `flock`, released on drop and on process death; contention fails loudly, never blocks.
//! Readers take no lock — Lance gives each a consistent snapshot. [`try_lock_file`] exposes the same
//! `flock` on any path, for the binary's other advisory locks.
//!
//! The lock lives in the memory it guards, so its writers exclude each other whichever funes home
//! they run from.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::dataset;

/// An exclusive advisory lock on a local memory, released on drop (and on process death).
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

/// The lock of the memory at `memory_dir`: inside it, but outside its Lance dataset, so version
/// cleanup never reaps it.
fn lockfile_path(memory_dir: &Path) -> PathBuf {
    memory_dir.join("store.lock")
}

impl MemoryLock {
    /// Try to take the lock of the home's own memory without blocking: `Some` if acquired, `None` if
    /// another operation holds it. A caller that wants to wait retries this itself.
    pub fn try_acquire() -> Result<Option<Self>> {
        Ok(try_lock_file(&lockfile_path(Path::new(&dataset::local_memory_dir())))?.map(Self))
    }

    /// Take the lock of the home's own memory, or fail if another memory operation holds it. Never
    /// blocks.
    pub fn acquire() -> Result<Self> {
        Self::try_acquire()?.ok_or_else(in_progress)
    }

    /// Take the lock of the memory at `memory_dir`, or fail if another memory operation holds it.
    /// Never blocks.
    pub fn acquire_in(memory_dir: &Path) -> Result<Self> {
        try_lock_file(&lockfile_path(memory_dir))?
            .map(Self)
            .ok_or_else(in_progress)
    }
}

fn in_progress() -> anyhow::Error {
    anyhow!("another funes memory operation is in progress; retry once it finishes")
}

/// Whether a funes up to 1.6 is writing the home's memory: those lock `store.lock` in the funes
/// home rather than in the memory.
pub(crate) fn older_writer_running() -> Result<bool> {
    let path = dataset::funes_dir().join("store.lock");
    Ok(path.exists() && try_lock_file(&path)?.is_none())
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

    #[test]
    fn a_memory_is_locked_under_any_spelling_of_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory");
        let held = MemoryLock::acquire_in(&memory).unwrap();
        let other_spelling = memory.join("..").join("memory");
        assert!(MemoryLock::acquire_in(&other_spelling).is_err());
        drop(held);
        assert!(MemoryLock::acquire_in(&other_spelling).is_ok());
    }
}
