//! Secret scanning and redaction, behind the [`SecretScanner`] trait so the detection engine is
//! pluggable. Built on it: [`scan_blocks`] (scan every block in one pass and report what was found
//! in each), [`excise`] (redact matched values from a text), and [`summary`]/[`detectors`] for
//! user-facing messages. Everything operates on plain text and [`Finding`]s; the module depends on
//! no other funes module.
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
///   against the stored text. It says nothing about *where* the match is — an escaped or quoted key
///   won't match verbatim.
/// - `decoder` is the decoder trufflehog used to uncover the match (`PLAIN`, `BASE64`, …). `PLAIN`
///   means the secret bytes are in the text directly (possibly string-escaped); anything else means
///   it was inside an encoded region [`excise`] won't reconstruct, so that block is dropped, not redacted.
#[derive(Debug, Clone)]
pub struct Finding {
    pub detector: String,
    pub raw: String,
    pub decoder: String,
}

/// A pluggable secret-detection engine. The rest of this module — redaction, the allowlist, the
/// gate — depends only on this, never on a specific tool.
pub trait SecretScanner {
    /// What was found in each of `texts`, one entry per text: `out[i]` holds the secrets in
    /// `texts[i]`. Fail-closed: `Err` means "couldn't scan", never "clean".
    fn scan(&self, texts: &[&str]) -> Result<Vec<Vec<Finding>>>;
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

    /// Scan already staged complete blocks in one pass. Files must be named `0` through
    /// `blocks - 1`, with no other directory entries. Missing or unexpected files fail closed.
    pub(crate) fn scan_directory(&self, dir: &Path, blocks: usize) -> Result<Vec<Vec<Finding>>> {
        if blocks == 0 {
            return Ok(Vec::new());
        }
        let mut staged = 0;
        for entry in std::fs::read_dir(dir).context("reading the staged secret scan directory")? {
            let entry = entry.context("reading a staged secret scan entry")?;
            let name = entry.file_name();
            let valid_name = name
                .to_str()
                .and_then(|n| n.parse::<usize>().ok().filter(|&i| i < blocks && i.to_string() == n))
                .is_some();
            if !valid_name || !entry.file_type()?.is_file() {
                bail!("unexpected entry in the staged secret scan directory; refusing to treat the text as clean");
            }
            staged += 1;
        }
        if staged != blocks {
            bail!(
                "secret scan expected {blocks} staged text(s) but found {staged}; refusing to treat the text as clean"
            );
        }
        let out = Command::new(&self.bin)
            .arg("filesystem")
            .arg(dir)
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

        let records = interpret_scan_output(out.status.code(), &out.stdout, &out.stderr)?;
        group_by_file(records, blocks)
    }
}

impl SecretScanner for Trufflehog {
    /// One file per text, one run over the directory: trufflehog names the file each secret came
    /// from. One file for all of them would leave only the reported line to tell them apart, and
    /// that line is counted in the decoder's output, not in the text funes holds.
    fn scan(&self, texts: &[&str]) -> Result<Vec<Vec<Finding>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let dir = tempfile::tempdir().context("creating a temp dir for the secret scan")?;
        for (i, text) in texts.iter().enumerate() {
            std::fs::write(dir.path().join(i.to_string()), text)
                .with_context(|| format!("staging text {i} for the scan"))?;
        }
        self.scan_directory(dir.path(), texts.len())
    }
}

/// Which text each secret came from, read off the file trufflehog reported: each text is staged
/// under its own index. Fail-closed on a file that is not one of them — a secret no text owns can
/// be neither redacted nor held back.
fn group_by_file(records: Vec<(String, Finding)>, texts: usize) -> Result<Vec<Vec<Finding>>> {
    let mut out: Vec<Vec<Finding>> = (0..texts).map(|_| Vec::new()).collect();
    for (file, finding) in records {
        let name = Path::new(&file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let i = name.parse::<usize>().ok().filter(|&i| i < texts).ok_or_else(|| {
            anyhow!(
                "trufflehog reported a {} finding in {name:?}, which is not one of the {texts} \
                     staged text(s); refusing to treat the text as clean",
                finding.detector
            )
        })?;
        out[i].push(finding);
    }
    Ok(out)
}

fn interpret_scan_output(status: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Result<Vec<(String, Finding)>> {
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

fn parse_findings(stdout: &[u8]) -> Result<Vec<(String, Finding)>> {
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

/// Parse one trufflehog JSON result line into the file it was found in and the [`Finding`] itself.
fn parse_finding(line: &str) -> Result<(String, Finding)> {
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
    let file = v
        .pointer("/SourceMetadata/Data/Filesystem/file")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("missing or non-string SourceMetadata.Data.Filesystem.file"))?
        .to_string();
    let decoder = required_string("DecoderName")?;
    if decoder.is_empty() {
        bail!("empty DecoderName");
    }
    Ok((
        file,
        Finding {
            detector,
            raw: required_string("Raw")?,
            decoder,
        },
    ))
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

/// Block-level detection for in-memory callers: scans `texts` in one pass and
/// returns what it found in each of them. `texts` must each be a contiguous unit (a reconstructed
/// block), so a secret never straddles two. Redaction ([`excise`]) and the drop/hold-back decisions
/// come from this result without re-scanning. Fail-closed on the scanner, and on one that answers
/// for fewer texts than it was given — the rest would go unscanned.
pub fn scan_blocks(texts: &[&str], scanner: &dyn SecretScanner) -> Result<Vec<Vec<Finding>>> {
    let per_text = scanner.scan(texts)?;
    if per_text.len() != texts.len() {
        bail!(
            "secret scanner answered for {} text(s) but was given {}; refusing to treat them as clean",
            per_text.len(),
            texts.len()
        );
    }
    Ok(per_text)
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

    fn finding(detector: &str, raw: &str) -> Finding {
        Finding {
            detector: detector.to_string(),
            raw: raw.to_string(),
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

    #[test]
    fn scan_directory_rejects_incomplete_or_unexpected_staging_before_running() {
        let scanner = Trufflehog {
            bin: PathBuf::from("a-scanner-that-does-not-exist"),
        };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0"), "text").unwrap();
        let error = scanner.scan_directory(dir.path(), 2).unwrap_err().to_string();
        assert!(error.contains("expected 2 staged text(s) but found 1"), "{error}");
        for name in ["01", "extra", "2"] {
            let extra = tempfile::tempdir().unwrap();
            std::fs::write(extra.path().join(name), "text").unwrap();
            let error = scanner.scan_directory(extra.path(), 2).unwrap_err().to_string();
            assert!(error.contains("unexpected entry"), "{name}: {error}");
        }
        let nested = tempfile::tempdir().unwrap();
        std::fs::create_dir(nested.path().join("0")).unwrap();
        assert!(scanner.scan_directory(nested.path(), 1).is_err());
    }

    #[test]
    fn scan_directory_rejects_an_unavailable_scanner() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0"), "text").unwrap();
        let scanner = Trufflehog {
            bin: dir.path().join("missing-scanner"),
        };
        let error = scanner.scan_directory(dir.path(), 1).unwrap_err().to_string();
        assert!(error.contains("running trufflehog"), "{error}");
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
            decoder: "BASE64".into(),
        };
        let r = excise(stored, &[finding]);
        assert_eq!(r.text, stored, "a non-PLAIN finding is never redacted in place");
        assert!(r.removed_detectors.is_empty());
        assert!(!r.fully_redacted, "an encoded secret must mark the text for dropping");
    }

    #[test]
    fn group_by_file_finds_the_text_a_secret_is_in() {
        let records = vec![
            ("/tmp/x/2".to_string(), finding("AWS", "SEKRET")),
            ("/tmp/x/0".to_string(), finding("PrivateKey", "KEY")),
        ];
        let out = group_by_file(records, 3).unwrap();
        assert_eq!(detectors(&out[0]), vec!["PrivateKey".to_string()]);
        assert!(out[1].is_empty());
        assert_eq!(detectors(&out[2]), vec!["AWS".to_string()]);
    }

    #[test]
    fn group_by_file_rejects_a_file_that_is_not_one_of_the_staged_texts() {
        for file in ["/tmp/x/blob.txt", "/tmp/x/9", "/tmp/x/1/inner.txt"] {
            let err = group_by_file(vec![(file.to_string(), finding("AWS", "SEKRET"))], 2)
                .unwrap_err()
                .to_string();
            assert!(err.contains("not one of the 2 staged text(s)"), "{file}: {err}");
        }
    }

    #[test]
    fn scan_blocks_rejects_a_scanner_that_answers_for_the_wrong_number_of_texts() {
        struct Short;
        impl SecretScanner for Short {
            fn scan(&self, _texts: &[&str]) -> Result<Vec<Vec<Finding>>> {
                Ok(vec![Vec::new()])
            }
        }
        let err = scan_blocks(&["alpha", "beta"], &Short).unwrap_err().to_string();
        assert!(err.contains("answered for 1 text(s) but was given 2"), "{err}");
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
        let Some(key) = throwaway_key(dir.path(), "id_ed25519") else {
            eprintln!("skip: ssh-keygen unavailable");
            return;
        };
        let found = scanner.scan(&[key.as_str()]).expect("scan");
        assert!(
            found[0].iter().any(|f| f.detector == "PrivateKey"),
            "expected a PrivateKey finding, got {:?}",
            detectors(&found[0])
        );
    }

    #[test]
    fn each_secret_is_found_in_the_text_that_holds_it() {
        let Ok(scanner) = Trufflehog::find() else {
            eprintln!("skip: trufflehog not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (Some(a), Some(b)) = (throwaway_key(dir.path(), "a"), throwaway_key(dir.path(), "b")) else {
            eprintln!("skip: ssh-keygen unavailable");
            return;
        };
        // Escaped newlines: each key is found through a decoder, which counts lines in its own output.
        let stored = |k: &str| format!("tool_result: {{\"key\":\"{}\"}}", k.replace('\n', "&#xa;"));
        let (first, second) = (stored(&a), stored(&b));
        let texts = [
            "user: the deploy failed, can you look?",
            first.as_str(),
            "assistant: nothing obvious in there.",
            second.as_str(),
            "assistant: found it, fixed.",
        ];
        let found = scanner.scan(&texts).expect("scan");
        let holding: Vec<usize> = (0..texts.len()).filter(|&i| !found[i].is_empty()).collect();
        assert_eq!(holding, vec![1, 3], "each key must be found in its own text");
    }

    /// A throwaway ed25519 key generated at test time — never committed, so funes ships no secret.
    /// `None` when ssh-keygen isn't available.
    fn throwaway_key(dir: &Path, name: &str) -> Option<String> {
        let path = dir.join(name);
        let made = Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-q", "-f"])
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        made.then(|| std::fs::read_to_string(&path).unwrap())
    }
}
