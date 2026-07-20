//! Parse hermes sessions from its SQLite state store (`~/.hermes/state.db`) into the shared
//! [`crate::trace`] turn/block model. Unlike Claude/Codex/pi (one JSONL file per session), hermes
//! keeps every session's messages in one WAL SQLite DB: a `sessions` table (one row per session,
//! carrying `cwd`) and a `messages` table (one row per message, ordered by the `id` autoincrement).
//!
//! One `messages` row becomes one [`Turn`]: `user`/`assistant`/`tool` roles map straight across,
//! an assistant row expands into thinking (`reasoning_content`) + text (`content`) + tool_use
//! (`tool_calls`) blocks, and a tool row becomes a tool_result block. Reading rules that matter for
//! a faithful, stable index:
//! - order by `id`, never `timestamp` (hermes writes `time.time()`, which is non-monotonic);
//! - read every row regardless of `active`/`compacted` — pre-compression turns are real history we
//!   want recallable, and `id` gives them stable chunk ids;
//! - read `content`, not `api_content` (the latter is a byte-fidelity API sidecar with ephemeral
//!   injections); the `messages_fts*` virtual tables are search indexes and are ignored.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::jsonl;
use crate::trace::{Block, Turn};

/// One indexable hermes session: its id, resolved workdir facet, and the high-water `messages.id`
/// used as the incremental signature (a session is re-read only once a newer message lands).
pub struct SessionUnit {
    pub session_id: String,
    pub workdir: String,
    pub watermark: i64,
}

/// Open the state DB read-only — funes never writes hermes' store, and a reader takes no lock, so
/// this is safe while hermes is running (WAL).
fn open_ro(db: &Path) -> Result<Connection> {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening hermes state.db at {}", db.display()))
}

/// The workdir facet for a session: its `sessions.cwd`, munged the way every parser munges a cwd,
/// or `None` when the session has no recorded cwd.
fn session_workdir(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    let cwd: Option<String> = conn
        .query_row("SELECT cwd FROM sessions WHERE id = ?1", [session_id], |r| r.get(0))
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(cwd.as_deref().and_then(jsonl::workdir_of_cwd))
}

/// Every session that has at least one message, each signed by its high-water `messages.id`. The
/// caller ([`crate::source`]) turns these into incremental units; a session whose watermark is
/// unchanged since the last index is skipped.
pub fn sessions_with_watermark(db: &Path) -> Result<Vec<SessionUnit>> {
    let conn = open_ro(db)?;
    // `messages` drives the query, so a session with no messages is already excluded (nothing to
    // index) whatever the join. The join is LEFT to tolerate a message whose `session_id` has no
    // `sessions` row: `sessions.cwd` is then NULL, coalesced to '' (workdir falls back).
    let mut stmt = conn.prepare(
        "SELECT m.session_id, COALESCE(s.cwd, ''), MAX(m.id) \
         FROM messages m LEFT JOIN sessions s ON s.id = m.session_id \
         GROUP BY m.session_id ORDER BY m.session_id",
    )?;
    let rows = stmt.query_map([], |r| {
        let session_id: String = r.get(0)?;
        let cwd: String = r.get(1)?;
        let watermark: i64 = r.get(2)?;
        Ok((session_id, cwd, watermark))
    })?;
    let mut units = Vec::new();
    for row in rows {
        let (session_id, cwd, watermark) = row?;
        let workdir = jsonl::workdir_of_cwd(&cwd).unwrap_or_default();
        units.push(SessionUnit {
            session_id,
            workdir,
            watermark,
        });
    }
    Ok(units)
}

/// Parse one session's messages into turns, in `id` order. `fallback_workdir` covers a session with
/// no recorded cwd (parity with the JSONL parsers).
pub fn turns_from_state_db(db: &Path, session_id: &str, fallback_workdir: &str) -> Result<Vec<Turn>> {
    let conn = open_ro(db)?;
    let workdir = session_workdir(&conn, session_id)?.unwrap_or_else(|| fallback_workdir.to_string());
    let source_path = db.to_string_lossy().into_owned();

    let mut stmt = conn.prepare(
        "SELECT id, role, content, tool_call_id, tool_calls, tool_name, timestamp, \
         reasoning, reasoning_content \
         FROM messages WHERE session_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([session_id], |r| {
        Ok(Row {
            id: r.get(0)?,
            role: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            content: r.get(2)?,
            tool_call_id: r.get(3)?,
            tool_calls: r.get(4)?,
            tool_name: r.get(5)?,
            timestamp: r.get(6)?,
            reasoning: r.get(7)?,
            reasoning_content: r.get(8)?,
        })
    })?;

    let mut turns = Vec::new();
    let mut seq = 0i64; // index among RETAINED turns, in id order
    for row in rows {
        let row = row?;
        let blocks = blocks_for(&row);
        if blocks.is_empty() {
            continue;
        }
        turns.push(Turn {
            session_id: session_id.to_string(),
            workdir: workdir.clone(),
            turn_uuid: row.id.to_string(),
            parent_uuid: None,
            seq,
            ts: ts_rfc3339(row.timestamp),
            role: row.role,
            blocks,
            source_path: source_path.clone(),
            harness: "hermes".into(),
        });
        seq += 1;
    }
    Ok(turns)
}

/// A `messages` row, only the columns we index.
struct Row {
    id: i64,
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<f64>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
}

/// The typed blocks for a row by role. `user` → text; `assistant` → thinking + text + tool_use (in
/// that order); `tool` → tool_result. Any other role (e.g. `system`) yields nothing and the turn is
/// dropped, as are rows whose blocks are all blank.
fn blocks_for(row: &Row) -> Vec<Block> {
    match row.role.as_str() {
        "user" => text_like_block("text", row.content.as_deref().unwrap_or(""))
            .into_iter()
            .collect(),
        "assistant" => {
            let mut blocks = Vec::new();
            // reasoning_content is the human-readable trace; reasoning is the terser fallback.
            let thinking = row
                .reasoning_content
                .as_deref()
                .or(row.reasoning.as_deref())
                .unwrap_or("");
            blocks.extend(text_like_block("thinking", thinking));
            blocks.extend(text_like_block("text", row.content.as_deref().unwrap_or("")));
            blocks.extend(tool_use_blocks(row.tool_calls.as_deref().unwrap_or("")));
            blocks
        }
        "tool" => tool_result_block(row).into_iter().collect(),
        _ => Vec::new(),
    }
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

/// The assistant's `tool_calls` (OpenAI JSON: a list of `{id, function:{name, arguments}}`) as
/// tool_use blocks. `arguments` is already a JSON string in the OpenAI shape, kept verbatim; an
/// object form is re-serialized compactly. A blank/unparseable value yields no blocks.
fn tool_use_blocks(tool_calls: &str) -> Vec<Block> {
    let parsed: Value = match serde_json::from_str(tool_calls) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match parsed.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .map(|call| {
            let func = call.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let args = match func.and_then(|f| f.get("arguments")) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".into()),
                None => "{}".to_string(),
            };
            Block {
                block_type: "tool_use".to_string(),
                text: args,
                tool_name: name,
                tool_use_id: call.get("id").and_then(Value::as_str).map(str::to_string),
            }
        })
        .collect()
}

/// A tool row's `content` as a tool_result block; its name/id come from `tool_name`/`tool_call_id`.
/// `None` if the content is blank.
fn tool_result_block(row: &Row) -> Option<Block> {
    let text = row.content.as_deref().unwrap_or("");
    if text.trim().is_empty() {
        return None;
    }
    Some(Block {
        block_type: "tool_result".to_string(),
        text: text.to_string(),
        tool_name: row.tool_name.clone(),
        tool_use_id: row.tool_call_id.clone(),
    })
}

/// An epoch-seconds `timestamp` (hermes writes `time.time()`) as RFC3339 — the format recency
/// weighting parses. A missing or out-of-range value yields an empty string (recency then treats
/// the hit as fresh, matching an unparseable stamp).
fn ts_rfc3339(secs: Option<f64>) -> String {
    let secs = match secs {
        Some(s) => s,
        None => return String::new(),
    };
    let whole = secs.trunc() as i64;
    let nanos = (secs.fract().abs() * 1e9) as u32;
    match Utc.timestamp_opt(whole, nanos).single() {
        Some(dt) => dt.to_rfc3339(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp state.db with the load-bearing columns of hermes' `sessions`/`messages` schema.
    fn make_db(cwd: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, started_at REAL);
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_call_id TEXT,
                tool_calls TEXT,
                tool_name TEXT,
                timestamp REAL NOT NULL,
                reasoning TEXT,
                reasoning_content TEXT,
                active INTEGER NOT NULL DEFAULT 1,
                api_content TEXT
             );",
        )
        .unwrap();
        conn.execute("INSERT INTO sessions (id, cwd) VALUES ('sess', ?1)", [cwd])
            .unwrap();
        (dir, path)
    }

    fn insert(path: &Path, sql: &str) {
        Connection::open(path).unwrap().execute_batch(sql).unwrap();
    }

    #[test]
    fn parses_user_assistant_and_tool_rows() {
        let (_dir, path) = make_db(Some("/home/u/funes"));
        insert(
            &path,
            r#"
            INSERT INTO messages (session_id, role, content, timestamp) VALUES
                ('sess','user','how does this work',1000.0);
            INSERT INTO messages (session_id, role, content, timestamp, reasoning_content, tool_calls) VALUES
                ('sess','assistant','on it',1001.5,'let me look',
                 '[{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls -la\"}"}}]');
            INSERT INTO messages (session_id, role, content, tool_call_id, tool_name, timestamp) VALUES
                ('sess','tool','file.txt','call_1','bash',1002.0);
            INSERT INTO messages (session_id, role, content, timestamp) VALUES
                ('sess','system','you are helpful',999.0);
            "#,
        );

        let turns = turns_from_state_db(&path, "sess", "fb").unwrap();
        // system row dropped → user, assistant, tool.
        assert_eq!(turns.len(), 3);

        let u = &turns[0];
        assert_eq!(u.role, "user");
        assert_eq!(u.seq, 0);
        assert_eq!(u.turn_uuid, "1");
        assert_eq!(u.workdir, "-home-u-funes");
        assert_eq!(u.harness, "hermes");
        assert_eq!(u.blocks.len(), 1);
        assert_eq!(u.blocks[0].text, "how does this work");
        // epoch-seconds → RFC3339, parseable by recency.
        assert!(
            chrono::DateTime::parse_from_rfc3339(&u.ts).is_ok(),
            "ts not rfc3339: {}",
            u.ts
        );

        let a = &turns[1];
        assert_eq!(a.role, "assistant");
        assert_eq!(a.turn_uuid, "2");
        let kinds: Vec<&str> = a.blocks.iter().map(|b| b.block_type.as_str()).collect();
        assert_eq!(kinds, vec!["thinking", "text", "tool_use"]);
        assert_eq!(a.blocks[0].text, "let me look");
        assert_eq!(a.blocks[1].text, "on it");
        let tool = &a.blocks[2];
        assert_eq!(tool.tool_name.as_deref(), Some("bash"));
        assert_eq!(tool.tool_use_id.as_deref(), Some("call_1"));
        assert_eq!(tool.text, r#"{"command":"ls -la"}"#);

        let r = &turns[2];
        assert_eq!(r.role, "tool");
        assert_eq!(r.blocks[0].block_type, "tool_result");
        assert_eq!(r.blocks[0].text, "file.txt");
        assert_eq!(r.blocks[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(r.blocks[0].tool_use_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn includes_inactive_rows_and_drops_blank() {
        let (_dir, path) = make_db(Some("/w"));
        insert(
            &path,
            r#"
            INSERT INTO messages (session_id, role, content, timestamp, active) VALUES
                ('sess','user','pre-compaction turn',1.0,0);
            INSERT INTO messages (session_id, role, content, timestamp) VALUES
                ('sess','assistant','   ',2.0);
            "#,
        );
        let turns = turns_from_state_db(&path, "sess", "fb").unwrap();
        // The inactive (active=0) row is kept; the whitespace-only assistant row is dropped.
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].blocks[0].text, "pre-compaction turn");
    }

    #[test]
    fn workdir_falls_back_when_cwd_absent() {
        let (_dir, path) = make_db(None);
        insert(
            &path,
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('sess','user','hi',1.0);",
        );
        assert_eq!(
            turns_from_state_db(&path, "sess", "fallback").unwrap()[0].workdir,
            "fallback"
        );
    }

    #[test]
    fn watermark_is_max_message_id_per_session() {
        let (_dir, path) = make_db(Some("/home/u/funes"));
        insert(
            &path,
            r#"
            INSERT INTO sessions (id, cwd) VALUES ('other','/home/u/other');
            INSERT INTO messages (session_id, role, content, timestamp) VALUES
                ('sess','user','a',1.0),
                ('other','user','b',2.0),
                ('sess','assistant','c',3.0);
            "#,
        );
        let mut units = sessions_with_watermark(&path).unwrap();
        units.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].session_id, "other");
        assert_eq!(units[0].watermark, 2);
        assert_eq!(units[1].session_id, "sess");
        assert_eq!(units[1].workdir, "-home-u-funes");
        // 'sess' has message ids 1 and 3 → watermark 3.
        assert_eq!(units[1].watermark, 3);
    }
}
