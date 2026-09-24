//! Format-agnostic JSONL machinery: the recursive file walk, the session id and workdir facet a
//! path or a recorded `cwd` resolves to, and the line-1 peek harness detection reads.

use serde_json::Value;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// All `*.jsonl` under `root`, recursively, sorted by path.
pub fn iter_jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort();
    files
}

/// A session id from a transcript file's stem.
pub fn session_id_of(p: &Path) -> String {
    p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
}

/// A Claude Code sub-agent session id: each sub-agent's transcript is named `agent-<hash>`, and
/// the turns file converted from it keeps that stem.
pub fn is_subagent(session_id: &str) -> bool {
    session_id.starts_with("agent-")
}

/// The workdir facet for a session's recorded working directory: the whole cwd munged the way
/// Claude Code names its workdir dirs — every non-alphanumeric character becomes `-` — so the
/// facet is unique per directory per host (no basename collisions) and matches the `projects`
/// segment Claude Code itself writes, meaning rows indexed before and after this derivation
/// agree. `None` when nothing of the cwd survives the munge (empty, or a bare `/`-ish root).
pub fn workdir_of_cwd(cwd: &str) -> Option<String> {
    let munged: String = cwd
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    munged.contains(|c: char| c != '-').then_some(munged)
}

/// The workdir facet of a session: its recorded `cwd` munged ([`workdir_of_cwd`]), else `fallback`.
pub fn workdir_facet(cwd: Option<&str>, fallback: &str) -> String {
    cwd.and_then(workdir_of_cwd).unwrap_or_else(|| fallback.to_string())
}

/// The first non-blank, parseable JSON record of a `*.jsonl` file — a cheap line-1 peek (for
/// harness detection and Codex's session id) that stops reading after the first record rather than
/// loading the whole file.
pub fn first_record(p: &Path) -> Option<Value> {
    let file = std::fs::File::open(p).ok()?;
    for line in std::io::BufReader::new(file).lines() {
        let line = line.ok()?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_file_stem() {
        assert_eq!(session_id_of(Path::new("/x/y/71626b12.jsonl")), "71626b12");
    }

    #[test]
    fn is_subagent_matches_the_agent_prefix() {
        assert!(is_subagent("agent-a90670a3067db59ca"));
        assert!(
            !is_subagent("72e856b6-9214-47ed-ab2d-0a1905093f45"),
            "a uuid is a top-level session"
        );
        assert!(!is_subagent("agentic-refactor"), "the prefix is `agent-`, not `agent`");
    }

    #[test]
    fn workdir_of_cwd_matches_claude_codes_project_dir_convention() {
        // The munge must reproduce the `projects` segment Claude Code itself writes, so old and
        // new rows of the same workdir share one facet.
        assert_eq!(
            workdir_of_cwd("/home/ubuntu/funes").as_deref(),
            Some("-home-ubuntu-funes")
        );
        assert_eq!(
            workdir_of_cwd("/home/ubuntu/llama.cpp").as_deref(),
            Some("-home-ubuntu-llama-cpp")
        );
        assert_eq!(
            workdir_of_cwd(r"C:\Users\d\dev\funes").as_deref(),
            Some("C--Users-d-dev-funes")
        );
        // Distinct directories can never share a facet, whatever their basenames.
        assert_ne!(workdir_of_cwd("/work/api"), workdir_of_cwd("/personal/api"));
    }

    #[test]
    fn workdir_of_cwd_rejects_empty_and_root_cwds() {
        assert_eq!(workdir_of_cwd(""), None);
        assert_eq!(workdir_of_cwd("/"), None);
        assert_eq!(workdir_of_cwd("///"), None);
        assert_eq!(workdir_of_cwd("  "), None);
    }
}
