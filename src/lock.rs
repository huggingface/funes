//! Serializes writers to the local store: concurrent writers can lose, duplicate, or orphan rows
//! (Lance's commit guard doesn't prevent it on a local dataset), so at most one mutates at a time. It
//! lives in the binary, not a launcher script, so the rule holds for every writer however it started.
//! An advisory `flock`, released on drop and on process death; contention fails loudly, never blocks.
//! Readers take no lock — Lance gives each a consistent snapshot.

use std::fs::{File, TryLockError};

use anyhow::{bail, Context, Result};

use crate::dataset;

/// An exclusive advisory lock on the local store, released on drop (and on process death).
#[derive(Debug)]
pub struct StoreLock(#[allow(dead_code)] File);

fn open_lockfile() -> Result<File> {
    let dir = dataset::funes_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // A sibling of store/, not inside the Lance dataset, so version cleanup never reaps it.
    let path = dir.join("store.lock");
    File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening the store lock at {}", path.display()))
}

impl StoreLock {
    /// Take the lock, or fail if another store operation holds it. Never blocks.
    pub fn acquire() -> Result<Self> {
        let f = open_lockfile()?;
        match f.try_lock() {
            Ok(()) => Ok(Self(f)),
            Err(TryLockError::WouldBlock) => {
                bail!("another funes store operation is in progress; retry once it finishes")
            }
            Err(TryLockError::Error(e)) => Err(e).context("locking the store"),
        }
    }
}
