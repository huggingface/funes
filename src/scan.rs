//! Secret scanning and redaction, behind the [`SecretScanner`] trait so the detection engine is
//! pluggable. Built on it: [`scan_blocks`] (locate findings per text by line, in one pass),
//! [`excise`] (redact matched values from a text), and [`summary`]/[`detectors`] for user-facing
//! messages. Everything operates on plain text and [`Finding`]s; the module depends on no other
//! funes module.
//!
//! funes ships one scanner, [`Trufflehog`]. Discovery is via `$FUNES_TRUFFLEHOG`, then `$PATH`,
//! then common install dirs — funes runs as an IDE-spawned MCP server, whose `$PATH` is often
//! stripped of `/opt/homebrew/bin` and the like, so PATH alone isn't enough.
//!
//! Fail-closed: a scanner that can't run errors rather than reporting "clean".

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// trufflehog's exit code (under `--fail`) when at least one result was found.
const FOUND: i32 = 183;

/// A potential secret a scanner flagged; never logged.
///
/// - `raw` is the matched value in the scanner's *canonical* form (real newlines, surrounding
///   quotes/escapes stripped). It's used to redact where it byte-matches the stored text — but it
///   often won't (a key stored with escaped `\n`, or quoted), so it's unreliable for *locating*.
/// - `line` is the 1-based line in the scanned blob where the match begins. This is robust to
///   escaping, so it — not `raw` — is what [`locate`] uses to attribute a finding to its source.
#[derive(Debug, Clone)]
pub struct Finding {
    pub detector: String,
    pub raw: String,
    pub line: Option<usize>,
}

/// A pluggable secret-detection engine. The rest of this module — redaction, the allowlist, the
/// gate — depends only on this, never on a specific tool.
pub trait SecretScanner {
    /// Every potential secret in `blob`. Fail-closed: `Err` means "couldn't scan", never "clean".
    fn scan(&self, blob: &str) -> Result<Vec<Finding>>;
}

/// The default engine: trufflehog, run offline (no verification) over the text.
pub struct Trufflehog {
    bin: PathBuf,
}

impl Trufflehog {
    /// Locate the trufflehog binary; fail-closed if none is found.
    pub fn find() -> Result<Self> {
        Ok(Self {
            bin: find_in(|k| std::env::var_os(k), |p| p.is_file())?,
        })
    }
}

impl SecretScanner for Trufflehog {
    fn scan(&self, blob: &str) -> Result<Vec<Finding>> {
        let dir = tempfile::tempdir().context("creating a temp dir for the secret scan")?;
        std::fs::write(dir.path().join("blob.txt"), blob).context("staging text for the scan")?;
        let out = Command::new(&self.bin)
            .arg("filesystem")
            .arg(dir.path())
            .args([
                "--json",
                "--no-verification",
                "--no-update",
                "--fail",
                "--results=verified,unknown,unverified",
            ])
            .output()
            .with_context(|| format!("running trufflehog at {}", self.bin.display()))?;

        match out.status.code() {
            Some(0) => Ok(Vec::new()),
            Some(FOUND) => Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(parse_finding)
                .collect()),
            other => bail!(
                "trufflehog exited abnormally ({other:?}); refusing to treat the text as clean:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }
}

/// Parse one trufflehog JSON result line into a [`Finding`], pulling the detector, the raw match,
/// and the filesystem line number (`SourceMetadata.Data.Filesystem.line`). Non-result lines (no
/// `DetectorName`) and unparseable lines are dropped.
fn parse_finding(line: &str) -> Option<Finding> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let detector = v.get("DetectorName")?.as_str()?.to_string();
    if detector.is_empty() {
        return None;
    }
    Some(Finding {
        detector,
        raw: v.get("Raw").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
        line: v
            .pointer("/SourceMetadata/Data/Filesystem/line")
            .and_then(|x| x.as_u64())
            .map(|n| n as usize),
    })
}

/// Candidate order for the trufflehog binary: `$FUNES_TRUFFLEHOG` → `$PATH` entries → common
/// install dirs; the first that exists wins. Split out so discovery is testable without touching
/// the real environment or filesystem. Errors if none exists — the scan is mandatory, never a
/// silent pass.
fn find_in(env: impl Fn(&str) -> Option<OsString>, exists: impl Fn(&Path) -> bool) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(over) = env("FUNES_TRUFFLEHOG") {
        candidates.push(PathBuf::from(over));
    }
    if let Some(path) = env("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join("trufflehog")));
    }
    if let Some(home) = env("HOME").map(PathBuf::from) {
        candidates.push(home.join("go/bin/trufflehog"));
        candidates.push(home.join(".local/bin/trufflehog"));
    }
    candidates.extend(
        [
            "/opt/homebrew/bin/trufflehog",
            "/usr/local/bin/trufflehog",
            "/usr/bin/trufflehog",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    candidates.into_iter().find(|p| exists(p)).ok_or_else(|| {
        anyhow!(
            "trufflehog not found (checked $FUNES_TRUFFLEHOG, $PATH, Homebrew, /usr/local/bin, \
             /usr/bin, ~/go/bin, ~/.local/bin). The secret scan is mandatory — refusing to proceed \
             unscanned. Install it (https://github.com/trufflesecurity/trufflehog) or set \
             FUNES_TRUFFLEHOG=/path/to/trufflehog."
        )
    })
}

/// The one place a scanner is invoked for *block-level* detection. Scans `texts` together in a
/// single pass and attributes each finding to the text it falls in — by **line number**, not by
/// matching `raw`. The line number is robust where `raw`-substring matching is not: an escaped or
/// quoted key still maps to the right text, because the line doesn't depend on the stored bytes
/// matching trufflehog's canonical form. `texts` must each be a contiguous unit (a reconstructed
/// block), so a secret never straddles two. `out[i]` holds the findings located in `texts[i]`.
/// Redaction ([`excise`]) and the drop/hold-back decisions are derived from this result without
/// re-scanning. Fail-closed on the scanner.
pub fn scan_blocks(texts: &[&str], scanner: &dyn SecretScanner) -> Result<Vec<Vec<Finding>>> {
    let mut out: Vec<Vec<Finding>> = (0..texts.len()).map(|_| Vec::new()).collect();
    if texts.is_empty() {
        return Ok(out);
    }
    let findings = scanner.scan(&texts.join("\n"))?;
    if findings.is_empty() {
        return Ok(out);
    }

    // Line span [start, end) of each text in the joined blob (1-based). `join("\n")` puts one
    // newline between texts, so text i+1 begins on the line right after text i ends.
    let mut spans = Vec::with_capacity(texts.len());
    let mut cursor = 1usize;
    for t in texts {
        let lines = t.split('\n').count().max(1);
        spans.push((cursor, cursor + lines));
        cursor += lines;
    }

    for f in findings {
        match f.line {
            // Primary: map the reported line to the text whose span contains it.
            Some(line) => {
                if let Some(i) = spans.iter().position(|&(s, e)| line >= s && line < e) {
                    out[i].push(f);
                }
            }
            // Fallback for a scanner that reports no line: best-effort `raw` containment, so an
            // engine without line info still flags *something* rather than silently passing.
            None => {
                let needle = f.raw.trim().to_string();
                if !needle.is_empty() {
                    for (i, t) in texts.iter().enumerate() {
                        if t.contains(&needle) {
                            out[i].push(f.clone());
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The outcome of excising one block's secrets; see [`excise`].
pub struct Redaction {
    /// `text` with every matched secret value replaced by `[REDACTED:<detector>]`.
    pub text: String,
    /// The detector name of each distinct secret removed (deduplicated by value), for the
    /// user-facing summary.
    pub removed_detectors: Vec<String>,
    /// Whether *every* finding was excised. `false` means a secret's bytes didn't match trufflehog's
    /// canonical `raw` (e.g. a key stored with escaped newlines) and so survives — the caller must
    /// drop the text rather than store it. Not derivable from `removed_detectors.len()` vs
    /// `findings.len()`: the detectors are deduplicated by value, so repeated findings collapse and
    /// the lengths legitimately differ even when nothing survived.
    pub fully_redacted: bool,
}

/// Excise each finding's value from `text`, replacing it with `[REDACTED:<detector>]`. Takes findings
/// from [`scan_blocks`] and never re-scans: it inserts a marker (never splices fragments together), so
/// excision can't manufacture a new secret, and the fail-closed push gate re-scans every block before
/// any upload regardless.
pub fn excise(text: &str, findings: &[Finding]) -> Redaction {
    let mut redacted = text.to_string();
    let mut removed_detectors = Vec::new();
    let mut fully_redacted = true;
    let mut seen: HashSet<String> = HashSet::new();
    for f in findings {
        // trufflehog normalizes a match's surrounding whitespace (a multiline key comes back with a
        // trailing newline the stored chunk lacks), so match on the trimmed value, not the raw.
        let needle = f.raw.trim();
        if needle.is_empty() {
            fully_redacted = false; // nothing to match on — can't excise it
            continue;
        }
        if !seen.insert(needle.to_string()) {
            continue;
        }
        // Decide presence against the *original* `text`, not the progressively-redacted result: a
        // value that was a byte-substring of an already-excised one is genuinely gone (count it
        // removed, replace is a no-op), whereas a value that was never present (e.g. a key stored
        // with escaped newlines) marks the text unredactable so the caller drops it.
        if text.contains(needle) {
            redacted = redacted.replace(needle, &format!("[REDACTED:{}]", f.detector));
            removed_detectors.push(f.detector.clone());
        } else {
            fully_redacted = false;
        }
    }
    Redaction {
        text: redacted,
        removed_detectors,
        fully_redacted,
    }
}

/// The distinct detector names among `findings`, in first-seen order — what each secret-bearing
/// block contributes to a held-back/scrubbed message.
pub fn detectors(findings: &[Finding]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in findings {
        if !out.iter().any(|d| d == &f.detector) {
            out.push(f.detector.clone());
        }
    }
    out
}

/// `Detector×count` over the given detector names, for a user-facing held-back/scrubbed message.
/// Counts each occurrence, so the caller controls multiplicity (per block, per secret, …).
pub fn summary<'a>(detectors: impl IntoIterator<Item = &'a str>) -> String {
    let mut by: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for d in detectors {
        *by.entry(d).or_default() += 1;
    }
    by.iter()
        .map(|(d, n)| format!("{d}×{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scanner with canned findings — lets the redaction/allowlist logic be tested without
    /// trufflehog (and exercises the trait seam).
    struct FakeScanner(Vec<Finding>);
    impl SecretScanner for FakeScanner {
        fn scan(&self, _blob: &str) -> Result<Vec<Finding>> {
            Ok(self.0.clone())
        }
    }

    fn finding(detector: &str, raw: &str) -> Finding {
        Finding {
            detector: detector.to_string(),
            raw: raw.to_string(),
            line: None,
        }
    }

    fn finding_at(detector: &str, line: usize) -> Finding {
        Finding {
            detector: detector.to_string(),
            raw: String::new(),
            line: Some(line),
        }
    }

    #[test]
    fn discovery_precedence() {
        // $FUNES_TRUFFLEHOG wins outright, even with a PATH set.
        let env = |k: &str| match k {
            "FUNES_TRUFFLEHOG" => Some(OsString::from("/custom/trufflehog")),
            "PATH" => Some(OsString::from("/usr/bin")),
            _ => None,
        };
        assert_eq!(find_in(env, |_| true).unwrap(), PathBuf::from("/custom/trufflehog"));

        // No override: the first existing PATH entry wins.
        let env = |k: &str| (k == "PATH").then(|| OsString::from("/aa:/bb"));
        assert_eq!(
            find_in(env, |p| p.starts_with("/aa") || p.starts_with("/bb")).unwrap(),
            PathBuf::from("/aa/trufflehog")
        );

        // Nothing anywhere: fail-closed with an actionable message.
        let err = find_in(|_| None, |_| false).unwrap_err().to_string();
        assert!(err.contains("not found") && err.contains("FUNES_TRUFFLEHOG"), "{err}");
    }

    /// Detectors located in each text — the view `scan_blocks` callers derive for "which texts are
    /// dirty, with what".
    fn detectors(texts: &[&str], scanner: &dyn SecretScanner) -> Vec<Vec<String>> {
        scan_blocks(texts, scanner)
            .unwrap()
            .into_iter()
            .map(|fs| fs.into_iter().map(|f| f.detector).collect())
            .collect()
    }

    #[test]
    fn excise_replaces_each_distinct_match_in_place() {
        let r = excise("before SEKRET after", &[finding("PrivateKey", "SEKRET")]);
        assert_eq!(r.text, "before [REDACTED:PrivateKey] after");
        assert_eq!(r.removed_detectors, vec!["PrivateKey".to_string()]);
        assert!(r.fully_redacted);
    }

    #[test]
    fn excise_trims_normalized_matches() {
        // trufflehog reports a multiline match with a trailing newline the stored text lacks; the
        // trimmed match must still be redacted.
        let r = excise("x KEYLINE y", &[finding("PrivateKey", "KEYLINE\n")]);
        assert_eq!(r.text, "x [REDACTED:PrivateKey] y");
        assert_eq!(r.removed_detectors, vec!["PrivateKey".to_string()]);
        assert!(r.fully_redacted);
    }

    #[test]
    fn excise_keeps_a_block_when_one_value_is_a_substring_of_another() {
        // Two findings where one value contains the other. Excising the longer also removes the
        // shorter; presence is judged against the original text, so the shorter still counts as
        // removed and the block is redacted (kept), not dropped.
        let r = excise(
            "x SECRET-TOKEN y",
            &[finding("Long", "SECRET-TOKEN"), finding("Short", "TOKEN")],
        );
        assert!(!r.text.contains("TOKEN"), "both values must be gone: {}", r.text);
        assert!(
            r.fully_redacted,
            "a transitively-removed value must not mark the block unredactable"
        );
    }

    #[test]
    fn excise_flags_a_value_it_cannot_match() {
        // The escaped case: the canonical value (real newlines) isn't a substring of the stored text
        // (escaped `\n`), so it can't be excised — `fully_redacted` must be false so the caller drops it.
        let stored = "key: -----BEGIN-----\\nABC\\n-----END-----";
        let finding = Finding {
            detector: "PrivateKey".into(),
            raw: "-----BEGIN-----\nABC\n-----END-----".into(), // real newlines
            line: Some(1),
        };
        let r = excise(stored, &[finding]);
        assert_eq!(r.text, stored, "nothing matched, so nothing changed");
        assert!(r.removed_detectors.is_empty());
        assert!(
            !r.fully_redacted,
            "an unremovable secret must mark the text for dropping"
        );
    }

    #[test]
    fn scan_blocks_maps_findings_to_texts_by_line() {
        // Two single-line texts then a 3-line one. A finding on line 4 (the second line of text 2)
        // must be attributed to text 2 — by line range, never by `raw`.
        let scanner = FakeScanner(vec![finding_at("PrivateKey", 4)]);
        let hits = detectors(&["alpha", "beta", "k1\nk2\nk3"], &scanner);
        assert_eq!(hits[0], Vec::<String>::new());
        assert_eq!(hits[1], Vec::<String>::new());
        assert_eq!(hits[2], vec!["PrivateKey".to_string()]);
    }

    #[test]
    fn scan_blocks_does_not_use_raw_so_escaping_cannot_hide_a_secret() {
        // The regression that leaked: `raw` (real newlines) is NOT a substring of the stored text
        // (escaped `\n`). Value-matching misses it; the reported line number does not.
        let escaped = "[tool_result] key: -----BEGIN-----\\nABC\\n-----END-----";
        let scanner = FakeScanner(vec![Finding {
            detector: "PrivateKey".to_string(),
            raw: "-----BEGIN-----\nABC\n-----END-----".to_string(), // real newlines: not in `escaped`
            line: Some(2),
        }]);
        let hits = detectors(&["clean chatter", escaped, "more chatter"], &scanner);
        assert_eq!(
            hits[1],
            vec!["PrivateKey".to_string()],
            "escaped secret must still be located"
        );
        assert!(hits[0].is_empty() && hits[2].is_empty());
    }

    #[test]
    fn scan_blocks_falls_back_to_raw_when_a_finding_has_no_line() {
        // A scanner that reports no line still flags via `raw` containment rather than passing.
        let scanner = FakeScanner(vec![finding("AWS", "SEKRET")]);
        let hits = detectors(&["nothing here", "contains SEKRET inline"], &scanner);
        assert!(hits[0].is_empty());
        assert_eq!(hits[1], vec!["AWS".to_string()]);
    }

    #[test]
    fn summary_counts_detectors() {
        assert_eq!(summary(["PrivateKey", "AWS", "AWS"]), "AWS×2, PrivateKey×1");
        assert_eq!(summary(std::iter::empty()), "");
    }

    #[test]
    fn flags_a_generated_private_key() {
        let Ok(scanner) = Trufflehog::find() else {
            eprintln!("skip: trufflehog not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id_ed25519");
        // A throwaway key generated at test time — never committed, so funes ships no secret.
        let made = Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-q", "-f"])
            .arg(&key)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skip: ssh-keygen unavailable");
            return;
        }
        let blob = std::fs::read_to_string(&key).unwrap();
        let findings = scanner.scan(&blob).expect("scan");
        assert!(
            findings.iter().any(|f| f.detector == "PrivateKey"),
            "expected a PrivateKey finding, got {findings:?}"
        );
    }
}
