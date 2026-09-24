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
/// funes deletes what it owns here, so the answer is by what the paths resolve to: a `..` or a
/// symlink cannot make a file elsewhere look like the spool's.
pub fn is_spool(path: &Path) -> bool {
    is_under(&spool_root(), path)
}

fn is_under(root: &Path, path: &Path) -> bool {
    match (root.canonicalize(), path.canonicalize()) {
        (Ok(root), Ok(path)) => path.starts_with(root),
        _ => false,
    }
}

/// The spool `--harness <id>` selects: a valid id whose directory a producer has created. Finding
/// it clears the stamp a refusal left.
pub fn select(id: &str) -> Result<PathBuf> {
    if !is_id(id) {
        bail!("{id:?} is not an integration id (lowercase [a-z0-9_-])");
    }
    let dir = spool_dir(id);
    if !dir.is_dir() {
        bail!(
            "the {id} integration does not match this version of funes. \
             Re-run `funes add {id}`, naming the memory it is bound to, to update it."
        );
    }
    let _ = std::fs::remove_file(missing_stamp(id));
    Ok(dir)
}

/// The stamp a refused `--harness <id>` leaves, `<root>/<id>.missing`: a file, which the sweep
/// (listing directories) never sees.
fn missing_stamp(id: &str) -> PathBuf {
    spool_root().join(format!("{id}.missing"))
}

/// Record that `id`'s spool was asked for and does not exist. Only a hook asks unattended, and only
/// a hook from an install older than the spool asks for one nothing writes, so the stamp is what
/// the read verbs report until `funes add {id}` creates the spool.
pub fn note_missing(id: &str) -> Result<()> {
    if !is_id(id) {
        bail!("{id:?} is not an integration id (lowercase [a-z0-9_-])");
    }
    std::fs::create_dir_all(spool_root())?;
    std::fs::write(missing_stamp(id), "")?;
    Ok(())
}

/// Drop the stamp `id`'s refusals left, if any: `funes add` has made the spool, or `funes remove`
/// has taken the hooks that asked for it, and neither leaves a spool to find it by.
pub fn forget_missing(id: &str) -> Result<()> {
    match std::fs::remove_file(missing_stamp(id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The ids asked for whose spool still does not exist, in id order.
pub fn missing() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(spool_root()) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".missing").map(str::to_string))
        .filter(|id| is_id(id) && !spool_dir(id).is_dir())
        .collect();
    ids.sort();
    ids
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
        assert!(
            !is_spool(Path::new("/elsewhere/pi")),
            "a directory elsewhere is not funes's"
        );
    }

    /// What funes owns is decided on resolved paths: a file reached through `..` or a symlink from
    /// inside the spool is someone else's, and a file in the spool is funes's however it is spelled.
    #[test]
    fn ownership_follows_the_resolved_path_not_its_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("spool");
        let mine = tmp.path().join("mine");
        std::fs::create_dir_all(root.join("pi")).unwrap();
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(root.join("pi/s.funes.jsonl"), "").unwrap();
        std::fs::write(mine.join("y.funes.jsonl"), "").unwrap();
        std::os::unix::fs::symlink(mine.join("y.funes.jsonl"), root.join("pi/link.funes.jsonl")).unwrap();

        assert!(is_under(&root, &root.join("pi/s.funes.jsonl")));
        assert!(is_under(&root, &root.join("pi/../pi/s.funes.jsonl")));
        assert!(!is_under(&root, &root.join("pi/../../mine/y.funes.jsonl")));
        assert!(!is_under(&root, &root.join("pi/link.funes.jsonl")));
        assert!(!is_under(&root, &root.join("pi/gone.funes.jsonl")), "nothing to own");
    }
}
