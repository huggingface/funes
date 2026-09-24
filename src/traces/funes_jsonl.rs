//! The `.funes.jsonl` source: turns already in funes's own shape, written by a producer funes has
//! no parser for (`docs/funes-jsonl.md`). A file is one unit; a directory of them is one unit per
//! file, each stamped so an unchanged one is skipped. A line is read with serde and validated,
//! never coerced: one invalid line rejects its file.

use super::jsonl;
use super::source::{file_sig, TraceSource, Unit};
use super::{Turn, BLOCK_TYPES};
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const SUFFIX: &str = ".funes.jsonl";

/// Whether `path` names a turns file.
pub fn is_turns_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(SUFFIX))
}

/// A turns file, or a directory of them. `listing` is every `.jsonl` under `path` (the file itself
/// for a file), listed by the caller; `limit` keeps the most recent N files by mtime.
pub struct FunesJsonl {
    path: PathBuf,
    listing: Vec<PathBuf>,
    limit: Option<usize>,
}

impl FunesJsonl {
    pub fn new(path: &Path, listing: Vec<PathBuf>, limit: Option<usize>) -> FunesJsonl {
        FunesJsonl {
            path: path.to_path_buf(),
            listing,
            limit,
        }
    }

    /// The unit files — all of the listing, which must be turns files, or the directory is
    /// ambiguous and rejected.
    fn files(&self) -> Result<&[PathBuf]> {
        if let Some(other) = self.listing.iter().find(|p| !is_turns_file(p)) {
            bail!(
                "{} is not a turns directory: {} is not a `{SUFFIX}` file",
                self.path.display(),
                other.display()
            );
        }
        Ok(&self.listing)
    }
}

impl TraceSource for FunesJsonl {
    fn describe(&self) -> String {
        if self.path.is_dir() {
            format!("indexing funes JSONL files under {}", self.path.display())
        } else {
            format!("indexing funes JSONL {}", self.path.display())
        }
    }

    fn units(&self) -> Result<Vec<Unit>> {
        let mut files = self.files()?.to_vec();
        files.sort_by_cached_key(|p| {
            std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH))
        });
        if let Some(n) = self.limit {
            files.truncate(n);
        }
        // A directory is a store funes revisits, so its files are stamped; a file named on its own
        // is re-read every time, and chunk-id dedup makes that a no-op.
        let stamped = self.path.is_dir();
        let mut units: Vec<Unit> = files
            .into_iter()
            .map(|p| Unit {
                signature: stamped.then(|| file_sig(&p)).flatten(),
                is_subagent: jsonl::is_subagent(&jsonl::session_id_of(&p)),
                key: p.to_string_lossy().into_owned(),
            })
            .collect();
        // Subagents last (stable sort preserves recency within each group), so a budgeted run
        // spends its time on the sessions a person held before the ones an agent spawned.
        units.sort_by_key(|u| u.is_subagent);
        Ok(units)
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        read_turns(Path::new(&unit.key))
    }

    /// A lone file is the run; in a directory each file stands alone.
    fn fatal_on_read_error(&self) -> bool {
        !self.path.is_dir()
    }

    fn owns(&self, key: &str) -> bool {
        Path::new(key).starts_with(&self.path)
    }

    fn unit_keys(&self) -> Result<Vec<String>> {
        Ok(self.files()?.iter().map(|p| p.to_string_lossy().into_owned()).collect())
    }
}

/// Every turn of a turns file, validated and stamped with its `source_path` and the `workdir` its
/// `cwd` derives to. `Err` names the first bad line.
pub fn read_turns(path: &Path) -> Result<Vec<Turn>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let source_path = path.to_string_lossy().into_owned();
    let mut turns = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let at = |e: String| anyhow!("{}:{}: {e}", path.display(), i + 1);
        let mut turn: Turn = serde_json::from_str(line).map_err(|e| at(e.to_string()))?;
        validate(&turn).map_err(|e| at(e.to_string()))?;
        turn.source_path = source_path.clone();
        turn.workdir = jsonl::workdir_facet(turn.cwd.as_deref(), "");
        turns.push(turn);
    }
    Ok(turns)
}

/// The rules serde cannot express (`docs/funes-jsonl.md`, "Validation").
fn validate(t: &Turn) -> Result<()> {
    for (field, v) in [("session_id", &t.session_id), ("turn_uuid", &t.turn_uuid)] {
        if v.contains(':') {
            bail!("{field} {v:?} contains `:`");
        }
    }
    let harness_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
    if t.harness.is_empty() || !t.harness.chars().all(harness_char) {
        bail!("harness {:?} is not [a-z0-9_-]", t.harness);
    }
    if !t.ts.ends_with('Z') || chrono::DateTime::parse_from_rfc3339(&t.ts).is_err() {
        bail!("ts {:?} is not RFC 3339 UTC (`Z`)", t.ts);
    }
    for (i, b) in t.blocks.iter().enumerate() {
        if !BLOCK_TYPES.contains(&b.block_type.as_str()) {
            bail!("blocks[{i}].block_type {:?} is unknown", b.block_type);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{self, Tier};
    use crate::traces::Block;

    const LINE: &str = r#"{"session_id":"s","turn_uuid":"t-1","seq":0,"ts":"2026-09-18T09:41:07Z","role":"user","harness":"opencode","cwd":"/home/me/x","blocks":[{"block_type":"text","text":"hi"}]}"#;

    fn write(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    fn read_line(dir: &Path, line: &str) -> Result<Vec<Turn>> {
        read_turns(&write(dir, "t.funes.jsonl", &[line]))
    }

    fn source(path: &Path) -> FunesJsonl {
        FunesJsonl::new(path, jsonl::iter_jsonl_files(path), None)
    }

    #[test]
    fn limit_keeps_the_most_recent_files_and_leaves_the_listing_whole() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.funes.jsonl", "b.funes.jsonl", "c.funes.jsonl"] {
            write(dir.path(), name, &[LINE]);
        }
        let capped = FunesJsonl::new(dir.path(), jsonl::iter_jsonl_files(dir.path()), Some(2));
        assert_eq!(capped.units().unwrap().len(), 2);
        assert_eq!(capped.unit_keys().unwrap().len(), 3);
    }

    #[test]
    fn a_sub_agents_session_is_ordered_last() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "agent-x.funes.jsonl",
            "s-a.funes.jsonl",
            "agent-y.funes.jsonl",
            "s-b.funes.jsonl",
        ] {
            write(dir.path(), name, &[LINE]);
        }
        let units = source(dir.path()).units().unwrap();
        let first_sub = units.iter().position(|u| u.is_subagent).expect("has a sub-agent unit");
        assert!(units[first_sub..].iter().all(|u| u.is_subagent), "whatever the mtimes");
        assert_eq!(units.iter().filter(|u| u.is_subagent).count(), 2);
    }

    #[test]
    fn a_valid_line_is_stamped_with_source_path_and_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "t.funes.jsonl", &[LINE]);
        let turns = read_turns(&p).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].source_path, p.to_string_lossy());
        assert_eq!(turns[0].workdir, "-home-me-x");
        assert_eq!(turns[0].harness, "opencode");
    }

    #[test]
    fn no_cwd_means_no_workdir_facet() {
        let dir = tempfile::tempdir().unwrap();
        let turns = read_line(dir.path(), &LINE.replace(r#""cwd":"/home/me/x","#, "")).unwrap();
        assert_eq!(turns[0].cwd, None);
        assert_eq!(turns[0].workdir, "");
    }

    #[test]
    fn every_rule_rejects_and_names_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let bad = [
            (r#""session_id":"s""#, r#""session_id":"a:b""#, "contains `:`"),
            (r#""turn_uuid":"t-1""#, r#""turn_uuid":"a:b""#, "contains `:`"),
            (r#""harness":"opencode""#, r#""harness":"OpenCode""#, "[a-z0-9_-]"),
            (r#""harness":"opencode""#, r#""harness":"""#, "[a-z0-9_-]"),
            (
                r#""ts":"2026-09-18T09:41:07Z""#,
                r#""ts":"2026-09-18T09:41:07+00:00""#,
                "RFC 3339",
            ),
            (r#""ts":"2026-09-18T09:41:07Z""#, r#""ts":"yesterdayZ""#, "RFC 3339"),
            (r#""block_type":"text""#, r#""block_type":"image""#, "unknown"),
            (r#""seq":0,"#, "", "missing field"),
            (r#""seq":0,"#, r#""seq":0,"extra":1,"#, "unknown field"),
            (r#""seq":0,"#, r#""seq":0,"format":2,"#, "unknown format"),
        ];
        for (from, to, reason) in bad {
            let line = LINE.replace(from, to);
            assert_ne!(line, LINE, "the substitution {from} → {to} did nothing");
            let err = read_turns(&write(dir.path(), "t.funes.jsonl", &[LINE, &line]))
                .unwrap_err()
                .to_string();
            assert!(err.contains(reason), "{to}: {err}");
            assert!(err.contains("t.funes.jsonl:2:"), "{to}: {err}");
        }
        // A blank line is not a turn either.
        let err = read_turns(&write(dir.path(), "t.funes.jsonl", &[LINE, "", LINE])).unwrap_err();
        assert!(err.to_string().contains("t.funes.jsonl:2:"), "{err}");
    }

    /// A native transcript may record a tool call with no name, and funes renders that block
    /// `[tool_use None] …`. The format carries it: refusing it would leave a producer unable to
    /// reproduce a session funes already indexed, and dropping the block would renumber the turn.
    #[test]
    fn a_tool_use_without_a_name_keeps_its_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let parsed = Turn {
            format: crate::traces::FORMAT_VERSION,
            session_id: "s".into(),
            cwd: None,
            workdir: String::new(),
            turn_uuid: "t-1".into(),
            parent_uuid: None,
            seq: 0,
            ts: "2026-09-18T09:41:07Z".into(),
            role: "assistant".into(),
            blocks: vec![Block {
                block_type: "tool_use".into(),
                text: "{}".into(),
                tool_name: None,
                tool_use_id: Some("call_1".into()),
            }],
            source_path: String::new(),
            harness: "opencode".into(),
        };
        let line = serde_json::to_string(&parsed).unwrap();
        let turns = read_line(dir.path(), &line).unwrap();
        assert_eq!(turns[0].blocks[0].tool_name, None);

        let chunks = |t: &[Turn]| -> Vec<(String, String)> {
            chunk::chunks_from_turns(t, &Tier::ALL, true)
                .into_iter()
                .map(|c| (c.id, c.text))
                .collect()
        };
        assert_eq!(chunks(&turns), chunks(std::slice::from_ref(&parsed)));
        assert_eq!(chunks(&turns)[0].1, "[tool_use None] {}");
    }

    #[test]
    fn a_file_is_one_fatal_unit_and_a_directory_one_best_effort_unit_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a.funes.jsonl", &[LINE]);
        write(dir.path(), "b.funes.jsonl", &[LINE]);
        write(dir.path(), "notes.txt", &["ignored"]);

        let file = source(&a);
        let units = file.units().unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].signature.is_none());
        assert!(file.fatal_on_read_error());
        assert!(file.owns(&units[0].key));

        let tree = source(dir.path());
        let units = tree.units().unwrap();
        assert_eq!(units.len(), 2);
        assert!(units.iter().all(|u| u.signature.is_some()));
        assert!(!tree.fatal_on_read_error());
        let b = units.iter().find(|u| u.key.ends_with("b.funes.jsonl")).unwrap();
        assert!(tree.owns(&b.key) && !file.owns(&b.key));
        assert_eq!(tree.read(b).unwrap().len(), 1);
    }

    #[test]
    fn a_directory_holding_another_jsonl_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.funes.jsonl", &[LINE]);
        write(dir.path(), "session.jsonl", &["{}"]);
        let err = source(dir.path()).units().err().expect("rejected").to_string();
        assert!(err.contains("session.jsonl"), "{err}");
    }
}
