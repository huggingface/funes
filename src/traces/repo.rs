//! Resolve the source repository a session belongs to: the `owner/name` of its checkout's git
//! remotes, from the working directory its transcript recorded.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Command;

/// The raw working directory a transcript recorded — top-level `cwd` (Claude, pi) or `payload.cwd`
/// (Codex) — from the first record that carries one. `None` if none does.
pub fn cwd_of_transcript(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(cwd) = v
            .get("cwd")
            .or_else(|| v.pointer("/payload/cwd"))
            .and_then(|c| c.as_str())
        {
            return Some(cwd.to_string());
        }
    }
    None
}

/// Normalize a git remote URL to its `owner/name` identity — scheme, host, and a trailing `.git`
/// stripped, taking the last two path segments. Handles ssh (`git@host:owner/name.git`) and https
/// (`https://host/owner/name`, `https://host/datasets/owner/name`). `None` without a path.
fn identity(url: &str) -> Option<String> {
    // Trailing slashes first, then `.git` — a `…/name.git/` URL must still shed `.git`.
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let path = if let Some((_, rest)) = u.split_once("://") {
        rest.split_once('/').map(|(_, p)| p)? // host/owner/name… → owner/name…
    } else if let Some((_, rest)) = u.split_once(':') {
        rest // ssh host:owner/name → owner/name
    } else {
        return None;
    };
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    (segs.len() >= 2).then(|| format!("{}/{}", segs[segs.len() - 2], segs[segs.len() - 1]))
}

/// The `owner/name` identities every git remote of the checkout at `cwd` names — space-joined,
/// deduped, sorted. Any remote counts, so a fork's `upstream` is included alongside its `origin`.
/// Empty when `cwd` isn't a resolvable git checkout (gone, not a repo, or git unavailable).
pub fn of_cwd(cwd: &str) -> String {
    let Ok(out) = Command::new("git").args(["-C", cwd, "remote", "-v"]).output() else {
        return String::new();
    };
    if !out.status.success() {
        return String::new();
    }
    let mut ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1)) // "<name>\t<url> (fetch|push)"
        .filter_map(identity)
        .collect();
    ids.sort();
    ids.dedup();
    ids.join(" ")
}

/// The repo identities for a session, resolved from its `transcript`'s recorded cwd. `""` when the
/// transcript records no cwd or the checkout can't be resolved.
pub fn of_transcript(path: &Path) -> String {
    cwd_of_transcript(path).map(|cwd| of_cwd(&cwd)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_normalizes_ssh_https_and_hf() {
        assert_eq!(
            identity("git@github.com:huggingface/funes.git").as_deref(),
            Some("huggingface/funes")
        );
        assert_eq!(
            identity("https://github.com/huggingface/funes").as_deref(),
            Some("huggingface/funes")
        );
        assert_eq!(
            identity("https://huggingface.co/datasets/acme/kb").as_deref(),
            Some("acme/kb"),
            "the HF datasets/ segment is dropped — identity is owner/name"
        );
        assert_eq!(
            identity("https://github.com/huggingface/funes.git/").as_deref(),
            Some("huggingface/funes"),
            "a trailing slash after .git must still shed .git"
        );
        assert_eq!(identity("garbage"), None);
    }

    #[test]
    fn cwd_reads_claude_top_level_and_codex_payload() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("c.jsonl");
        std::fs::write(&claude, "{\"cwd\":\"/w/claude\"}\n{\"cwd\":\"/other\"}\n").unwrap();
        assert_eq!(cwd_of_transcript(&claude).as_deref(), Some("/w/claude"));

        let codex = dir.path().join("x.jsonl");
        std::fs::write(
            &codex,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/w/codex\"}}\n",
        )
        .unwrap();
        assert_eq!(cwd_of_transcript(&codex).as_deref(), Some("/w/codex"));

        assert_eq!(cwd_of_transcript(&dir.path().join("missing.jsonl")), None);
    }

    #[test]
    fn of_cwd_lists_all_remotes_and_empty_when_not_git() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(["-C", dir.path().to_str().unwrap()])
                .args(args)
                .output()
                .unwrap();
        };
        // Not a git checkout yet.
        assert_eq!(of_cwd(dir.path().to_str().unwrap()), "");
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", "git@github.com:acme/widget.git"]);
        git(&["remote", "add", "upstream", "https://github.com/upstream/widget.git"]);
        assert_eq!(of_cwd(dir.path().to_str().unwrap()), "acme/widget upstream/widget");
        // A gone directory resolves to nothing, never an error.
        assert_eq!(of_cwd("/no/such/dir/anywhere"), "");
    }
}
