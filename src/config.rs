//! The funes config (`funes.json`): the persisted active store. Lives in `$FUNES_HOME` next to the
//! local index. It holds the remote this host is attached to — its absence means "local only".
//! This is the stateful replacement for the old `$FUNES_STORE` env var: set once with `funes use`,
//! honored by every command (read source, push target, the MCP server's recall target).

use crate::dataset;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// The attached remote (`hf://datasets/<org>/<repo>`). `None` ⇒ recall/push use the local index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

/// Path to `funes.json` under funes's home directory.
pub fn path() -> PathBuf {
    dataset::funes_dir().join("funes.json")
}

/// Load the config, defaulting to empty (local) when the file is absent or unreadable.
pub fn load() -> Config {
    load_from(&path())
}

/// Persist the config, creating the parent directory if needed.
pub fn save(cfg: &Config) -> Result<()> {
    save_to(&path(), cfg)
}

/// Pure core of [`load`]: read+parse `p`, or the default (local) when it's absent/unreadable.
fn load_from(p: &Path) -> Config {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Pure core of [`save`]: write `cfg` to `p`, creating its parent directory.
fn save_to(p: &Path, cfg: &Config) -> Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(cfg)?).context("writing funes.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips_the_remote() {
        let home = tempfile::tempdir().unwrap();
        let p = home.path().join("funes.json");

        // Absent file → empty (local).
        assert_eq!(load_from(&p).remote, None);

        save_to(
            &p,
            &Config {
                remote: Some("hf://datasets/acme/kb".into()),
            },
        )
        .unwrap();
        assert_eq!(load_from(&p).remote.as_deref(), Some("hf://datasets/acme/kb"));

        // Clearing it (back to local).
        save_to(&p, &Config { remote: None }).unwrap();
        assert_eq!(load_from(&p).remote, None);
    }
}
