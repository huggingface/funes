//! Secret scanning and redaction, kept modular: the detection engine sits behind the
//! [`SecretScanner`] trait, so redaction and the pre-publish gate are written against the trait,
//! not against any one tool — a different engine drops in without touching them.
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

/// Redact every secret in `texts`, in place, replacing each match with `[REDACTED:<detector>]`;
/// return the detector name of each distinct secret removed. Finds secrets through `scanner`, so
/// it's engine-agnostic.
pub fn redact(texts: &mut [String], scanner: &dyn SecretScanner) -> Result<Vec<String>> {
    let findings = scanner.scan(&texts.join("\n"))?;
    let mut out = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    for f in findings {
        // trufflehog normalizes a match's surrounding whitespace (a multiline key comes back with a
        // trailing newline the stored chunk lacks), so match on the trimmed value, not the raw.
        let needle = f.raw.trim();
        if needle.is_empty() || !done.insert(needle.to_string()) {
            continue;
        }
        let marker = format!("[REDACTED:{}]", f.detector);
        let mut hit = false;
        for t in texts.iter_mut() {
            if t.contains(needle) {
                *t = t.replace(needle, &marker);
                hit = true;
            }
        }
        if hit {
            out.push(f.detector);
        }
    }
    Ok(out)
}

/// Which of `texts` contain a secret, attributing each finding to its text by **line number**, not
/// by matching `raw`. The texts are scanned together in one pass (joined by `\n`); a finding's
/// reported line is mapped back through the join's per-text line ranges. This is robust where
/// [`redact`]'s `raw`-substring matching is not — an escaped or quoted key still gets attributed to
/// the right text, because the line number doesn't depend on the stored bytes matching trufflehog's
/// canonical form. `out[i]` lists the distinct detectors that fired inside `texts[i]` (empty ⇒
/// clean). Each text must be a contiguous unit (e.g. a reconstructed block), so a secret never
/// straddles two of them. Fail-closed on the scanner.
pub fn locate(texts: &[&str], scanner: &dyn SecretScanner) -> Result<Vec<Vec<String>>> {
    let mut out = vec![Vec::new(); texts.len()];
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

    let mut push = |i: usize, det: &str| {
        if !out[i].iter().any(|d| d == det) {
            out[i].push(det.to_string());
        }
    };
    for f in &findings {
        match f.line {
            // Primary: map the reported line to the text whose span contains it.
            Some(line) => {
                if let Some(i) = spans.iter().position(|&(s, e)| line >= s && line < e) {
                    push(i, &f.detector);
                }
            }
            // Fallback for a scanner that reports no line: best-effort `raw` containment, so an
            // engine without line info still flags *something* rather than silently passing.
            None => {
                let needle = f.raw.trim();
                if !needle.is_empty() {
                    for (i, t) in texts.iter().enumerate() {
                        if t.contains(needle) {
                            push(i, &f.detector);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
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

    #[test]
    fn redact_replaces_each_distinct_match_in_place() {
        let scanner = FakeScanner(vec![finding("PrivateKey", "SEKRET")]);
        let mut texts = vec!["before SEKRET after".to_string(), "clean".to_string()];
        let report = redact(&mut texts, &scanner).unwrap();
        assert_eq!(texts[0], "before [REDACTED:PrivateKey] after");
        assert_eq!(texts[1], "clean");
        assert_eq!(report, vec!["PrivateKey".to_string()]);
    }

    #[test]
    fn redact_trims_normalized_matches() {
        // trufflehog reports a multiline match with a trailing newline the stored text lacks; the
        // trimmed match must still be redacted.
        let scanner = FakeScanner(vec![finding("PrivateKey", "KEYLINE\n")]);
        let mut texts = vec!["x KEYLINE y".to_string()];
        let report = redact(&mut texts, &scanner).unwrap();
        assert_eq!(texts[0], "x [REDACTED:PrivateKey] y");
        assert_eq!(report, vec!["PrivateKey".to_string()]);
    }

    #[test]
    fn locate_maps_findings_to_texts_by_line() {
        // Two single-line texts then a 3-line one. A finding on line 4 (the second line of text 2)
        // must be attributed to text 2 — by line range, never by `raw`.
        let texts = ["alpha", "beta", "k1\nk2\nk3"];
        let scanner = FakeScanner(vec![finding_at("PrivateKey", 4)]);
        let refs: Vec<&str> = texts.to_vec();
        let hits = locate(&refs, &scanner).unwrap();
        assert_eq!(hits[0], Vec::<String>::new());
        assert_eq!(hits[1], Vec::<String>::new());
        assert_eq!(hits[2], vec!["PrivateKey".to_string()]);
    }

    #[test]
    fn locate_does_not_use_raw_so_escaping_cannot_hide_a_secret() {
        // The regression that leaked: `raw` (real newlines) is NOT a substring of the stored text
        // (escaped `\n`). `redact`'s containment misses it; `locate`'s line number does not.
        let escaped = "[tool_result] key: -----BEGIN-----\\nABC\\n-----END-----";
        let texts = ["clean chatter", escaped, "more chatter"];
        let scanner = FakeScanner(vec![Finding {
            detector: "PrivateKey".to_string(),
            raw: "-----BEGIN-----\nABC\n-----END-----".to_string(), // real newlines: not in `escaped`
            line: Some(2),
        }]);
        let refs: Vec<&str> = texts.to_vec();
        let hits = locate(&refs, &scanner).unwrap();
        assert_eq!(
            hits[1],
            vec!["PrivateKey".to_string()],
            "escaped secret must still be located"
        );
        assert!(hits[0].is_empty() && hits[2].is_empty());
    }

    #[test]
    fn locate_falls_back_to_raw_when_a_finding_has_no_line() {
        // A scanner that reports no line still flags via `raw` containment rather than passing.
        let texts = ["nothing here", "contains SEKRET inline"];
        let scanner = FakeScanner(vec![finding("AWS", "SEKRET")]);
        let refs: Vec<&str> = texts.to_vec();
        let hits = locate(&refs, &scanner).unwrap();
        assert!(hits[0].is_empty());
        assert_eq!(hits[1], vec!["AWS".to_string()]);
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
