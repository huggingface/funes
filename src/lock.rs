//! Serializes writers to the local memory: concurrent writers can lose, duplicate, or orphan rows
//! (Lance's commit guard doesn't prevent it on a local dataset), so at most one mutates at a time. It
//! lives in the binary, not a launcher script, so the rule holds for every writer however it started.
//! An advisory `flock`, released on drop and on process death; contention fails loudly, never blocks.
//! Readers take no lock — Lance gives each a consistent snapshot.

use std::fs::{File, TryLockError};

use anyhow::{anyhow, Context, Result};

use crate::dataset;

/// An exclusive advisory lock on the local memory, released on drop (and on process death).
#[derive(Debug)]
pub struct MemoryLock(#[allow(dead_code)] File);

fn open_lockfile() -> Result<File> {
    let dir = dataset::funes_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // A sibling of memory/, not inside the Lance dataset, so version cleanup never reaps it. The
    // filename stays `store.lock` (the pre-rename name) deliberately: during a `funes update` a
    // pre-rename binary may still be writing under this home, and both must contend on the *same*
    // lock file — renaming it would let old and new writers proceed concurrently and corrupt rows.
    let path = dir.join("store.lock");
    File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening the memory lock at {}", path.display()))
}

impl MemoryLock {
    /// Try to take the lock without blocking: `Some` if acquired, `None` if another operation holds
    /// it. A caller that wants to wait retries this itself.
    pub fn try_acquire() -> Result<Option<Self>> {
        let f = open_lockfile()?;
        match f.try_lock() {
            Ok(()) => Ok(Some(Self(f))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e).context("locking the memory"),
        }
    }

    /// Take the lock, or fail if another memory operation holds it. Never blocks.
    pub fn acquire() -> Result<Self> {
        Self::try_acquire()?
            .ok_or_else(|| anyhow!("another funes memory operation is in progress; retry once it finishes"))
    }
}
