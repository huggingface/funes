//! The coding agent a session came from: the [`Harness`] enum, its recorded facet value, the
//! `--harness` override parse, where each agent's sessions are read from, and detecting one from a
//! tree (a known session dir, else the first record's `type`). Nothing here parses JSONL.

use serde_json::Value;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::memory::dataset;

/// Which coding agent produced a session. Names its spool and its recorded `harness` facet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
    Pi,
    Hermes,
}

/// Session-dir tails funes recognizes, each with its harness. funes reads none of them — an
/// integration converts them — so recognizing one is how such a path is refused by name instead of
/// misread.
const KNOWN_DIRS: &[(&str, Harness)] = &[
    (".claude/projects", Harness::Claude),
    (".codex/sessions", Harness::Codex),
    (".pi/agent/sessions", Harness::Pi),
];

impl Harness {
    /// Every harness funes knows, in the order a no-arg sweep visits them.
    pub const ALL: [Harness; 4] = [Harness::Claude, Harness::Codex, Harness::Pi, Harness::Hermes];

    /// The stored facet value — matches the Hub's normalized `harness` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Harness::Claude => "claude_code",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Hermes => "hermes",
        }
    }

    /// The `--harness` spelling `index` accepts and shows in `--help`
    /// (`claude`/`codex`/`pi`/`hermes`). Differs from [`Harness::as_str`], the stored facet, only
    /// for Claude (facet `claude_code`).
    pub fn cli_name(&self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Hermes => "hermes",
        }
    }

    /// Parse a `--harness` override: `claude`/`claude_code`, `codex`, `pi`, or `hermes`.
    pub fn parse(s: &str) -> Result<Harness> {
        match s {
            "claude" | "claude_code" => Ok(Harness::Claude),
            "codex" => Ok(Harness::Codex),
            "pi" => Ok(Harness::Pi),
            "hermes" => Ok(Harness::Hermes),
            other => Err(anyhow!(
                "unknown harness {other:?} (expected claude, codex, pi, or hermes)"
            )),
        }
    }

    /// Detect a tree's harness: a known session dir wins; otherwise sniff the first record's
    /// `type` — Codex opens with `session_meta`, Pi with `session`. Claude has no positive
    /// first-line marker (line 1 may be a `summary`), so it is the fallback — as is an empty tree.
    pub fn detect(root: &Path, first_line: Option<&Value>) -> Harness {
        if let Some(h) = Self::from_known_dir(root) {
            return h;
        }
        match first_line.and_then(|v| v.get("type")).and_then(Value::as_str) {
            Some("session_meta") => Harness::Codex,
            Some("session") => Harness::Pi,
            _ => Harness::Claude,
        }
    }

    /// The harness for a path ending in a known session-dir tail (e.g. `~/.codex/sessions`), else
    /// `None`. A cheap tail match, so callers can skip walking the tree when the dir alone
    /// identifies the harness.
    pub fn from_known_dir(root: &Path) -> Option<Harness> {
        let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let s = canon.to_string_lossy();
        KNOWN_DIRS.iter().find(|(tail, _)| s.ends_with(tail)).map(|(_, h)| *h)
    }
}

/// Where a bundle writes the turns files funes indexes for it, one directory per registered id.
pub fn spool_dir(spools: &Path, h: Harness) -> PathBuf {
    spools.join(h.cli_name())
}

/// The directory holding every bundle's spool, `$FUNES_HOME/spool`.
pub fn spool_root() -> PathBuf {
    dataset::funes_dir().join("spool")
}

/// Whether `root` is a spool funes resolved for itself, rather than a path someone named.
pub fn is_spool(root: &Path) -> bool {
    root.starts_with(spool_root())
}

/// The `(root, harness)` pairs to index — drives a no-arg `funes index`. Every agent reaches funes
/// through the spool its integration converts into, so one with no spool contributes nothing.
pub fn known_harness_roots() -> Vec<(PathBuf, Harness)> {
    let spools = spool_root();
    Harness::ALL
        .into_iter()
        .map(|h| (spool_dir(&spools, h), h))
        .filter(|(dir, _)| dir.is_dir())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detect_prefers_known_dir() {
        assert_eq!(Harness::detect(Path::new("/x/.codex/sessions"), None), Harness::Codex);
        assert_eq!(Harness::detect(Path::new("/x/.pi/agent/sessions"), None), Harness::Pi);
        assert_eq!(Harness::detect(Path::new("/x/.claude/projects"), None), Harness::Claude);
    }

    #[test]
    fn detect_sniffs_first_line_for_unknown_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            Harness::detect(root, Some(&json!({"type": "session_meta"}))),
            Harness::Codex
        );
        assert_eq!(Harness::detect(root, Some(&json!({"type": "session"}))), Harness::Pi);
        // Claude's first line has no positive marker (here a summary), and an empty tree → Claude.
        assert_eq!(
            Harness::detect(root, Some(&json!({"type": "summary"}))),
            Harness::Claude
        );
        assert_eq!(Harness::detect(root, None), Harness::Claude);
    }

    #[test]
    fn parse_maps_aliases_and_rejects_unknown() {
        assert_eq!(Harness::parse("claude").unwrap(), Harness::Claude);
        assert_eq!(Harness::parse("claude_code").unwrap(), Harness::Claude);
        assert_eq!(Harness::parse("codex").unwrap(), Harness::Codex);
        assert_eq!(Harness::parse("pi").unwrap(), Harness::Pi);
        assert_eq!(Harness::parse("hermes").unwrap(), Harness::Hermes);
        assert!(Harness::parse("gpt").is_err());
    }

    /// Every agent is read from its spool, and one that has none is not a root at all.
    #[test]
    fn a_root_is_a_spool_or_nothing() {
        let spools = tempfile::tempdir().unwrap();
        // Named by the registered id, not the stored facet.
        let claude = spool_dir(spools.path(), Harness::Claude);
        assert_eq!(claude.file_name().unwrap(), "claude");
        assert!(!claude.is_dir(), "no spool, no root");
        // A spool funes resolved for itself is recognizable as one, so the `--harness` it carries
        // reads as funes's own rather than as a flag someone typed at a turns file.
        assert!(is_spool(&spool_dir(&spool_root(), Harness::Pi).join("s.funes.jsonl")));
        assert!(!is_spool(spools.path()), "a directory elsewhere is not funes's");
    }
}
