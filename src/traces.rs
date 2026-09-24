//! Reading agent sessions: where they come from ([`source`]), the readers of the formats funes
//! accepts, and the trace model they all produce.
//!
//! A session becomes a sequence of [`Turn`]s, each carrying typed [`Block`]s. Everything downstream
//! — chunk → embed → store → recall — operates on that shape, so the model is source-agnostic and
//! lives here, at the root of the readers that fill it. An agent's own transcripts are converted
//! into [`funes_jsonl`] by its integration, outside funes, so the readers here take that and Hub
//! parquet.

pub mod funes_jsonl;
pub mod harness;
pub mod jsonl;
pub mod parquet;
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

#[derive(Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, PartialEq, Serialize, Deserialize)]
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
    /// Who produced this session: any `[a-z0-9_-]` id a turns file carries — an integration names
    /// itself, and the rows an older funes wrote say `claude_code`.
    pub harness: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line of the spec's own example, minus `format`.
    const LINE: &str = r#"{"session_id":"s","turn_uuid":"t-1","seq":0,"ts":"2026-09-18T09:41:07Z","role":"user","harness":"opencode","blocks":[{"block_type":"text","text":"hi"}]}"#;

    fn with(field: &str) -> String {
        format!("{{{field},{}", &LINE[1..])
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
