//! Copilot CLI's persisted event stream. SDK and IDE-hosted CLI sessions use the same format.
//! Lifecycle and streaming events are not conversation; only durable content enters memory.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde_json::Value;

use super::jsonl::workdir_of_cwd;
use super::source::{TraceSource, Unit};
use super::{Block, Turn};

struct EventTurn {
    turn: Turn,
    agent: String,
    fallback_call: Option<String>,
}

/// Parsed content and the raw cwd, which the source keeps for repository attribution.
pub struct Session {
    pub turns: Vec<Turn>,
    pub cwd: Option<String>,
}

fn string(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn text_block(kind: &str, text: String) -> Option<Block> {
    (!text.trim().is_empty()).then_some(Block {
        block_type: kind.into(),
        text,
        tool_name: None,
        tool_use_id: None,
    })
}

fn json_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

fn tool_block(call: &Value, name_key: &str) -> Block {
    Block {
        block_type: "tool_use".into(),
        text: json_text(call.get("arguments")),
        tool_name: call.get(name_key).and_then(Value::as_str).map(str::to_owned),
        tool_use_id: call.get("toolCallId").and_then(Value::as_str).map(str::to_owned),
    }
}

fn metadata_cwd(path: &Path) -> Option<String> {
    let file = File::open(path.parent()?.join("workspace.yaml")).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_reader(file).ok()?;
    value.get("cwd")?.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Stream a native `events.jsonl`, keeping parsed conversation but not the full event log.
/// A partial final write is ignored, like the other event-log parsers. I/O errors propagate so
/// the indexer never stamps an unreadable session as successfully indexed.
pub fn read_session(path: &Path) -> Result<Session> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut session_id = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_owned();
    let mut cwd = None;
    let mut events = Vec::new();
    let mut requested = HashSet::new();
    let mut names = HashMap::new();
    let mut seen_events = HashSet::new();
    for (ordinal, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event.get("ephemeral").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let kind = string(&event, "type");
        let data = &event["data"];
        if kind == "session.start" {
            if let Some(id) = data.get("sessionId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                session_id = id.into();
            }
        }
        if cwd.is_none()
            && matches!(
                kind.as_str(),
                "session.start" | "session.resume" | "session.context_changed"
            )
        {
            cwd = data
                .pointer("/context/cwd")
                .or_else(|| data.get("cwd"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
        }
        let agent = string(&event, "agentId");
        let mut blocks = Vec::new();
        let mut fallback_call = None;
        let role = match kind.as_str() {
            "user.message" => {
                blocks.extend(text_block("text", string(data, "content")));
                "user"
            }
            "assistant.message" => {
                blocks.extend(text_block("thinking", string(data, "reasoningText")));
                blocks.extend(text_block("text", string(data, "content")));
                if let Some(calls) = data.get("toolRequests").and_then(Value::as_array) {
                    for call in calls {
                        let id = string(call, "toolCallId");
                        if !id.is_empty() && requested.insert((agent.clone(), id.clone())) {
                            names.insert((agent.clone(), id), string(call, "name"));
                            blocks.push(tool_block(call, "name"));
                        }
                    }
                }
                "assistant"
            }
            "assistant.reasoning" => {
                blocks.extend(text_block("thinking", string(data, "content")));
                "assistant"
            }
            "tool.execution_start" => {
                let id = string(data, "toolCallId");
                if !id.is_empty() {
                    names
                        .entry((agent.clone(), id.clone()))
                        .or_insert_with(|| string(data, "toolName"));
                    blocks.push(tool_block(data, "toolName"));
                    fallback_call = Some(id);
                }
                "assistant"
            }
            "tool.execution_complete" => {
                // Keep one representation of the result, preferring the complete display text.
                // Opaque/binary content is not useful text and is not decoded.
                let result = &data["result"];
                let text = result.get("detailedContent").or_else(|| result.get("content"));
                let text = if text.is_some() {
                    json_text(text)
                } else if data.get("error").is_some() {
                    json_text(data.get("error"))
                } else if result.is_string() {
                    json_text(Some(result))
                } else {
                    result
                        .get("contents")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default()
                };
                if let Some(mut block) = text_block("tool_result", text) {
                    block.tool_use_id = data.get("toolCallId").and_then(Value::as_str).map(str::to_owned);
                    blocks.push(block);
                }
                "tool"
            }
            _ => continue,
        };
        if blocks.is_empty() {
            continue;
        }
        let id = event
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{session_id}-event-{ordinal}"));
        if !seen_events.insert((agent.clone(), id.clone())) {
            continue;
        }
        events.push(EventTurn {
            turn: Turn {
                session_id: String::new(),
                workdir: String::new(),
                turn_uuid: id,
                parent_uuid: event.get("parentId").and_then(Value::as_str).map(str::to_owned),
                seq: 0,
                ts: string(&event, "timestamp"),
                role: role.into(),
                blocks,
                source_path: path.to_string_lossy().into_owned(),
                harness: "copilot".into(),
            },
            agent,
            fallback_call,
        });
    }
    let cwd = cwd.or_else(|| metadata_cwd(path));
    let workdir = cwd.as_deref().and_then(workdir_of_cwd).unwrap_or_default();
    let mut fallback_seen = HashSet::new();
    let mut turns = Vec::new();
    for mut event in events {
        if let Some(call) = &event.fallback_call {
            let key = (event.agent.clone(), call.clone());
            if requested.contains(&key) || !fallback_seen.insert(key) {
                continue;
            }
        }
        for block in &mut event.turn.blocks {
            if block.block_type == "tool_result" {
                if let Some(call) = &block.tool_use_id {
                    block.tool_name = names
                        .get(&(event.agent.clone(), call.clone()))
                        .filter(|s| !s.is_empty())
                        .cloned();
                }
            }
        }
        event.turn.session_id.clone_from(&session_id);
        event.turn.workdir.clone_from(&workdir);
        event.turn.seq = turns.len() as i64;
        turns.push(event.turn);
    }
    Ok(Session { turns, cwd })
}

/// One session directory per unit. Artifacts and debug logs beside `events.jsonl` are excluded.
pub struct CopilotSource {
    root: PathBuf,
    limit: Option<usize>,
    cwds: RefCell<HashMap<String, Option<String>>>,
}

impl CopilotSource {
    pub fn new(root: PathBuf, limit: Option<usize>) -> Self {
        Self {
            root: root.canonicalize().unwrap_or(root),
            limit,
            cwds: RefCell::new(HashMap::new()),
        }
    }

    fn files(&self) -> Result<Vec<PathBuf>> {
        if self.root.is_file() {
            return Ok(if self.root.file_name().is_some_and(|n| n == "events.jsonl") {
                vec![self.root.clone()]
            } else {
                vec![]
            });
        }
        if self.root.join("events.jsonl").is_file() {
            return Ok(vec![self.root.join("events.jsonl")]);
        }
        let mut files = Vec::new();
        if !self.root.exists() {
            return Ok(files);
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let path = entry.path().join("events.jsonl");
                if path.is_file() {
                    files.push(path);
                }
            }
        }
        files.sort();
        Ok(files)
    }
}

fn signature(path: &Path) -> Option<String> {
    let stamp = |p: &Path| -> Option<String> {
        let md = std::fs::metadata(p).ok()?;
        Some(format!(
            "{}:{}",
            md.len(),
            md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos()
        ))
    };
    Some(format!(
        "{}:{}",
        stamp(path)?,
        stamp(&path.parent()?.join("workspace.yaml")).unwrap_or_default()
    ))
}

impl TraceSource for CopilotSource {
    fn describe(&self) -> String {
        format!("scanning copilot transcripts under {}", self.root.display())
    }

    fn units(&self) -> Result<Vec<Unit>> {
        let mut files = self.files()?;
        files.sort_by_cached_key(|p| {
            (
                std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH)),
                p.clone(),
            )
        });
        if let Some(limit) = self.limit {
            files.truncate(limit);
        }
        Ok(files
            .into_iter()
            .map(|p| Unit {
                signature: signature(&p),
                key: p.to_string_lossy().into_owned(),
                is_subagent: false,
            })
            .collect())
    }

    fn read(&self, unit: &Unit) -> Result<Vec<Turn>> {
        let path = Path::new(&unit.key);
        let before = signature(path);
        let session = read_session(path)?;
        anyhow::ensure!(
            before == signature(path),
            "Copilot session changed while reading; retry indexing"
        );
        self.cwds.borrow_mut().insert(unit.key.clone(), session.cwd);
        Ok(session.turns)
    }

    fn cwd(&self, unit: &Unit) -> Option<String> {
        self.cwds.borrow().get(&unit.key).cloned().flatten()
    }

    fn owns(&self, key: &str) -> bool {
        let path = Path::new(key);
        path.file_name().is_some_and(|n| n == "events.jsonl")
            && (path == self.root
                || path.parent() == Some(self.root.as_path())
                || path.parent().and_then(Path::parent) == Some(self.root.as_path()))
    }

    fn unit_keys(&self) -> Result<Vec<String>> {
        Ok(self
            .files()?
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn event(id: &str, kind: &str, data: Value) -> Value {
        json!({"id":id,"type":kind,"timestamp":"2026-09-11T00:00:00Z","parentId":"previous","data":data})
    }

    fn write(path: &Path, events: &[Value]) {
        let mut f = File::create(path).unwrap();
        for e in events {
            writeln!(f, "{e}").unwrap();
        }
    }

    #[test]
    fn durable_content_and_scoped_tool_correlation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut child = event(
            "child-start",
            "tool.execution_start",
            json!({"toolCallId":"c","toolName":"child_tool","arguments":{"x":1}}),
        );
        child["agentId"] = json!("child");
        let mut child_result = event(
            "child-result",
            "tool.execution_complete",
            json!({"toolCallId":"c","error":{"message":"failed"}}),
        );
        child_result["agentId"] = json!("child");
        write(
            &path,
            &[
                event(
                    "s",
                    "session.start",
                    json!({"sessionId":"native","context":{"cwd":"/work/repo"}}),
                ),
                event(
                    "u",
                    "user.message",
                    json!({"content":"Question", "transformedContent":"expanded prompt"}),
                ),
                event(
                    "a",
                    "assistant.message",
                    json!({"content":"Answer","reasoningText":"Reason","reasoningOpaque":"secret opaque","toolRequests":[{"toolCallId":"c","name":"bash","arguments":{"command":"test"}}]}),
                ),
                event(
                    "start",
                    "tool.execution_start",
                    json!({"toolCallId":"c","toolName":"bash","arguments":{"command":"test"}}),
                ),
                event(
                    "delta",
                    "assistant.message_delta",
                    json!({"deltaContent":"not persisted"}),
                ),
                event(
                    "result",
                    "tool.execution_complete",
                    json!({"toolCallId":"c","result":{"content":"brief","detailedContent":"full result"}}),
                ),
                child,
                child_result,
                event("unknown", "future.event", json!({"content":"ignore"})),
            ],
        );
        let session = read_session(&path).unwrap();
        assert_eq!(session.cwd.as_deref(), Some("/work/repo"));
        assert_eq!(session.turns.len(), 5);
        assert_eq!(session.turns[1].turn_uuid, "a");
        assert_eq!(session.turns[1].parent_uuid.as_deref(), Some("previous"));
        assert_eq!(session.turns[1].blocks.len(), 3);
        assert_eq!(session.turns[2].blocks[0].text, "full result");
        assert_eq!(session.turns[2].blocks[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(session.turns[4].blocks[0].tool_name.as_deref(), Some("child_tool"));
        assert!(session
            .turns
            .iter()
            .all(|t| t.session_id == "native" && t.harness == "copilot"));
    }

    #[test]
    fn partial_tail_resume_and_metadata_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(dir.path().join("workspace.yaml"), "cwd: /fallback\n").unwrap();
        let first = event("u", "user.message", json!({"content":"First"}));
        write(&path, std::slice::from_ref(&first));
        let before = read_session(&path).unwrap();
        assert_eq!(before.cwd.as_deref(), Some("/fallback"));
        let second = event("a", "assistant.message", json!({"content":"Second"}));
        write(&path, &[first, event("resume", "session.resume", json!({})), second]);
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{{\"type\":"
        )
        .unwrap();
        let after = read_session(&path).unwrap();
        assert_eq!(after.turns.len(), 2);
        assert_eq!(before.turns[0].turn_uuid, after.turns[0].turn_uuid);
        assert_eq!(before.turns[0].seq, after.turns[0].seq);
    }

    #[test]
    fn source_excludes_artifacts_limits_and_tracks_metadata() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["one", "two"] {
            std::fs::create_dir(dir.path().join(id)).unwrap();
            write(
                &dir.path().join(id).join("events.jsonl"),
                &[event("u", "user.message", json!({"content":"hello"}))],
            );
            std::fs::write(dir.path().join(id).join("debug.jsonl"), "{}").unwrap();
        }
        let source = CopilotSource::new(dir.path().to_owned(), Some(1));
        assert_eq!(source.unit_keys().unwrap().len(), 2);
        let unit = source.units().unwrap().remove(0);
        assert!(source.owns(&unit.key));
        assert!(!source.owns(&format!("{}/debug.jsonl", dir.path().display())));
        let p = Path::new(&unit.key);
        std::fs::write(p.parent().unwrap().join("workspace.yaml"), "cwd: /new\n").unwrap();
        assert_ne!(unit.signature, signature(p));
        assert_eq!(CopilotSource::new(p.to_owned(), None).units().unwrap().len(), 1);
        assert_eq!(
            CopilotSource::new(p.parent().unwrap().to_owned(), None)
                .units()
                .unwrap()
                .len(),
            1
        );
    }
}
