//! funes's home, `$FUNES_HOME` or `~/.funes`, and where it keeps each thing. The agent registry and
//! the install id live under `~/.funes` whatever the home, as their modules say.

use std::path::PathBuf;

/// funes's home: `$FUNES_HOME`, else `~/.funes`.
pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("FUNES_HOME") {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".funes")
}

/// The local memory. An earlier funes kept it at `<home>/store`, which is renamed here once, when
/// this path doesn't exist yet. Best-effort: a failed rename leaves the old memory unfound, which
/// reads as "no index yet".
pub fn memory_dir() -> PathBuf {
    let memory = dir().join("memory");
    let legacy = dir().join("store");
    if legacy.is_dir() && !memory.exists() {
        let _ = std::fs::rename(&legacy, &memory);
    }
    memory
}

/// Every integration's spool, one directory per integration id.
pub fn spool_root() -> PathBuf {
    dir().join("spool")
}

/// What this host pushed, one receipt per remote memory, and the locks of the pushes in flight.
pub fn pushed_dir() -> PathBuf {
    dir().join("pushed")
}

/// The memory this host is bound to, as typed; absent when the host is local.
pub fn bound_file() -> PathBuf {
    dir().join("bound")
}

/// What indexing recorded of each source unit.
pub fn index_state_file() -> PathBuf {
    dir().join("state.json")
}

/// The source sessions the last indexing run left rows owed for.
pub fn index_coverage_file() -> PathBuf {
    dir().join("index-coverage.json")
}

/// The session sketches, cached.
pub fn sketch_cache_dir() -> PathBuf {
    dir().join("cache/session-sketch")
}

/// The lock a funes up to 1.6 takes on the local memory: in the home, not in the memory.
pub fn legacy_lock_file() -> PathBuf {
    dir().join("store.lock")
}
