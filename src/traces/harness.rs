//! The coding agent a transcript came from: the [`Harness`] enum, its recorded facet value, the
//! `--harness` override parse, and detecting it from a session tree (a known session dir, else the
//! first record's `type`). `source`/`main`/the parsers call in here; nothing here parses JSONL.

use serde_json::Value;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::memory::dataset;

/// Which coding agent produced a transcript. Selects the parser and the recorded `harness` facet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
    Pi,
    Hermes,
}

/// Session-dir tails funes recognizes, each with its harness. Order also fixes the no-arg scan
/// order.
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

/// hermes' session store — a single SQLite file under `$HOME`, not a session dir like the others.
pub const HERMES_DB: &str = ".hermes/state.db";

/// Where each harness writes its own sessions, for those present on this machine.
fn native_roots_from(home: &Path, pi_agent_dir: Option<&Path>) -> Vec<(PathBuf, Harness)> {
    let mut roots: Vec<(PathBuf, Harness)> = KNOWN_DIRS
        .iter()
        .filter(|(_, h)| *h != Harness::Pi)
        .map(|(tail, h)| (home.join(tail), *h))
        .filter(|(dir, _)| dir.is_dir())
        .collect();

    let pi_sessions = pi_agent_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".pi/agent"))
        .join("sessions");
    if pi_sessions.is_dir() {
        roots.push((pi_sessions, Harness::Pi));
    }

    let hermes_db = home.join(HERMES_DB);
    if hermes_db.is_file() {
        roots.push((hermes_db, Harness::Hermes));
    }
    roots
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

/// A harness's spool when its bundle converts for it, else the store it writes natively.
fn harness_root(h: Harness, native: Option<PathBuf>, spools: &Path) -> Option<PathBuf> {
    let spool = spool_dir(spools, h);
    spool.is_dir().then_some(spool).or(native)
}

/// The `(root, harness)` pairs to index — drives a no-arg `funes index`. Each harness contributes
/// its spool if it has one, else the store it writes natively.
pub fn known_harness_roots() -> Vec<(PathBuf, Harness)> {
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => return Vec::new(),
    };
    let pi_agent_dir = std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from);
    let spools = spool_root();
    let native = native_roots_from(&home, pi_agent_dir.as_deref());
    Harness::ALL
        .into_iter()
        .filter_map(|h| {
            let own = native.iter().find(|(_, nh)| *nh == h).map(|(p, _)| p.clone());
            Some((harness_root(h, own, &spools)?, h))
        })
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

    /// A bundle's spool replaces the agent's own store as the root funes indexes, and a harness
    /// funes no longer parses is reachable through its spool alone.
    #[test]
    fn a_spool_stands_in_for_the_agents_own_store() {
        let spools = tempfile::tempdir().unwrap();
        let native = PathBuf::from("/home/u/.pi/agent/sessions");

        assert_eq!(
            harness_root(Harness::Pi, Some(native.clone()), spools.path()),
            Some(native.clone())
        );
        assert_eq!(harness_root(Harness::Pi, None, spools.path()), None);

        let pi_spool = spool_dir(spools.path(), Harness::Pi);
        std::fs::create_dir_all(&pi_spool).unwrap();
        assert_eq!(
            harness_root(Harness::Pi, Some(native), spools.path()),
            Some(pi_spool.clone())
        );
        assert_eq!(harness_root(Harness::Pi, None, spools.path()), Some(pi_spool.clone()));
        // Named by the id the bundle is registered under, not the stored facet.
        assert!(spool_dir(spools.path(), Harness::Claude).ends_with("claude"));

        assert!(is_spool(&spool_dir(&spool_root(), Harness::Pi).join("s.funes.jsonl")));
        assert!(!is_spool(&pi_spool), "a spool elsewhere is not funes's");
    }

    #[test]
    fn known_roots_honor_pi_coding_agent_dir() {
        let home = tempfile::tempdir().unwrap();
        let default_pi = home.path().join(".pi/agent/sessions");
        std::fs::create_dir_all(&default_pi).unwrap();

        let custom = tempfile::tempdir().unwrap();
        let custom_sessions = custom.path().join("sessions");
        std::fs::create_dir_all(&custom_sessions).unwrap();

        let roots = native_roots_from(home.path(), Some(custom.path()));

        assert!(roots.contains(&(custom_sessions, Harness::Pi)));
        assert!(!roots.contains(&(default_pi, Harness::Pi)));
    }

    #[test]
    fn known_roots_fall_back_to_default_pi_dir() {
        let home = tempfile::tempdir().unwrap();
        let default_pi = home.path().join(".pi/agent/sessions");
        std::fs::create_dir_all(&default_pi).unwrap();

        let roots = native_roots_from(home.path(), None);

        assert!(roots.contains(&(default_pi, Harness::Pi)));
    }
}
