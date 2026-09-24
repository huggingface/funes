//! The spool: where an integration writes the turns files funes indexes for it,
//! `$FUNES_HOME/spool/<id>/`, one directory per integration id. funes owns the directory — a bundle
//! only writes into it; funes reads, records and deletes — and finds producers by listing it, so
//! indexing needs neither an installed integration nor a list of agents.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

use crate::memory::dataset;

/// Whether `s` is an id funes accepts: lowercase `[a-z0-9_-]`, non-empty. One charset serves the
/// integration id, which names a directory funes creates, and the `harness` facet a turn carries,
/// so an id is always a valid facet and a safe path segment.
pub fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The directory holding every integration's spool, `$FUNES_HOME/spool`.
pub fn spool_root() -> PathBuf {
    dataset::funes_dir().join("spool")
}

/// Where the integration `id` writes its turns files.
pub fn spool_dir(id: &str) -> PathBuf {
    spool_root().join(id)
}

/// Whether `path` is inside a spool funes resolved for itself, rather than a path someone named.
pub fn is_spool(path: &Path) -> bool {
    path.starts_with(spool_root())
}

/// The spool `--harness <id>` selects: a valid id whose directory a producer has created.
pub fn select(id: &str) -> Result<PathBuf> {
    if !is_id(id) {
        bail!("{id:?} is not an integration id (lowercase [a-z0-9_-])");
    }
    let dir = spool_dir(id);
    if !dir.is_dir() {
        bail!(
            "no {id} spool at {} — `funes add {id}` installs the integration that writes it; to index turns files elsewhere, pass their path",
            dir.display()
        );
    }
    Ok(dir)
}

/// Every spool a producer has created, in id order — what a no-argument `funes index` sweeps.
pub fn spools() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(spool_root()) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(is_id))
        .collect();
    dirs.sort();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_lowercase_digits_underscore_or_dash() {
        for ok in ["claude", "claude_code", "my-agent2", "x"] {
            assert!(is_id(ok), "{ok}");
        }
        for bad in ["", "Claude", "a b", "a/b", "a:b", ".hidden", "é"] {
            assert!(!is_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_spool_is_under_the_root_and_named_by_its_id() {
        assert_eq!(spool_dir("pi").file_name().unwrap(), "pi");
        assert!(is_spool(&spool_dir("pi").join("s.funes.jsonl")));
        assert!(
            !is_spool(Path::new("/elsewhere/pi")),
            "a directory elsewhere is not funes's"
        );
    }
}
