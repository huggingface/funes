//! Shared index location + connection. `$FUNES_DB` if set, else `~/.funes`;
//! the lancedb connection lives in `<dir>/lancedb`.

use anyhow::Result;
use lancedb::{connect, Connection};
use std::path::PathBuf;

pub const TABLE: &str = "chunks";

pub fn funes_dir() -> PathBuf {
    if let Ok(d) = std::env::var("FUNES_DB") {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".funes")
}

pub fn lancedb_uri() -> String {
    funes_dir().join("lancedb").to_string_lossy().into_owned()
}

pub async fn open_db() -> Result<Connection> {
    Ok(connect(&lancedb_uri()).execute().await?)
}
