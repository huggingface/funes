//! Read Cursor Agent conversations from its global `state.vscdb` store.
//!
//! Cursor stores one `composerData:<id>` row per conversation and keeps the message payloads in
//! `bubbleId:<conversation-id>:<bubble-id>` rows. The database is opened read-only; a composer is
//! an incremental unit, signed by its `lastUpdatedAt` millisecond timestamp.

use super::{Block, Turn};
use anyhow::{Context, Result};
use rusqlite::{types::Value as SqlValue, Connection, OpenFlags};
use serde_json::Value;
use std::path::Path;

pub struct SessionUnit {
    pub session_id: String,
    pub watermark: i64,
}

fn open_ro(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening Cursor state.vscdb at {}", path.display()))
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn read_json(conn: &Connection, key: &str) -> Result<Option<Value>> {
    let raw: Option<SqlValue> = conn
        .query_row("SELECT value FROM cursorDiskKV WHERE key = ?1", [key], |r| r.get(0))
        .optional()?;
    Ok(raw.and_then(|value| match value {
        SqlValue::Blob(bytes) => serde_json::from_slice(&bytes).ok(),
        SqlValue::Text(text) => serde_json::from_str(&text).ok(),
        _ => None,
    }))
}

pub fn sessions_with_watermark(path: &Path) -> Result<Vec<SessionUnit>> {
    let conn = open_ro(path)?;
    let mut stmt = conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%' ORDER BY key")?;
    let rows = stmt.query_map([], |r| {
        let key: String = r.get(0)?;
        let value: SqlValue = r.get(1)?;
        Ok((key, value))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (key, value) = row?;
        let data = match value {
            SqlValue::Blob(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
            SqlValue::Text(text) => serde_json::from_str::<Value>(&text).ok(),
            _ => None,
        };
        let Some(data) = data else {
            continue;
        };
        let Some(session_id) = key.strip_prefix("composerData:") else {
            continue;
        };
        let watermark = data
            .get("lastUpdatedAt")
            .and_then(Value::as_i64)
            .or_else(|| data.get("createdAt").and_then(Value::as_i64))
            .unwrap_or(0);
        let has_messages = data
            .get("fullConversationHeadersOnly")
            .and_then(Value::as_array)
            .is_some_and(|messages| !messages.is_empty());
        if has_messages {
            sessions.push(SessionUnit {
                session_id: session_id.to_string(),
                watermark,
            });
        }
    }
    Ok(sessions)
}

pub fn turns_from_state_db(path: &Path, session_id: &str) -> Result<Vec<Turn>> {
    let conn = open_ro(path)?;
    let composer = read_json(&conn, &format!("composerData:{session_id}"))?
        .context("Cursor composerData row is missing or invalid")?;
    let headers = composer
        .get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
        .context("Cursor composerData has no conversation headers")?;
    let mut turns = Vec::new();
    for (seq, header) in headers.iter().enumerate() {
        let Some(bubble_id) = header.get("bubbleId").and_then(Value::as_str) else {
            continue;
        };
        let Some(bubble) = read_json(&conn, &format!("bubbleId:{session_id}:{bubble_id}"))? else {
            continue;
        };
        let kind = header
            .get("type")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| bubble.get("type").and_then(Value::as_i64).unwrap_or(0));
        let mut blocks = Vec::new();
        if kind == 1 {
            if let Some(s) = text(bubble.get("text")) {
                blocks.push(Block {
                    block_type: "text".into(),
                    text: s,
                    tool_name: None,
                    tool_use_id: None,
                });
            }
        } else if kind == 2 {
            if let Some(s) = bubble
                .pointer("/thinking/text")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                blocks.push(Block {
                    block_type: "thinking".into(),
                    text: s.to_string(),
                    tool_name: None,
                    tool_use_id: None,
                });
            }
            if let Some(s) = text(bubble.get("text")) {
                blocks.push(Block {
                    block_type: "text".into(),
                    text: s,
                    tool_name: None,
                    tool_use_id: None,
                });
            }
        }
        if blocks.is_empty() {
            continue;
        }
        let ts = bubble
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        turns.push(Turn {
            session_id: session_id.to_string(),
            workdir: "cursor".into(),
            turn_uuid: bubble_id.to_string(),
            parent_uuid: None,
            seq: seq as i64,
            ts,
            role: if kind == 1 { "user" } else { "assistant" }.into(),
            blocks,
            source_path: path.to_string_lossy().into_owned(),
            harness: "cursor".into(),
        });
    }
    Ok(turns)
}

trait OptionalRow<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}
impl<T> OptionalRow<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_composer_headers_and_bubbles() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
             INSERT INTO cursorDiskKV VALUES
             ('composerData:s1', '{\"createdAt\":1,\"lastUpdatedAt\":2,\"fullConversationHeadersOnly\":[{\"bubbleId\":\"b1\",\"type\":1},{\"bubbleId\":\"b2\",\"type\":2}]}'),
             ('bubbleId:s1:b1', '{\"type\":1,\"text\":\"question\",\"createdAt\":\"t1\"}'),
             ('bubbleId:s1:b2', '{\"type\":2,\"text\":\"answer\",\"thinking\":{\"text\":\"plan\"},\"createdAt\":\"t2\"}');",
        )
        .unwrap();

        let units = sessions_with_watermark(&db).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].session_id, "s1");
        assert_eq!(units[0].watermark, 2);

        let turns = turns_from_state_db(&db, "s1").unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].blocks[0].text, "question");
        assert_eq!(turns[1].blocks[0].block_type, "thinking");
        assert_eq!(turns[1].blocks[1].text, "answer");
        assert!(turns.iter().all(|turn| turn.harness == "cursor"));
    }
}
