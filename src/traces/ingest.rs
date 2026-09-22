//! Validated, versioned JSONL supplied by external harness integrations.

use super::{Block, Turn};
use crate::traces::source::{TraceSource, Unit};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Read};

const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u64,
    harness: String,
    session_id: String,
    cwd: String,
    turns: Vec<InputTurn>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputTurn {
    turn_uuid: String,
    parent_uuid: Option<String>,
    seq: i64,
    ts: String,
    role: String,
    blocks: Vec<InputBlock>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputBlock {
    block_type: String,
    text: String,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
}

/// A fully validated external import. Construction reads and validates the whole stream, so an
/// invalid later line cannot leave an earlier session committed to memory.
pub struct IngestSource {
    label: String,
    sessions: BTreeMap<String, Vec<Turn>>,
}

impl IngestSource {
    /// Read and validate versioned session JSONL, rejecting input over 64 MiB.
    /// `label` identifies the source in progress messages and read errors.
    pub fn read(reader: impl BufRead, label: impl Into<String>) -> Result<Self> {
        let label = label.into();
        let mut bytes = Vec::new();
        reader
            .take(MAX_INPUT_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {label}"))?;
        if bytes.len() as u64 > MAX_INPUT_BYTES {
            bail!("ingest input exceeds 64 MiB");
        }
        let text = std::str::from_utf8(&bytes).context("ingest input is not UTF-8")?;
        let mut sessions = BTreeMap::new();
        for (line_index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                bail!("line {} is empty", line_index + 1);
            }
            let envelope: Envelope =
                serde_json::from_str(line).with_context(|| format!("invalid JSON on line {}", line_index + 1))?;
            let (id, turns) =
                validate(envelope).with_context(|| format!("invalid session on line {}", line_index + 1))?;
            if sessions.insert(id, turns).is_some() {
                bail!("duplicate session on line {}", line_index + 1);
            }
        }
        Ok(Self { label, sessions })
    }

    /// Whether no session envelopes were supplied, so callers can skip model and memory setup.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

fn validate(input: Envelope) -> Result<(String, Vec<Turn>)> {
    if input.version != 1 {
        bail!("unsupported ingest version (expected 1)");
    }
    validate_id("harness", &input.harness)?;
    let harness = crate::traces::harness::normalize_ingest_harness(&input.harness)?;
    validate_id("session_id", &input.session_id)?;
    if input.session_id.contains(':') {
        bail!("session_id must not contain ':'");
    }
    if input.cwd.len() > 4096 || !std::path::Path::new(&input.cwd).is_absolute() {
        bail!("cwd must be an absolute path of at most 4096 bytes");
    }
    let workdir = crate::traces::jsonl::workdir_of_cwd(&input.cwd)
        .ok_or_else(|| anyhow!("cwd must identify a working directory"))?;
    let session_id = format!("{harness}:{}", input.session_id);
    let mut ids = HashSet::new();
    let mut previous_seq = None;
    let mut turns = Vec::with_capacity(input.turns.len());
    for turn in input.turns {
        validate_id("turn_uuid", &turn.turn_uuid)?;
        if !ids.insert(turn.turn_uuid.clone()) {
            bail!("duplicate turn_uuid");
        }
        if previous_seq.is_some_and(|seq| turn.seq <= seq) {
            bail!("turn seq values must be strictly increasing");
        }
        if turn.seq < 0 {
            bail!("turn seq values must be nonnegative");
        }
        previous_seq = Some(turn.seq);
        // UTC at fixed precision, so string order is chronological order for `sessions`.
        let ts = chrono::DateTime::parse_from_rfc3339(&turn.ts)
            .context("invalid timestamp")?
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if !matches!(turn.role.as_str(), "user" | "assistant" | "tool") {
            bail!("role must be user, assistant, or tool");
        }
        if let Some(parent) = &turn.parent_uuid {
            validate_id("parent_uuid", parent)?;
        }
        let mut blocks = Vec::with_capacity(turn.blocks.len());
        for block in turn.blocks {
            if !matches!(
                block.block_type.as_str(),
                "text" | "thinking" | "tool_use" | "tool_result"
            ) {
                bail!("block_type must be text, thinking, tool_use, or tool_result");
            }
            if let Some(name) = &block.tool_name {
                validate_id("tool_name", name)?;
            }
            if let Some(id) = &block.tool_use_id {
                validate_id("tool_use_id", id)?;
            }
            blocks.push(Block {
                block_type: block.block_type,
                text: block.text,
                tool_name: block.tool_name,
                tool_use_id: block.tool_use_id,
            });
        }
        turns.push(Turn {
            format: crate::traces::FORMAT_VERSION,
            session_id: session_id.clone(),
            cwd: Some(input.cwd.clone()),
            workdir: workdir.clone(),
            turn_uuid: format!("{harness}:{}", turn.turn_uuid),
            parent_uuid: turn.parent_uuid.map(|id| format!("{harness}:{id}")),
            seq: turn.seq,
            ts,
            role: turn.role,
            blocks,
            source_path: format!("ingest:{session_id}"),
            harness: harness.clone(),
        });
    }
    Ok((session_id, turns))
}

fn validate_id(field: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        bail!("{field} must be 1..=1024 bytes without control characters");
    }
    Ok(())
}

impl TraceSource for IngestSource {
    fn describe(&self) -> String {
        format!("ingesting external sessions from {}", self.label)
    }

    fn units(&self) -> Result<Vec<Unit>> {
        Ok(self
            .sessions
            .keys()
            .map(|id| Unit {
                key: format!("ingest:{id}"),
                signature: None,
                is_subagent: false,
            })
            .collect())
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        unit.key
            .strip_prefix("ingest:")
            .and_then(|id| self.sessions.get(id))
            .cloned()
            .ok_or_else(|| anyhow!("unknown ingest unit {:?}", unit.key))
    }

    fn fatal_on_read_error(&self) -> bool {
        true
    }

    fn unit_keys(&self) -> Result<Vec<String>> {
        Ok(self.units()?.into_iter().map(|unit| unit.key).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const VALID: &str = r#"{"version":1,"harness":"opencode","session_id":"ses_1","cwd":"/work","turns":[{"turn_uuid":"msg_1","parent_uuid":null,"seq":0,"ts":"2026-09-17T12:00:00Z","role":"user","blocks":[{"block_type":"text","text":"hello","tool_name":null,"tool_use_id":null}]}]}"#;

    #[test]
    fn reads_namespaced_external_session() {
        let source = IngestSource::read(Cursor::new(VALID), "stdin").unwrap();
        let unit = source.units().unwrap().pop().unwrap();
        let turns = source.read(&unit).unwrap();
        assert_eq!(unit.key, "ingest:opencode:ses_1");
        assert_eq!(turns[0].session_id, "opencode:ses_1");
        assert_eq!(turns[0].turn_uuid, "opencode:msg_1");
        assert_eq!(turns[0].harness, "opencode");
    }

    #[test]
    fn rejects_later_invalid_session_before_exposing_units() {
        let input = format!("{VALID}\n{}\n", VALID.replace("\"version\":1", "\"version\":2"));
        let error = IngestSource::read(Cursor::new(input), "stdin").err().unwrap();
        assert!(error.to_string().contains("line 2"), "{error:#}");
    }

    #[test]
    fn rejects_values_outside_the_wire_contract() {
        for invalid in [
            VALID.replace("\"role\":\"user\"", "\"role\":\"system\""),
            VALID.replace("\"block_type\":\"text\"", "\"block_type\":\"image\""),
            VALID.replace("2026-09-17T12:00:00Z", "not-an-rfc3339-timestamp"),
            VALID.replace("\"seq\":0", "\"seq\":-1"),
            VALID.replace("\"cwd\":\"/work\"", "\"cwd\":\"relative/work\""),
            VALID.replace("\"cwd\":\"/work\"", &format!("\"cwd\":\"/{}\"", "w".repeat(4096))),
            VALID.replace(
                "\"harness\":\"opencode\"",
                &format!("\"harness\":\"{}\"", "o".repeat(1025)),
            ),
            VALID.replace("\"version\":1", "\"version\":1,\"extra\":true"),
            VALID.replace("\"seq\":0", "\"seq\":0,\"extra\":true"),
            VALID.replace("\"text\":\"hello\"", "\"text\":\"hello\",\"extra\":true"),
        ] {
            assert!(
                IngestSource::read(Cursor::new(invalid), "stdin").is_err(),
                "accepted invalid input"
            );
        }
    }

    #[test]
    fn normalizes_workdir_and_preserves_tool_metadata() {
        let input = VALID
            .replace("\"block_type\":\"text\"", "\"block_type\":\"tool_use\"")
            .replace("\"tool_name\":null", "\"tool_name\":\"bash\"")
            .replace("\"tool_use_id\":null", "\"tool_use_id\":\"call_1\"");
        let source = IngestSource::read(Cursor::new(input), "stdin").unwrap();
        let turn = source.read(&source.units().unwrap()[0]).unwrap().remove(0);
        assert_eq!(turn.format, crate::traces::FORMAT_VERSION);
        assert_eq!(turn.cwd.as_deref(), Some("/work"));
        assert_eq!(turn.workdir, "-work");
        assert_eq!(turn.blocks[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(turn.blocks[0].tool_use_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn normalizes_timestamps_to_sortable_utc() {
        let input = VALID.replace("2026-09-17T12:00:00Z", "2026-09-17T01:30:00.5+05:00");
        let source = IngestSource::read(Cursor::new(input), "stdin").unwrap();
        let turn = source.read(&source.units().unwrap()[0]).unwrap().remove(0);
        assert_eq!(turn.ts, "2026-09-16T20:30:00.500Z");
    }

    #[test]
    fn accepts_colons_outside_session_ids() {
        let input = VALID
            .replace("\"turn_uuid\":\"msg_1\"", "\"turn_uuid\":\"turn:1\"")
            .replace("\"parent_uuid\":null", "\"parent_uuid\":\"parent:1\"")
            .replace("\"block_type\":\"text\"", "\"block_type\":\"tool_use\"")
            .replace("\"tool_name\":null", "\"tool_name\":\"shell:local\"")
            .replace("\"tool_use_id\":null", "\"tool_use_id\":\"call:1\"");
        let source = IngestSource::read(Cursor::new(input), "stdin").unwrap();
        let turn = source.read(&source.units().unwrap()[0]).unwrap().remove(0);
        assert_eq!(turn.turn_uuid, "opencode:turn:1");
        assert_eq!(turn.parent_uuid.as_deref(), Some("opencode:parent:1"));
        assert_eq!(turn.blocks[0].tool_name.as_deref(), Some("shell:local"));
        assert_eq!(turn.blocks[0].tool_use_id.as_deref(), Some("call:1"));
    }

    #[test]
    fn rejects_input_over_64_mib() {
        let input = vec![b' '; MAX_INPUT_BYTES as usize + 1];
        assert!(IngestSource::read(Cursor::new(input), "stdin").is_err());
    }

    #[test]
    fn rejects_duplicate_session_and_turn_identities_within_input() {
        let duplicate_session = format!("{VALID}\n{VALID}\n");
        assert!(IngestSource::read(Cursor::new(duplicate_session), "stdin").is_err());

        let mut value: serde_json::Value = serde_json::from_str(VALID).unwrap();
        let mut same_id = value["turns"][0].clone();
        same_id["seq"] = 1.into();
        value["turns"].as_array_mut().unwrap().push(same_id);
        assert!(IngestSource::read(Cursor::new(serde_json::to_string(&value).unwrap()), "stdin").is_err());

        let mut value: serde_json::Value = serde_json::from_str(VALID).unwrap();
        let mut same_seq = value["turns"][0].clone();
        same_seq["turn_uuid"] = "msg_2".into();
        value["turns"].as_array_mut().unwrap().push(same_seq);
        assert!(IngestSource::read(Cursor::new(serde_json::to_string(&value).unwrap()), "stdin").is_err());
    }
}
