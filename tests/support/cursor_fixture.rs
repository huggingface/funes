//! Restore a sanitized native Cursor KV export into a temporary SQLite store.

use std::path::Path;

use rusqlite::{types::Value as SqlValue, Connection};
use serde_json::Value;

pub const SESSION_ID: &str = "cursor-fixture";
pub const HEADER_COUNT: usize = 7;

/// Keep the first `headers` messages, so callers can later grow the same conversation.
/// Alternate TEXT/BLOB values and reverse insertion order to exercise SQLite storage types and
/// ensure the parser follows headers rather than key or insertion order.
pub fn write(path: &Path, headers: usize) {
    assert!(headers <= HEADER_COUNT);
    let mut rows: Vec<Value> = serde_json::from_str(include_str!("../fixtures/cursor_session.json")).unwrap();
    let composer = &mut rows[0]["value"];
    composer["fullConversationHeadersOnly"]
        .as_array_mut()
        .unwrap()
        .truncate(headers);
    composer["lastUpdatedAt"] = (1_767_225_600_000_i64 + headers as i64 * 1000).into();
    rows.truncate(headers + 1);

    let mut conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
        .unwrap();
    let tx = conn.transaction().unwrap();
    for (i, row) in rows.iter().enumerate().rev() {
        let json = serde_json::to_string(&row["value"]).unwrap();
        let value = if i % 2 == 0 {
            SqlValue::Text(json)
        } else {
            SqlValue::Blob(json.into_bytes())
        };
        tx.execute(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![row["key"].as_str().unwrap(), value],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}
