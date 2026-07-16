//! Format-agnostic JSONL machinery shared by the per-harness transcript parsers
//! ([`crate::claude_traces`], `codex_traces`, `pi_traces`): the recursive file walk, whole-file
//! read+parse, and tool-name back-fill. Each parser keeps only its record→[`Turn`] mapping.

use serde_json::Value;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::trace::Turn;

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

/// The project facet for a session's recorded working directory: the cwd's basename, so clones of
/// the same repo agree across hosts and teammates (`/Users/d/dev/funes` and `/home/u/funes` are
/// both `funes`). The cwd was written by whatever machine ran the session, so both separator
/// styles are split rather than trusting the local platform's path semantics. `None` when there
/// is no final component (an empty or root cwd — a Windows drive root like `C:\` counts, its
/// basename being the bare drive designator).
pub fn project_of_cwd(cwd: &str) -> Option<String> {
    let name = cwd.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next()?;
    (!name.is_empty() && !name.ends_with(':')).then(|| name.to_string())
}

/// Parse a `*.jsonl` file into one [`Value`] per non-blank, parseable line. A read failure
/// propagates as `Err` so the indexer skips the file *without recording state* — swallowing it
/// would silently mark an unreadable file fully indexed. A line that doesn't parse is dropped (a
/// transcript may carry a partial trailing write).
pub fn read_jsonl_records(p: &Path) -> std::io::Result<Vec<Value>> {
    let raw = std::fs::read(p)?;
    let content = String::from_utf8_lossy(&raw);
    let mut records: Vec<Value> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            records.push(v);
        }
    }
    Ok(records)
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

/// Fill each `tool_result` block's `tool_name` from the `tool_use` with the same `tool_use_id`,
/// scoped to one file's turns (last write wins). Formats that carry the tool name only on the call
/// (Claude, Codex) opt in; Pi carries it inline on the result and does not call this.
pub fn backfill_tool_names(turns: &mut [Turn]) {
    let mut tool_by_id: HashMap<String, Option<String>> = HashMap::new();
    for t in turns.iter() {
        for b in &t.blocks {
            if b.block_type == "tool_use" {
                if let Some(id) = &b.tool_use_id {
                    tool_by_id.insert(id.clone(), b.tool_name.clone());
                }
            }
        }
    }
    for t in turns.iter_mut() {
        for b in &mut t.blocks {
            if b.block_type == "tool_result" && b.tool_name.is_none() {
                if let Some(id) = &b.tool_use_id {
                    if let Some(name) = tool_by_id.get(id) {
                        b.tool_name = name.clone();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn session_id_is_file_stem() {
        assert_eq!(session_id_of(Path::new("/x/y/71626b12.jsonl")), "71626b12");
    }

    #[test]
    fn project_of_cwd_is_the_basename_on_any_host() {
        // The same clone dir name yields the same facet whichever machine recorded the cwd.
        assert_eq!(project_of_cwd("/Users/dcorvoysier/dev/funes").as_deref(), Some("funes"));
        assert_eq!(project_of_cwd("/home/ubuntu/funes").as_deref(), Some("funes"));
        assert_eq!(project_of_cwd(r"C:\Users\d\dev\funes").as_deref(), Some("funes"));
        assert_eq!(project_of_cwd("/home/ubuntu/funes/").as_deref(), Some("funes"));
    }

    #[test]
    fn project_of_cwd_rejects_empty_and_root_cwds() {
        assert_eq!(project_of_cwd(""), None);
        assert_eq!(project_of_cwd("/"), None);
        assert_eq!(project_of_cwd("///"), None);
        assert_eq!(project_of_cwd(r"C:\"), None);
        assert_eq!(project_of_cwd("C:/"), None);
    }

    #[test]
    fn read_jsonl_records_parses_valid_and_skips_blank_and_bad() {
        let mut f = tempfile::Builder::new().suffix(".jsonl").tempfile().unwrap();
        writeln!(f, r#"{{"a":1}}"#).unwrap();
        writeln!(f).unwrap();
        writeln!(f, "   ").unwrap();
        writeln!(f, "not json{{").unwrap();
        writeln!(f, r#"{{"b":2}}"#).unwrap();
        f.flush().unwrap();
        let recs = read_jsonl_records(f.path()).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["a"], 1);
        assert_eq!(recs[1]["b"], 2);
    }

    #[test]
    fn read_jsonl_records_missing_file_is_err() {
        // The indexer relies on Err to skip a file without recording it as fully indexed.
        assert!(read_jsonl_records(Path::new("/no/such/file.jsonl")).is_err());
    }

    #[test]
    fn backfill_tool_names_fills_from_matching_id() {
        let block = |bt: &str, name: Option<&str>, id: &str| crate::trace::Block {
            block_type: bt.into(),
            text: String::new(),
            tool_name: name.map(str::to_string),
            tool_use_id: Some(id.into()),
        };
        let turn = |uuid: &str, blocks: Vec<crate::trace::Block>| Turn {
            session_id: "s".into(),
            project: "p".into(),
            turn_uuid: uuid.into(),
            parent_uuid: None,
            seq: 0,
            ts: String::new(),
            role: "assistant".into(),
            blocks,
            source_path: String::new(),
            harness: "claude_code".into(),
        };
        let mut turns = vec![
            turn("t0", vec![block("tool_use", Some("Bash"), "call_1")]),
            turn(
                "t1",
                vec![
                    block("tool_result", None, "call_1"),
                    block("tool_result", None, "unknown"),
                ],
            ),
        ];
        backfill_tool_names(&mut turns);
        // The matching id fills the name; an unmatched id stays None.
        assert_eq!(turns[1].blocks[0].tool_name.as_deref(), Some("Bash"));
        assert_eq!(turns[1].blocks[1].tool_name, None);
    }
}
