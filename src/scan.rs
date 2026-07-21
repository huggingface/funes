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

/// trufflehog's decoder name for a secret found directly in the text (not inside base64/UTF-16/…).
/// Only these are redacted in place; an encoded match can't be reconstructed, so its block is dropped.
const PLAIN: &str = "PLAIN";

/// A potential secret a scanner flagged; never logged.
///
/// - `raw` is the matched value in the scanner's *canonical* form (real newlines, surrounding
///   quotes/escapes stripped). [`excise`] redacts by byte-matching it (or its JSON-escaped form)
///   against the stored text; for *locating* a finding it's unreliable — an escaped or quoted key
///   won't match verbatim — so location goes by `line`, not `raw`.
/// - `line` is the 1-based line in the scanned blob where the match begins. This is robust to
///   escaping, so it — not `raw` — is what [`scan_blocks`] uses to attribute a finding to its source.
/// - `decoder` is the decoder trufflehog used to uncover the match (`PLAIN`, `BASE64`, …). `PLAIN`
///   means the secret bytes are in the text directly (possibly string-escaped); anything else means
///   it was inside an encoded region [`excise`] won't reconstruct, so that block is dropped, not redacted.
#[derive(Debug, Clone)]
pub struct Finding {
    pub detector: String,
    pub raw: String,
    pub line: Option<usize>,
    pub decoder: String,
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
                "--fail-on-scan-errors",
                "--results=verified,unknown,unverified",
            ])
            .output()
            .with_context(|| format!("running trufflehog at {}", self.bin.display()))?;

        interpret_scan_output(out.status.code(), &out.stdout, &out.stderr)
    }
}

fn interpret_scan_output(status: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Result<Vec<Finding>> {
    match status {
        Some(0) if stdout.iter().all(u8::is_ascii_whitespace) => Ok(Vec::new()),
        Some(0) => bail!(
            "trufflehog exited successfully but emitted unexpected result data; refusing to treat the text as clean"
        ),
        Some(FOUND) => parse_findings(stdout),
        other => bail!(
            "trufflehog exited abnormally ({other:?}); refusing to treat the text as clean:\n{}",
            String::from_utf8_lossy(stderr).trim()
        ),
    }
}

fn parse_findings(stdout: &[u8]) -> Result<Vec<Finding>> {
    let text = std::str::from_utf8(stdout).map_err(|e| {
        anyhow!(
            "trufflehog emitted non-UTF-8 result data near byte {}; refusing to treat the text as clean",
            e.valid_up_to()
        )
    })?;
    let mut findings = Vec::new();
    for (record, line) in text.lines().filter(|line| !line.trim().is_empty()).enumerate() {
        findings.push(parse_finding(line).with_context(|| {
            format!(
                "trufflehog result record {} does not match the supported schema; refusing to treat the text as clean",
                record + 1
            )
        })?);
    }
    if findings.is_empty() {
        bail!("trufflehog exited {FOUND} but emitted no valid findings; refusing to treat the text as clean");
    }
    Ok(findings)
}

/// Parse one trufflehog JSON result line into a [`Finding`].
fn parse_finding(line: &str) -> Result<Finding> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).context("invalid JSON result record")?;
    let required_string = |field: &str| -> Result<String> {
        v.get(field)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("missing or non-string {field}"))
    };
    let detector = required_string("DetectorName")?;
    if detector.is_empty() {
        bail!("empty DetectorName");
    }
    let line = v
        .pointer("/SourceMetadata/Data/Filesystem/line")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| anyhow!("missing or invalid SourceMetadata.Data.Filesystem.line"))?;
    let line = usize::try_from(line).context("filesystem line does not fit this platform")?;
    if line == 0 {
        bail!("filesystem line must be 1-based");
    }
    let decoder = required_string("DecoderName")?;
    if decoder.is_empty() {
        bail!("empty DecoderName");
    }
    Ok(Finding {
        detector,
        raw: required_string("Raw")?,
        line: Some(line),
        decoder,
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

    for (finding, f) in findings.into_iter().enumerate() {
        match f.line {
            // Primary: map the reported line to the text whose span contains it.
            Some(line) => {
                let i = spans.iter().position(|&(s, e)| line >= s && line < e).ok_or_else(|| {
                    anyhow!(
                        "secret-scanner finding {} reports line {line} outside the scanned text; refusing to continue",
                        finding + 1
                    )
                })?;
                out[i].push(f);
            }
            // Fall back to raw containment when the scanner has no line information.
            None => {
                let needle = f.raw.trim().to_string();
                if needle.is_empty() {
                    bail!(
                        "secret-scanner finding {} has neither a line nor a usable match; refusing to continue",
                        finding + 1
                    );
                }
                let mut mapped = false;
                for (i, t) in texts.iter().enumerate() {
                    if t.contains(&needle) {
                        out[i].push(f.clone());
                        mapped = true;
                    }
                }
                if !mapped {
                    bail!(
                        "secret-scanner finding {} could not be attributed to scanned text; refusing to continue",
                        finding + 1
                    );
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
    /// Whether *every* finding was excised. `false` means a secret survives — either it was found
    /// inside an encoded region (a non-PLAIN decoder, e.g. base64) [`excise`] won't reconstruct, or
    /// none of the byte forms it tries (canonical or JSON-escaped) matched — so the caller must drop
    /// the text rather than store it. Not derivable from `removed_detectors.len()` vs `findings.len()`:
    /// the detectors are deduplicated by value, so repeated findings collapse and the lengths
    /// legitimately differ even when nothing survived.
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
        // Redact in place only when trufflehog found the secret directly in the text (the PLAIN
        // decoder). Its bytes may be string-escaped — a compact-JSON `tool_use` input backslash-
        // escapes a key's newlines and quotes — which `candidate_forms` covers; match against the
        // *original* `text` so a value nested in an already-excised one still counts as removed. A
        // non-PLAIN decoder (BASE64, …) means the secret sat inside an encoded region we won't
        // reconstruct, so the block is unredactable and the caller must drop it.
        let hit = if f.decoder == PLAIN {
            candidate_forms(needle).into_iter().find(|c| text.contains(c.as_str()))
        } else {
            None
        };
        match hit {
            Some(form) => {
                redacted = redacted.replace(&form, &format!("[REDACTED:{}]", f.detector));
                removed_detectors.push(f.detector.clone());
            }
            None => fully_redacted = false,
        }
    }
    Redaction {
        text: redacted,
        removed_detectors,
        fully_redacted,
    }
}

/// The byte forms a secret value can take in stored text: the canonical value trufflehog reports
/// (real newlines, unquoted), and its JSON-string escaping without the wrapping quotes — how it
/// appears inside a compact-JSON `tool_use` block, where `serde_json::to_string` backslash-escapes
/// newlines and quotes. Canonical first; the escaped form is added only when it differs.
fn candidate_forms(value: &str) -> Vec<String> {
    let mut forms = vec![value.to_string()];
    if let Ok(json) = serde_json::to_string(value) {
        let escaped = json[1..json.len() - 1].to_string(); // drop serde's wrapping quotes
        if escaped != value {
            forms.push(escaped);
        }
    }
    forms
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
            decoder: "PLAIN".into(),
        }
    }

    fn finding_at(detector: &str, line: usize) -> Finding {
        Finding {
            detector: detector.to_string(),
            raw: String::new(),
            line: Some(line),
            decoder: "PLAIN".into(),
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
    fn scanner_contract_rejects_exit_183_without_records() {
        let err = interpret_scan_output(Some(FOUND), b"", b"").unwrap_err().to_string();
        assert!(err.contains("no valid findings"), "{err}");
    }

    #[test]
    fn scanner_contract_rejects_result_data_on_a_clean_exit_without_echoing_it() {
        let secret = b"DO_NOT_ECHO";
        let err = interpret_scan_output(Some(0), secret, b"").unwrap_err().to_string();
        assert!(err.contains("unexpected result data"), "{err}");
        assert!(!err.contains("DO_NOT_ECHO"), "scanner output leaked in error: {err}");
        assert!(interpret_scan_output(Some(0), b" \n\t", b"").unwrap().is_empty());
    }

    #[test]
    fn scanner_contract_rejects_malformed_json_without_echoing_it() {
        let secret = "DO_NOT_ECHO";
        let malformed = format!("{{\"DetectorName\":\"Test\",\"Raw\":\"{secret}\"");
        let err = interpret_scan_output(Some(FOUND), malformed.as_bytes(), b"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("record 1"), "{err}");
        assert!(!err.contains(secret), "scanner output leaked in error: {err}");
    }

    #[test]
    fn scanner_contract_rejects_an_unknown_json_schema() {
        let output = br#"{"DetectorName":"Test","Raw":"DO_NOT_ECHO","DecoderName":"PLAIN"}"#;
        let err = interpret_scan_output(Some(FOUND), output, b"").unwrap_err().to_string();
        assert!(err.contains("record 1"), "{err}");
        assert!(!err.contains("DO_NOT_ECHO"), "scanner output leaked in error: {err}");
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
    fn excise_matches_a_json_escaped_value() {
        // A tool_use input is stored as compact JSON, so a key's newlines arrive as literal `\n`
        // (backslash-n) while trufflehog reports the canonical value (real newlines). excise must
        // fall back to the JSON-escaped form and redact it rather than leave it for dropping.
        let stored = "key: -----BEGIN-----\\nABC\\n-----END-----"; // literal backslash-n
        let finding = Finding {
            detector: "PrivateKey".into(),
            raw: "-----BEGIN-----\nABC\n-----END-----".into(), // real newlines
            line: Some(1),
            decoder: "PLAIN".into(),
        };
        let r = excise(stored, &[finding]);
        assert_eq!(r.text, "key: [REDACTED:PrivateKey]");
        assert!(r.fully_redacted, "the JSON-escaped form must match and redact");
    }

    #[test]
    fn excise_drops_a_non_plain_finding_even_when_its_value_appears_in_text() {
        // A BASE64 (non-PLAIN) decoder means the secret was uncovered inside an encoded blob. Even
        // when its decoded value also appears in plaintext, redacting that copy would leave the
        // encoded one live — so the block must be dropped, never kept as "fully redacted".
        let stored = "plain SEKRET and blob U0VLUkVU"; // U0VLUkVU is base64("SEKRET")
        let finding = Finding {
            detector: "Generic".into(),
            raw: "SEKRET".into(),
            line: Some(1),
            decoder: "BASE64".into(),
        };
        let r = excise(stored, &[finding]);
        assert_eq!(r.text, stored, "a non-PLAIN finding is never redacted in place");
        assert!(r.removed_detectors.is_empty());
        assert!(!r.fully_redacted, "an encoded secret must mark the text for dropping");
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
            decoder: "PLAIN".to_string(),
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
    fn scan_blocks_rejects_an_unmappable_finding() {
        let scanner = FakeScanner(vec![finding_at("PrivateKey", 99)]);
        let err = scan_blocks(&["only one line"], &scanner).unwrap_err().to_string();
        assert!(err.contains("outside the scanned text"), "{err}");
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
