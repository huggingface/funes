//! Reading agent sessions: where they come from ([`source`]), the per-harness parsers, and the
//! parsed-trace model they all produce.
//!
//! A transcript becomes a sequence of [`Turn`]s, each carrying typed [`Block`]s. Every parser
//! produces this shape, and everything downstream — chunk → embed → store → recall — operates on
//! it, so the model is source-agnostic and lives here, at the root of the parsers that fill it.

pub mod claude;
pub mod codex;
pub mod funes_jsonl;
pub mod harness;
pub mod hermes;
pub mod ingest;
pub mod jsonl;
pub mod parquet;
pub mod pi;
pub mod repo;
pub mod source;

use serde::{Deserialize, Deserializer, Serialize};

/// The version of the serialized turn format this build reads and writes.
pub const FORMAT_VERSION: u32 = 1;

// serde's `default` takes a function path, not a constant.
/// The closed block vocabulary.
pub const BLOCK_TYPES: [&str; 4] = ["text", "thinking", "tool_use", "tool_result"];

fn format_default() -> u32 {
    FORMAT_VERSION
}

/// A `format` this build does not know is refused rather than misread.
fn known_format<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = u32::deserialize(d)?;
    if v == FORMAT_VERSION {
        Ok(v)
    } else {
        Err(serde::de::Error::custom(format!("unknown format {v}")))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub block_type: String, // one of [`BLOCK_TYPES`]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}

/// The serde derives on [`Turn`] and [`Block`] are the serialized turn format, `docs/funes-jsonl.md`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    #[serde(default = "format_default", deserialize_with = "known_format")]
    pub format: u32,
    pub session_id: String,
    /// The working directory the session recorded, as its harness wrote it; `None` when the
    /// source records none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip)]
    pub workdir: String,
    pub turn_uuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>,
    pub seq: i64,
    pub ts: String,
    pub role: String,
    pub blocks: Vec<Block>,
    #[serde(skip)]
    pub source_path: String,
    /// Who produced this session — `claude_code` | `codex` | `pi` | `hermes` from the native
    /// parsers, any `[a-z0-9_-]` id from a turns file.
    pub harness: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{self, Tier};
    use std::path::Path;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    /// A line of the spec's own example, minus `format`.
    const LINE: &str = r#"{"session_id":"s","turn_uuid":"t-1","seq":0,"ts":"2026-09-18T09:41:07Z","role":"user","harness":"opencode","blocks":[{"block_type":"text","text":"hi"}]}"#;

    fn with(field: &str) -> String {
        format!("{{{field},{}", &LINE[1..])
    }

    /// Serialize each fixture's turns and read them back: the turns are equal (bar the two funes-stamped
    /// fields, which the format does not carry) and so are the chunk ids they produce.
    #[test]
    fn a_parsed_turn_round_trips_with_identical_chunk_ids() {
        let claude = fixture("claude_session.jsonl");
        let codex = fixture("codex_session.jsonl");
        let pi = fixture("pi_session.jsonl");
        let parsed = [
            claude::turns_from_jsonl_file(&claude, "s", "fb").unwrap(),
            codex::turns_from_jsonl_file(&codex, "fb").unwrap(),
            pi::turns_from_jsonl_file(&pi, "s", "fb").unwrap(),
        ];
        for turns in &parsed {
            let ids = |t: &[Turn]| -> Vec<String> {
                chunk::chunks_from_turns(t, &Tier::ALL, true)
                    .into_iter()
                    .map(|c| c.id)
                    .collect()
            };
            let back: Vec<Turn> = turns
                .iter()
                .map(|t| serde_json::from_str(&serde_json::to_string(t).unwrap()).unwrap())
                .collect();
            assert_eq!(ids(&back), ids(turns));
            for (b, t) in back.into_iter().zip(turns) {
                assert_eq!((b.workdir.as_str(), b.source_path.as_str()), ("", ""));
                let b = Turn {
                    workdir: t.workdir.clone(),
                    source_path: t.source_path.clone(),
                    ..b
                };
                assert_eq!(&b, t);
            }
        }
    }

    #[test]
    fn format_defaults_to_one_and_an_unknown_one_is_refused() {
        assert_eq!(serde_json::from_str::<Turn>(LINE).unwrap().format, FORMAT_VERSION);
        assert_eq!(serde_json::from_str::<Turn>(&with(r#""format":1"#)).unwrap().format, 1);
        let err = serde_json::from_str::<Turn>(&with(r#""format":2"#)).unwrap_err();
        assert!(err.to_string().contains("unknown format 2"), "{err}");
    }

    #[test]
    fn an_unknown_or_funes_stamped_field_is_rejected() {
        assert!(serde_json::from_str::<Turn>(&with(r#""extra":1"#)).is_err());
        assert!(serde_json::from_str::<Turn>(&with(r#""workdir":"w""#)).is_err());
        let block = LINE.replace(r#""text":"hi""#, r#""text":"hi","extra":1"#);
        assert!(serde_json::from_str::<Turn>(&block).is_err());
    }
}
