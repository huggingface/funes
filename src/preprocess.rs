//! Index-time preprocessors: transforms applied to a session's turns *before* they are chunked,
//! embedded, and stored.
//!
//! Running before chunking is deliberate for secret redaction. A secret is contiguous in its
//! block's text, but [`crate::chunk::split`] can cut a long one (an RSA key, say) across chunk
//! boundaries — and a scanner can't detect half a key. Redacting the whole block text first means
//! `split` only ever sees already-clean text. Detection is batched: one scan over every block,
//! which suits a file-backed scanner like trufflehog.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::parse::Turn;
use crate::scan::{self, SecretScanner};

/// A transform over a session's turns, applied before they are chunked.
pub trait Preprocessor {
    /// Transform `turns` in place.
    fn process(&self, turns: &mut [Turn]) -> Result<()>;
}

/// Redacts secrets from block text, replacing each with `[REDACTED:<detector>]`, and reports to
/// stderr what it removed. Best-effort: it removes a secret whose value byte-matches the stored text
/// — the common case, since transcript text carries real newlines. Anything that resists redaction
/// (e.g. a key stored with escaped `\n`) is caught downstream by the push gate, which is fail-closed.
pub struct RedactSecrets {
    scanner: Box<dyn SecretScanner>,
}

impl RedactSecrets {
    pub fn new(scanner: Box<dyn SecretScanner>) -> Self {
        Self { scanner }
    }
}

impl Preprocessor for RedactSecrets {
    fn process(&self, turns: &mut [Turn]) -> Result<()> {
        // Every block's text, flattened in a stable order, scanned in one pass.
        let mut texts: Vec<String> = turns
            .iter()
            .flat_map(|t| t.blocks.iter().map(|b| b.text.clone()))
            .collect();
        if texts.is_empty() {
            return Ok(());
        }
        let report = scan::redact(&mut texts, self.scanner.as_ref())?;
        if report.is_empty() {
            return Ok(());
        }
        let mut redacted = texts.into_iter();
        for t in turns.iter_mut() {
            for b in t.blocks.iter_mut() {
                b.text = redacted.next().expect("one redacted text per block");
            }
        }

        let sid = turns.first().map(|t| t.session_id.as_str()).unwrap_or("?");
        let mut by_detector: BTreeMap<&str, usize> = BTreeMap::new();
        for detector in &report {
            *by_detector.entry(detector.as_str()).or_default() += 1;
        }
        let summary = by_detector
            .iter()
            .map(|(d, n)| format!("{d}×{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("    redacted {} secret(s) in {sid}: {summary}", report.len());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{Block, Turn};
    use crate::scan::Finding;

    struct Fake(Vec<Finding>);
    impl SecretScanner for Fake {
        fn scan(&self, _blob: &str) -> Result<Vec<Finding>> {
            Ok(self.0.clone())
        }
    }

    fn turn_with(texts: &[&str]) -> Turn {
        Turn {
            session_id: "sess".into(),
            project: "proj".into(),
            turn_uuid: "turn".into(),
            parent_uuid: None,
            seq: 0,
            ts: String::new(),
            role: "user".into(),
            blocks: texts
                .iter()
                .map(|t| Block {
                    block_type: "text".into(),
                    text: (*t).into(),
                    tool_name: None,
                    tool_use_id: None,
                })
                .collect(),
            source_path: String::new(),
        }
    }

    #[test]
    fn redacts_every_detected_secret_in_block_text() {
        let scanner = Box::new(Fake(vec![
            Finding {
                detector: "PrivateKey".into(),
                raw: "TOPSECRET".into(),
                line: None,
            },
            Finding {
                detector: "VirusTotal".into(),
                raw: "cafef00d".into(),
                line: None,
            },
        ]));
        let mut turns = vec![turn_with(&["key=TOPSECRET hash=cafef00d"])];
        RedactSecrets::new(scanner).process(&mut turns).unwrap();
        assert_eq!(
            turns[0].blocks[0].text,
            "key=[REDACTED:PrivateKey] hash=[REDACTED:VirusTotal]"
        );
    }

    #[test]
    fn leaves_clean_turns_untouched() {
        let scanner = Box::new(Fake(vec![]));
        let mut turns = vec![turn_with(&["nothing secret here", "just chatting"])];
        RedactSecrets::new(scanner).process(&mut turns).unwrap();
        assert_eq!(turns[0].blocks[0].text, "nothing secret here");
        assert_eq!(turns[0].blocks[1].text, "just chatting");
    }
}
