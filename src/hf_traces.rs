//! Index agent-trace datasets stored as parquet (one row per session) into the same `Turn`/`Block`
//! shape the JSONL parser produces, so the rest of the index pipeline is reused unchanged.
//!
//! The layout targeted is the HF auto-converted parquet of pi-harness traces (e.g.
//! `Glint-Research/Fable-5-traces`): each row is one session whose `messages` column is a list of
//! JSON-encoded OpenAI-style chat messages — `{role, content, reasoning_content?, tool_calls?}`.
//! `reasoning_content` becomes a thinking block, `content` a text block, and each `tool_calls`
//! entry a tool_use block, matching funes' block vocabulary. Per-row columns carry the facets:
//! `harness`, and the project as the basename of the `metadata` JSON's `cwd` key (where the
//! normalizer surfaces the session's working directory) — the same derivation as the JSONL
//! parsers.

use crate::jsonl;
use crate::trace::{Block, Turn};

use anyhow::{anyhow, Context, Result};
use arrow_array::{Array, LargeStringArray, ListArray, RecordBatch, StringArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::Value;
use std::fs::File;
use std::path::Path;

/// Read agent-trace sessions from a parquet file as a flat stream of turns — at most `limit`
/// sessions (one row each), skipping rows whose messages yield no indexable block. Each `Turn`
/// carries its own `session_id`, so the index pipeline groups them without a per-session wrapper —
/// mirroring `claude_traces::turns_from_jsonl_file`, which also returns `Vec<Turn>`.
pub fn turns_from_parquet(path: &Path, fallback_project: &str, limit: Option<usize>) -> Result<Vec<Turn>> {
    let file = File::open(path).with_context(|| format!("open parquet {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let cap = limit.unwrap_or(usize::MAX);

    let mut out: Vec<Turn> = Vec::new();
    let mut sessions = 0usize;
    for batch in reader {
        let batch = batch?;
        if batch.column_by_name("session_id").is_none() {
            return Err(anyhow!("parquet has no `session_id` column"));
        }
        let messages = batch
            .column_by_name("messages")
            .context("parquet has no `messages` column")?
            .as_any()
            .downcast_ref::<ListArray>()
            .context("`messages` is not a list column")?;

        for i in 0..batch.num_rows() {
            if sessions >= cap {
                return Ok(out);
            }
            let sid = match str_at(&batch, "session_id", i) {
                Some(s) => s,
                None => continue,
            };
            if messages.is_null(i) {
                continue;
            }
            let ts = str_at(&batch, "sent_at", i).unwrap_or_default();
            let source = str_at(&batch, "file_path", i).unwrap_or_else(|| path.display().to_string());
            // Per-row: a dataset can mix harnesses; a dataset without the column reads as "".
            let harness = str_at(&batch, "harness", i).unwrap_or_default();
            // Per-row facet: the basename of the session's recorded cwd (`metadata.cwd`, where
            // the normalizer surfaces it), else the dataset-level fallback (its file stem).
            let project = metadata_cwd(&batch, i)
                .as_deref()
                .and_then(jsonl::project_of_cwd)
                .unwrap_or_else(|| fallback_project.to_string());

            let msgs = parse_message_list(&messages.value(i))?;
            let turns = turns_from_messages(&msgs, &sid, &project, &ts, &source, &harness);
            if !turns.is_empty() {
                out.extend(turns);
                sessions += 1;
            }
        }
    }
    Ok(out)
}

/// The `cwd` recorded in a row's `metadata` JSON column — where the Hub's agent-traces normalizer
/// surfaces the session's working directory.
fn metadata_cwd(batch: &RecordBatch, i: usize) -> Option<String> {
    let md = str_at(batch, "metadata", i)?;
    let v: Value = serde_json::from_str(&md).ok()?;
    v.get("cwd").and_then(Value::as_str).map(str::to_string)
}

/// A `Utf8`/`LargeUtf8` column's non-null value at row `i`. HF auto-conversion can emit either
/// physical width, so accept both — the same dual handling `parse_message_list` uses for the
/// message elements.
fn str_at(batch: &RecordBatch, name: &str, i: usize) -> Option<String> {
    let col = batch.column_by_name(name)?;
    if let Some(a) = col.as_any().downcast_ref::<StringArray>() {
        (!a.is_null(i)).then(|| a.value(i).to_string())
    } else if let Some(a) = col.as_any().downcast_ref::<LargeStringArray>() {
        (!a.is_null(i)).then(|| a.value(i).to_string())
    } else {
        None
    }
}

/// The JSON-string elements of one row's `messages` list, parsed. The list's value type is the
/// `arrow.json` extension over either `Utf8` or `LargeUtf8`, so accept both physical widths.
fn parse_message_list(elems: &dyn Array) -> Result<Vec<Value>> {
    let raw: Vec<&str> = if let Some(a) = elems.as_any().downcast_ref::<StringArray>() {
        (0..a.len()).filter(|&i| !a.is_null(i)).map(|i| a.value(i)).collect()
    } else if let Some(a) = elems.as_any().downcast_ref::<LargeStringArray>() {
        (0..a.len()).filter(|&i| !a.is_null(i)).map(|i| a.value(i)).collect()
    } else {
        return Err(anyhow!("`messages` elements are neither Utf8 nor LargeUtf8"));
    };
    // Skip any element that isn't valid JSON, mirroring the JSONL parser's per-line tolerance in
    // `parse.rs`; a row whose messages all fail to parse simply yields no turns and is dropped.
    Ok(raw
        .iter()
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .collect())
}

/// Turn one session's chat messages into funes turns. `seq` counts only retained turns (those with
/// at least one block), and `parent_uuid` chains them — mirroring the JSONL parser.
fn turns_from_messages(
    msgs: &[Value],
    session_id: &str,
    project: &str,
    ts: &str,
    source: &str,
    harness: &str,
) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut seq = 0i64;
    let mut parent: Option<String> = None;
    for m in msgs {
        let blocks = blocks_from_message(m);
        if blocks.is_empty() {
            continue;
        }
        let role = m.get("role").and_then(Value::as_str).unwrap_or("").to_string();
        let turn_uuid = format!("{session_id}-{seq}");
        turns.push(Turn {
            session_id: session_id.to_string(),
            project: project.to_string(),
            turn_uuid: turn_uuid.clone(),
            parent_uuid: parent.take(),
            seq,
            ts: ts.to_string(),
            role,
            blocks,
            source_path: source.to_string(),
            harness: harness.to_string(),
        });
        parent = Some(turn_uuid);
        seq += 1;
    }
    turns
}

/// Blocks for one OpenAI-style chat message: thinking (`reasoning_content`) → text (`content`) →
/// a tool_use per `tool_calls` entry. Blank fields are dropped, matching `normalize_blocks`.
fn blocks_from_message(m: &Value) -> Vec<Block> {
    let mut blocks = Vec::new();
    let text_block = |block_type: &str, text: &str| Block {
        block_type: block_type.to_string(),
        text: text.to_string(),
        tool_name: None,
        tool_use_id: None,
    };

    if let Some(t) = m.get("reasoning_content").and_then(Value::as_str) {
        if !t.trim().is_empty() {
            blocks.push(text_block("thinking", t));
        }
    }
    if let Some(t) = m.get("content").and_then(Value::as_str) {
        if !t.trim().is_empty() {
            blocks.push(text_block("text", t));
        }
    }
    if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let func = call.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // `arguments` is usually a JSON string already; an object is re-serialized compactly.
            let args = match func.and_then(|f| f.get("arguments")) {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string(v).unwrap_or_default(),
                // Match the JSONL path, which defaults a missing tool input to an empty object.
                None => "{}".to_string(),
            };
            blocks.push(Block {
                block_type: "tool_use".to_string(),
                text: args,
                tool_name: name,
                tool_use_id: None,
            });
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{builder::ListBuilder, builder::StringBuilder, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    /// Write a tiny parquet with the same shape as the HF auto-parquet: `session_id`/`sent_at`
    /// Utf8 columns and a `messages` list-of-Utf8 column holding JSON-encoded chat messages.
    fn write_parquet(rows: &[(&str, &str, Vec<&str>)]) -> tempfile::NamedTempFile {
        let schema = Arc::new(Schema::new(vec![
            Field::new("session_id", DataType::Utf8, false),
            Field::new("sent_at", DataType::Utf8, true),
            Field::new(
                "messages",
                DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                false,
            ),
        ]));
        let mut sid = StringBuilder::new();
        let mut sent = StringBuilder::new();
        let mut msgs = ListBuilder::new(StringBuilder::new());
        for (s, t, ms) in rows {
            sid.append_value(s);
            sent.append_value(t);
            for m in ms {
                msgs.values().append_value(m);
            }
            msgs.append(true);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(sid.finish()), Arc::new(sent.finish()), Arc::new(msgs.finish())],
        )
        .unwrap();
        let f = tempfile::Builder::new().suffix(".parquet").tempfile().unwrap();
        let mut w = ArrowWriter::try_new(f.reopen().unwrap(), schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        f
    }

    #[test]
    fn maps_messages_into_thinking_text_and_tool_use_blocks() {
        let user = r#"{"role":"user","content":"build a parser"}"#;
        let asst = r#"{"role":"assistant","content":"on it","reasoning_content":"plan the parse","tool_calls":[{"type":"function","function":{"name":"Bash","arguments":"{\"command\":\"cargo test\"}"}}]}"#;
        let f = write_parquet(&[("sess-1", "2026-06-19T00:00:02.000Z", vec![user, asst])]);

        let turns = turns_from_parquet(f.path(), "Fable-5-traces", None).unwrap();
        assert_eq!(turns.len(), 2);

        let u = &turns[0];
        assert_eq!(u.session_id, "sess-1");
        assert_eq!(u.role, "user");
        assert_eq!(u.project, "Fable-5-traces");
        assert_eq!(u.seq, 0);
        assert_eq!(u.ts, "2026-06-19T00:00:02.000Z");
        assert_eq!(u.blocks.len(), 1);
        assert_eq!(u.blocks[0].block_type, "text");
        // No `harness` column in this parquet → "" (a pre-migration store reads clean).
        assert_eq!(u.harness, "");

        let a = &turns[1];
        assert_eq!(a.role, "assistant");
        assert_eq!(a.parent_uuid.as_deref(), Some("sess-1-0"));
        // thinking, then text, then tool_use.
        let kinds: Vec<&str> = a.blocks.iter().map(|b| b.block_type.as_str()).collect();
        assert_eq!(kinds, vec!["thinking", "text", "tool_use"]);
        let tool = a.blocks.iter().find(|b| b.block_type == "tool_use").unwrap();
        assert_eq!(tool.tool_name.as_deref(), Some("Bash"));
        assert_eq!(tool.text, r#"{"command":"cargo test"}"#);
    }

    #[test]
    fn harness_column_is_read_per_row() {
        // A dataset can mix harnesses; each session's turns carry that row's `harness` value.
        let msg = r#"{"role":"user","content":"hi"}"#;
        let schema = Arc::new(Schema::new(vec![
            Field::new("session_id", DataType::Utf8, false),
            Field::new(
                "messages",
                DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                false,
            ),
            Field::new("harness", DataType::Utf8, true),
        ]));
        let mut sid = StringBuilder::new();
        let mut msgs = ListBuilder::new(StringBuilder::new());
        let mut hn = StringBuilder::new();
        for (s, h) in [("s1", "codex"), ("s2", "claude_code")] {
            sid.append_value(s);
            msgs.values().append_value(msg);
            msgs.append(true);
            hn.append_value(h);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(sid.finish()), Arc::new(msgs.finish()), Arc::new(hn.finish())],
        )
        .unwrap();
        let f = tempfile::Builder::new().suffix(".parquet").tempfile().unwrap();
        let mut w = ArrowWriter::try_new(f.reopen().unwrap(), schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();

        let turns = turns_from_parquet(f.path(), "mixed", None).unwrap();
        let harness_for = |sess: &str| -> Vec<&str> {
            turns
                .iter()
                .filter(|t| t.session_id == sess)
                .map(|t| t.harness.as_str())
                .collect()
        };
        assert!(harness_for("s1").iter().all(|h| *h == "codex"), "s1 rows are codex");
        assert!(
            harness_for("s2").iter().all(|h| *h == "claude_code"),
            "s2 rows are claude_code"
        );
        assert!(!harness_for("s1").is_empty() && !harness_for("s2").is_empty());
    }

    #[test]
    fn project_is_the_metadata_cwd_basename_with_the_stem_fallback() {
        let msg = r#"{"role":"user","content":"hi"}"#;
        let schema = Arc::new(Schema::new(vec![
            Field::new("session_id", DataType::Utf8, false),
            Field::new(
                "messages",
                DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                false,
            ),
            Field::new("metadata", DataType::Utf8, true),
        ]));
        let mut sid = StringBuilder::new();
        let mut msgs = ListBuilder::new(StringBuilder::new());
        let mut md = StringBuilder::new();
        for (s, m) in [
            ("s1", Some(r#"{"cwd":"/Users/g/Desktop/huggingface.js"}"#)),
            ("s2", Some(r#"{"cwd":null}"#)),
            ("s3", None),
        ] {
            sid.append_value(s);
            msgs.values().append_value(msg);
            msgs.append(true);
            md.append_option(m);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(sid.finish()), Arc::new(msgs.finish()), Arc::new(md.finish())],
        )
        .unwrap();
        let f = tempfile::Builder::new().suffix(".parquet").tempfile().unwrap();
        let mut w = ArrowWriter::try_new(f.reopen().unwrap(), schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();

        let turns = turns_from_parquet(f.path(), "stem", None).unwrap();
        let project_for =
            |sess: &str| -> String { turns.iter().find(|t| t.session_id == sess).unwrap().project.clone() };
        assert_eq!(project_for("s1"), "huggingface.js", "metadata.cwd basename");
        assert_eq!(project_for("s2"), "stem", "null cwd → the dataset fallback");
        assert_eq!(project_for("s3"), "stem", "no metadata → the dataset fallback");
    }

    #[test]
    fn limit_caps_sessions_and_blank_messages_are_dropped() {
        let blank = r#"{"role":"assistant","content":"   "}"#; // no indexable block
        let real = r#"{"role":"user","content":"hello"}"#;
        let f = write_parquet(&[
            ("s1", "t", vec![real]),
            ("s2", "t", vec![blank]), // produces no turns → not emitted
            ("s3", "t", vec![real]),
        ]);

        // distinct session_ids in the returned (flat) turns, in order.
        let sids = |turns: Vec<Turn>| {
            let mut v: Vec<String> = turns.into_iter().map(|t| t.session_id).collect();
            v.dedup();
            v
        };
        // limit stops after the first emitted session.
        assert_eq!(sids(turns_from_parquet(f.path(), "p", Some(1)).unwrap()), vec!["s1"]);
        // without a limit, the all-blank session is skipped, leaving two.
        assert_eq!(sids(turns_from_parquet(f.path(), "p", None).unwrap()), vec!["s1", "s3"]);
    }

    #[test]
    fn malformed_message_json_is_skipped_not_fatal() {
        // A non-JSON message element must not abort the run: the bad element is dropped (like the
        // JSONL parser drops a bad line), and a session whose every message is bad is not emitted.
        let good = r#"{"role":"user","content":"hello"}"#;
        let bad = "this is not json{";
        let f = write_parquet(&[("s1", "t", vec![bad, good]), ("s2", "t", vec![bad])]);

        let turns = turns_from_parquet(f.path(), "p", None).unwrap();
        // s1: bad element skipped, good kept (1 turn); s2: all bad → no turns → dropped.
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].session_id, "s1");
        assert_eq!(turns[0].blocks[0].text, "hello");
    }
}
