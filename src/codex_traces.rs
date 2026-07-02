//! Parse Codex native rollout transcripts (`~/.codex/sessions/rollout-*.jsonl`) into the shared
//! [`crate::trace`] turn/block model. Codex writes the OpenAI Responses "rollout" format: one
//! `{timestamp, type, payload}` envelope per line. Only `response_item` lines carry conversation
//! (`session_meta`/`event_msg`/`turn_context` are session metadata and telemetry); each retained
//! item becomes its own turn, keyed `session_id-seq` so re-reads of a grown log stay id-stable.

use serde_json::{Map, Value};
use std::path::Path;

use crate::jsonl;
use crate::trace::{Block, Turn};

pub fn turns_from_jsonl_file(p: &Path, session_id: &str, project: &str) -> std::io::Result<Vec<Turn>> {
    let records = jsonl::read_jsonl_records(p)?;

    let mut turns = Vec::new();
    let mut seq = 0i64; // index among RETAINED turns, file order
    for rec in &records {
        let obj = match rec.as_object() {
            Some(o) => o,
            None => continue,
        };
        if obj.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let payload = match obj.get("payload").and_then(Value::as_object) {
            Some(p) => p,
            None => continue,
        };
        let (role, blocks): (String, Vec<Block>) = match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => (
                payload.get("role").and_then(Value::as_str).unwrap_or("").to_string(),
                message_blocks(payload),
            ),
            "reasoning" => ("assistant".to_string(), reasoning_block(payload).into_iter().collect()),
            "function_call" | "custom_tool_call" => ("assistant".to_string(), vec![tool_use_block(payload)]),
            "function_call_output" | "custom_tool_call_output" => {
                ("tool".to_string(), tool_result_block(payload).into_iter().collect())
            }
            // tool_schema (the tools list), image_generation_call, and anything else carry no turn.
            _ => (String::new(), Vec::new()),
        };
        if blocks.is_empty() {
            continue;
        }
        turns.push(Turn {
            session_id: session_id.to_string(),
            project: project.to_string(),
            turn_uuid: format!("{session_id}-{seq}"),
            parent_uuid: None,
            seq,
            ts: obj.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string(),
            role,
            blocks,
            source_path: p.to_string_lossy().into_owned(),
        });
        seq += 1;
    }

    jsonl::backfill_tool_names(&mut turns);
    Ok(turns)
}

/// A non-blank text as a `text` block, else `None`.
fn text_block(text: &str) -> Option<Block> {
    if text.trim().is_empty() {
        None
    } else {
        Some(Block {
            block_type: "text".into(),
            text: text.to_string(),
            tool_name: None,
            tool_use_id: None,
        })
    }
}

/// The `input_text`/`output_text` parts of a `message` payload, one text block each. Tolerates a
/// plain-string `content`.
fn message_blocks(payload: &Map<String, Value>) -> Vec<Block> {
    let content = match payload.get("content") {
        Some(c) => c,
        None => return Vec::new(),
    };
    if let Some(s) = content.as_str() {
        return text_block(s).into_iter().collect();
    }
    let arr = match content.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|part| {
            let obj = part.as_object()?;
            match obj.get("type").and_then(Value::as_str) {
                Some("input_text") | Some("output_text") => text_block(obj.get("text").and_then(Value::as_str)?),
                _ => None,
            }
        })
        .collect()
}

/// The `summary_text` parts of a `reasoning` payload joined into one thinking block. The real
/// reasoning is in `encrypted_content` (opaque) — only the human-readable summary is recoverable.
fn reasoning_block(payload: &Map<String, Value>) -> Option<Block> {
    let summary = payload.get("summary").and_then(Value::as_array)?;
    let parts: Vec<&str> = summary
        .iter()
        .filter_map(|s| {
            let o = s.as_object()?;
            if o.get("type").and_then(Value::as_str) == Some("summary_text") {
                o.get("text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect();
    let text = parts.join("\n");
    if text.trim().is_empty() {
        None
    } else {
        Some(Block {
            block_type: "thinking".into(),
            text,
            tool_name: None,
            tool_use_id: None,
        })
    }
}

/// A `function_call`/`custom_tool_call` payload as a tool_use block. `function_call` carries its
/// args as an `arguments` JSON string; `custom_tool_call` as a raw `input` string — either passes
/// through verbatim. `tool_use_id` is the `call_id` so the output can be correlated.
fn tool_use_block(payload: &Map<String, Value>) -> Block {
    let text = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .and_then(Value::as_str)
        .unwrap_or("{}")
        .to_string();
    Block {
        block_type: "tool_use".into(),
        text,
        tool_name: payload.get("name").and_then(Value::as_str).map(str::to_string),
        tool_use_id: payload.get("call_id").and_then(Value::as_str).map(str::to_string),
    }
}

/// A `*_output` payload as a tool_result block; `tool_name` is left for the `call_id` back-fill.
/// `None` if the output is blank.
fn tool_result_block(payload: &Map<String, Value>) -> Option<Block> {
    let text = output_text(payload.get("output"));
    if text.trim().is_empty() {
        None
    } else {
        Some(Block {
            block_type: "tool_result".into(),
            text,
            tool_name: None,
            tool_use_id: payload.get("call_id").and_then(Value::as_str).map(str::to_string),
        })
    }
}

/// A tool output value as text: a string verbatim, a null/absent as empty, anything else as compact
/// JSON.
fn output_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
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
    fn parses_message_reasoning_and_correlates_tool_names() {
        // session_meta is skipped; a reasoning item (two summary_text parts, opaque
        // encrypted_content) → one thinking turn; a function_call → a tool_use turn; its output →
        // a tool_result turn whose name back-fills from the matching call_id. seq counts only the
        // retained turns, and turn_uuid is session_id-seq.
        let f = write_jsonl(&[
            r#"{"type":"session_meta","timestamp":"t0","payload":{"id":"sess","cwd":"/w"}}"#,
            r#"{"type":"response_item","timestamp":"t1","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"plan a"},{"type":"summary_text","text":"plan b"}],"encrypted_content":"SECRETXYZ"}}"#,
            r#"{"type":"response_item","timestamp":"t2","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ls\"}","call_id":"call_1"}}"#,
            "",
            r#"{"type":"response_item","timestamp":"t3","payload":{"type":"function_call_output","call_id":"call_1","output":"file.txt"}}"#,
        ]);
        let turns = turns_from_jsonl_file(f.path(), "sess", "proj").unwrap();
        assert_eq!(turns.len(), 3);

        let think = &turns[0];
        assert_eq!(think.role, "assistant");
        assert_eq!(think.seq, 0);
        assert_eq!(think.turn_uuid, "sess-0");
        assert_eq!(think.ts, "t1");
        assert_eq!(think.blocks.len(), 1);
        assert_eq!(think.blocks[0].block_type, "thinking");
        assert_eq!(think.blocks[0].text, "plan a\nplan b");
        // The opaque ciphertext never leaks into the thinking block.
        assert!(!think.blocks[0].text.contains("SECRETXYZ"));

        let call = &turns[1];
        assert_eq!(call.seq, 1);
        assert_eq!(call.turn_uuid, "sess-1");
        assert_eq!(call.blocks[0].block_type, "tool_use");
        assert_eq!(call.blocks[0].tool_name.as_deref(), Some("exec_command"));
        assert_eq!(call.blocks[0].tool_use_id.as_deref(), Some("call_1"));
        // arguments (already a JSON string) passes through verbatim.
        assert_eq!(call.blocks[0].text, r#"{"cmd":"ls"}"#);

        let result = &turns[2];
        assert_eq!(result.role, "tool");
        assert_eq!(result.seq, 2);
        assert_eq!(result.blocks[0].block_type, "tool_result");
        assert_eq!(result.blocks[0].text, "file.txt");
        // name correlated from the function_call with the same call_id.
        assert_eq!(result.blocks[0].tool_name.as_deref(), Some("exec_command"));
    }

    #[test]
    fn custom_tool_call_uses_input_not_arguments() {
        let f = write_jsonl(&[
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch","call_id":"c2"}}"#,
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"custom_tool_call_output","call_id":"c2","output":"done"}}"#,
        ]);
        let turns = turns_from_jsonl_file(f.path(), "s", "p").unwrap();
        assert_eq!(turns.len(), 2);
        // The raw `input` string is the tool_use text (no `arguments` field).
        assert_eq!(turns[0].blocks[0].block_type, "tool_use");
        assert_eq!(turns[0].blocks[0].text, "*** Begin Patch");
        assert_eq!(turns[0].blocks[0].tool_name.as_deref(), Some("apply_patch"));
        assert_eq!(turns[1].blocks[0].block_type, "tool_result");
        assert_eq!(turns[1].blocks[0].tool_name.as_deref(), Some("apply_patch"));
    }

    #[test]
    fn message_content_parts_become_text_blocks() {
        let f = write_jsonl(&[
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"},{"type":"input_text","text":"world"}]}}"#,
        ]);
        let turns = turns_from_jsonl_file(f.path(), "s", "p").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "user");
        let kinds: Vec<&str> = turns[0].blocks.iter().map(|b| b.block_type.as_str()).collect();
        assert_eq!(kinds, vec!["text", "text"]);
        assert_eq!(turns[0].blocks[0].text, "hello");
        assert_eq!(turns[0].blocks[1].text, "world");
    }

    #[test]
    fn blank_reasoning_and_empty_output_are_dropped_seq_does_not_advance() {
        let f = write_jsonl(&[
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"   "}]}}"#,
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"function_call_output","call_id":"c","output":""}}"#,
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"real"}]}}"#,
        ]);
        let turns = turns_from_jsonl_file(f.path(), "s", "p").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].seq, 0);
        assert_eq!(turns[0].turn_uuid, "s-0");
        assert_eq!(turns[0].blocks[0].text, "real");
    }

    #[test]
    fn non_response_item_and_malformed_lines_tolerated() {
        let f = write_jsonl(&[
            r#"{"type":"event_msg","timestamp":"t","payload":{"type":"token_count"}}"#,
            r#"{"type":"turn_context","timestamp":"t","payload":{"model":"gpt"}}"#,
            "this is not json{",
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
        ]);
        let turns = turns_from_jsonl_file(f.path(), "s", "p").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "assistant");
        assert_eq!(turns[0].blocks[0].text, "hi");
    }

    #[test]
    fn missing_file_is_an_error_not_empty() {
        // A read failure must surface as Err so the indexer skips it without recording state.
        assert!(turns_from_jsonl_file(Path::new("/no/such/file.jsonl"), "s", "p").is_err());
    }
}
