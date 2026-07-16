//! Parse pi native session transcripts (`~/.pi/agent/sessions/*.jsonl`) into the shared
//! [`crate::trace`] turn/block model. pi writes one linked event per line; conversation lives in
//! `type:"message"` lines — a nested `message` with a typed `content` part list. Control events
//! (`session`/`model_change`/`thinking_level_change`) carry no turn. pi names its tool results
//! inline (`toolName`), so — unlike Claude/Codex — no `tool_use_id → name` back-fill is needed.

use serde_json::{Map, Value};
use std::path::Path;

use crate::jsonl;
use crate::trace::{Block, Turn};

/// The project a session's records name: the basename of the `session` line's `cwd`. `None` when
/// no record carries one.
pub fn project_from_records(records: &[Value]) -> Option<String> {
    records.iter().find_map(|r| {
        if r.get("type").and_then(Value::as_str) == Some("session") {
            r.get("cwd").and_then(Value::as_str).and_then(jsonl::project_of_cwd)
        } else {
            None
        }
    })
}

pub fn turns_from_jsonl_file(p: &Path, session_id: &str, fallback_project: &str) -> std::io::Result<Vec<Turn>> {
    let records = jsonl::read_jsonl_records(p)?;
    // The project facet is the basename of the session's recorded cwd, so the same clone names the
    // same project on every host; the path-derived fallback covers transcripts without one.
    let project = project_from_records(&records).unwrap_or_else(|| fallback_project.to_string());

    let mut turns = Vec::new();
    let mut seq = 0i64; // index among RETAINED turns, file order
    for rec in &records {
        let obj = match rec.as_object() {
            Some(o) => o,
            None => continue,
        };
        if obj.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let msg = match obj.get("message").and_then(Value::as_object) {
            Some(m) => m,
            None => continue,
        };
        let native_role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let blocks = normalize_blocks(native_role, msg);
        if blocks.is_empty() {
            continue;
        }
        // A tool result is its own role in pi; record it as `tool` to match the other parsers.
        let role = if native_role == "toolResult" {
            "tool"
        } else {
            native_role
        };
        turns.push(Turn {
            session_id: session_id.to_string(),
            project: project.to_string(),
            // The native line `id`/`parentId` — not the inner `toolCall.id` (that is the tool_use_id).
            turn_uuid: obj.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            parent_uuid: obj.get("parentId").and_then(Value::as_str).map(str::to_string),
            seq,
            ts: obj.get("timestamp").and_then(ts_string).unwrap_or_default(),
            role: role.to_string(),
            blocks,
            source_path: p.to_string_lossy().into_owned(),
            harness: "pi".into(),
        });
        seq += 1;
    }
    Ok(turns)
}

/// A line's `timestamp`, which pi writes as epoch milliseconds (a number) — stringified — or a
/// string; anything else yields `None`.
fn ts_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn normalize_blocks(role: &str, msg: &Map<String, Value>) -> Vec<Block> {
    match role {
        "user" => text_parts(msg),
        "assistant" => assistant_blocks(msg),
        "toolResult" => tool_result_block(msg).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The `text` parts of a message's `content` list, one block each (blank dropped).
fn text_parts(msg: &Map<String, Value>) -> Vec<Block> {
    let arr = match msg.get("content").and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|part| {
            let o = part.as_object()?;
            match o.get("type").and_then(Value::as_str) {
                Some("text") => text_like_block("text", o.get("text").and_then(Value::as_str).unwrap_or("")),
                _ => None,
            }
        })
        .collect()
}

/// An assistant message's typed content parts, in order: `thinking` → thinking block, `text` →
/// text block, `toolCall` → tool_use block. Blank text/thinking parts are dropped (pi's assistant
/// text is frequently just whitespace).
fn assistant_blocks(msg: &Map<String, Value>) -> Vec<Block> {
    let arr = match msg.get("content").and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut blocks = Vec::new();
    for part in arr {
        let o = match part.as_object() {
            Some(o) => o,
            None => continue,
        };
        match o.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                if let Some(b) = text_like_block("thinking", o.get("thinking").and_then(Value::as_str).unwrap_or("")) {
                    blocks.push(b);
                }
            }
            Some("text") => {
                if let Some(b) = text_like_block("text", o.get("text").and_then(Value::as_str).unwrap_or("")) {
                    blocks.push(b);
                }
            }
            Some("toolCall") => blocks.push(tool_use_block(o)),
            _ => {}
        }
    }
    blocks
}

/// A `text`/`thinking` block from non-blank text, else `None`.
fn text_like_block(block_type: &str, text: &str) -> Option<Block> {
    if text.trim().is_empty() {
        None
    } else {
        Some(Block {
            block_type: block_type.to_string(),
            text: text.to_string(),
            tool_name: None,
            tool_use_id: None,
        })
    }
}

/// A `toolCall` content part as a tool_use block. `arguments` is already an object; serialize it
/// compactly (source key order), defaulting a missing one to `{}` — matching the Claude parser.
fn tool_use_block(part: &Map<String, Value>) -> Block {
    let args = part
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let text = serde_json::to_string(&args).unwrap_or_else(|_| "{}".into());
    Block {
        block_type: "tool_use".to_string(),
        text,
        tool_name: part.get("name").and_then(Value::as_str).map(str::to_string),
        tool_use_id: part.get("id").and_then(Value::as_str).map(str::to_string),
    }
}

/// A `toolResult` message as a tool_result block; the name is inline (`toolName`) — no back-fill.
/// `None` if the flattened content is blank.
fn tool_result_block(msg: &Map<String, Value>) -> Option<Block> {
    let content = msg.get("content").cloned().unwrap_or(Value::Null);
    let text = flatten_tool_result(&content);
    if text.trim().is_empty() {
        None
    } else {
        Some(Block {
            block_type: "tool_result".to_string(),
            text,
            tool_name: msg.get("toolName").and_then(Value::as_str).map(str::to_string),
            tool_use_id: msg.get("toolCallId").and_then(Value::as_str).map(str::to_string),
        })
    }
}

/// Flatten a tool-result `content` — a string, or an array of `{type:"text", text}` parts — into
/// one string. Mirrors the helper in [`crate::claude_traces`]; kept local as pi is its only other
/// user for now.
fn flatten_tool_result(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let mut parts = Vec::new();
        for c in arr {
            if let Some(obj) = c.as_object() {
                if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
                    parts.push(obj.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string());
                }
            } else if let Some(s) = c.as_str() {
                parts.push(s.to_string());
            }
        }
        return parts.join("\n");
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".jsonl").tempfile().unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn parses_user_assistant_and_tool_result() {
        // Control lines are skipped. The assistant's blank text part is dropped, leaving thinking +
        // tool_use. turn_uuid/parent come from the top-level line id/parentId; the tool result names
        // its tool inline (no back-fill), and its role is normalized to `tool`.
        let f = write_jsonl(&[
            r#"{"type":"session","id":"sess","timestamp":1,"cwd":"/w","version":"1"}"#,
            r#"{"type":"model_change","id":"m","parentId":"sess","timestamp":2}"#,
            r#"{"type":"thinking_level_change","id":"tl","parentId":"m","timestamp":3}"#,
            r#"{"type":"message","id":"u1","parentId":"tl","timestamp":4,"message":{"role":"user","content":[{"type":"text","text":"how does this work"}]}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":5,"message":{"role":"assistant","content":[{"type":"thinking","thinking":"let me look","thinkingSignature":"x"},{"type":"text","text":"\n\n\n"},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"ls -la"}}]}}"#,
            r#"{"type":"message","id":"r1","parentId":"a1","timestamp":6,"message":{"role":"toolResult","content":[{"type":"text","text":"file.txt"}],"isError":false,"toolCallId":"call_1","toolName":"bash"}}"#,
        ]);
        let turns = turns_from_jsonl_file(f.path(), "sess", "proj").unwrap();
        assert_eq!(turns.len(), 3);

        let u = &turns[0];
        assert_eq!(u.role, "user");
        assert_eq!(u.seq, 0);
        assert_eq!(u.turn_uuid, "u1");
        assert_eq!(u.parent_uuid.as_deref(), Some("tl"));
        assert_eq!(u.ts, "4");
        assert_eq!(u.blocks.len(), 1);
        assert_eq!(u.blocks[0].text, "how does this work");

        let a = &turns[1];
        assert_eq!(a.role, "assistant");
        assert_eq!(a.turn_uuid, "a1");
        // blank text part dropped: thinking, then tool_use.
        let kinds: Vec<&str> = a.blocks.iter().map(|b| b.block_type.as_str()).collect();
        assert_eq!(kinds, vec!["thinking", "tool_use"]);
        assert_eq!(a.blocks[0].text, "let me look");
        let tool = &a.blocks[1];
        assert_eq!(tool.tool_name.as_deref(), Some("bash"));
        assert_eq!(tool.tool_use_id.as_deref(), Some("call_1"));
        assert_eq!(tool.text, r#"{"command":"ls -la"}"#);

        let r = &turns[2];
        assert_eq!(r.role, "tool");
        assert_eq!(r.turn_uuid, "r1");
        assert_eq!(r.blocks[0].block_type, "tool_result");
        assert_eq!(r.blocks[0].text, "file.txt");
        // name is inline, id from toolCallId.
        assert_eq!(r.blocks[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(r.blocks[0].tool_use_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn project_comes_from_the_session_line_cwd_with_path_fallback() {
        let msg = r#"{"type":"message","id":"u1","timestamp":1,"message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#;
        let mac = write_jsonl(&[
            r#"{"type":"session","id":"s","timestamp":1,"cwd":"/Users/d/dev/funes","version":"1"}"#,
            msg,
        ]);
        let linux = write_jsonl(&[
            r#"{"type":"session","id":"s","timestamp":1,"cwd":"/home/u/funes","version":"1"}"#,
            msg,
        ]);
        // The same clone names the same project on both hosts.
        assert_eq!(
            turns_from_jsonl_file(mac.path(), "s", "fb").unwrap()[0].project,
            "funes"
        );
        assert_eq!(
            turns_from_jsonl_file(linux.path(), "s", "fb").unwrap()[0].project,
            "funes"
        );
        // No recorded cwd → the path-derived fallback.
        let bare = write_jsonl(&[msg]);
        assert_eq!(turns_from_jsonl_file(bare.path(), "s", "fb").unwrap()[0].project, "fb");
    }

    #[test]
    fn skips_session_and_config_lines() {
        let f = write_jsonl(&[
            r#"{"type":"session","id":"s","timestamp":1}"#,
            r#"{"type":"model_change","id":"m","timestamp":2}"#,
        ]);
        assert!(turns_from_jsonl_file(f.path(), "s", "p").unwrap().is_empty());
    }

    #[test]
    fn assistant_blank_text_part_is_dropped_but_thinking_kept() {
        let f = write_jsonl(&[
            r#"{"type":"message","id":"a","timestamp":1,"message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"   "}]}}"#,
        ]);
        let turns = turns_from_jsonl_file(f.path(), "s", "p").unwrap();
        assert_eq!(turns.len(), 1);
        let kinds: Vec<&str> = turns[0].blocks.iter().map(|b| b.block_type.as_str()).collect();
        assert_eq!(kinds, vec!["thinking"]);
    }

    #[test]
    fn tool_result_name_is_inline_not_backfilled() {
        // A toolResult with no matching toolCall still gets its name from `toolName` — proving there
        // is no correlation pass.
        let f = write_jsonl(&[
            r#"{"type":"message","id":"r","timestamp":1,"message":{"role":"toolResult","content":[{"type":"text","text":"out"}],"toolCallId":"orphan","toolName":"grep"}}"#,
        ]);
        let turns = turns_from_jsonl_file(f.path(), "s", "p").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].blocks[0].tool_name.as_deref(), Some("grep"));
    }

    #[test]
    fn toolcall_missing_arguments_defaults_to_empty_object() {
        let f = write_jsonl(&[
            r#"{"type":"message","id":"a","timestamp":1,"message":{"role":"assistant","content":[{"type":"toolCall","id":"c","name":"ls"}]}}"#,
        ]);
        let turns = turns_from_jsonl_file(f.path(), "s", "p").unwrap();
        assert_eq!(turns[0].blocks[0].text, "{}");
    }

    #[test]
    fn flatten_tool_result_handles_string_and_array() {
        assert_eq!(flatten_tool_result(&Value::String("plain".into())), "plain");
        let arr = serde_json::json!([{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]);
        assert_eq!(flatten_tool_result(&arr), "a\nb");
        assert_eq!(flatten_tool_result(&Value::Null), "");
    }

    #[test]
    fn missing_file_is_an_error_not_empty() {
        assert!(turns_from_jsonl_file(Path::new("/no/such/file.jsonl"), "s", "p").is_err());
    }
}
