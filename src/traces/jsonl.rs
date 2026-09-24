//! Format-agnostic JSONL machinery: the recursive file walk and the workdir facet a recorded `cwd`
//! resolves to.

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

#[cfg(test)]
mod tests {
    use super::*;

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
