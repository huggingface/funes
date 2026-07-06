//! Rendering for the read commands, over `recall`'s structured results.
//!
//! Two layouts over the same hits: [`recall_agent`]/[`get_agent`] are the machine format —
//! byte-stable, its layout is a published contract (the `→ get` lines are parsed) and must not
//! change. [`recall_human`]/[`get_human`] render for a terminal: one scannable line per hit, tool
//! chunks compressed to a deterministic one-liner, scores and UUIDs left out.

use crate::recall::{Hit, Turn};
use chrono::{DateTime, Datelike, Utc};
use std::fmt::Write as _;

/// Longest scent ever built — beyond any line budget, so final truncation is [`ellipsize`]'s job.
const SCENT_CAP: usize = 240;

/// Payload lines a tool block shows in the human `get` view before folding.
const PAYLOAD_LINES: usize = 6;

/// The agent `recall` format: provenance header with score, a `→ get` line carrying `store_arg`
/// (the pre-rendered ` --store <label>` suffix, empty for the built-in guide), a 400-char
/// preview, and truncated neighbor lines per hit. Byte-stable — the layout is a published
/// contract.
pub fn recall_agent(note: &str, store_arg: &str, hits: &[(Hit, f64)]) -> String {
    let mut out = note.to_string();
    for (h, score) in hits {
        let s8 = &h.session_id[..h.session_id.len().min(8)];
        let _ = writeln!(
            out,
            "[{}] {} {}/{} {}  score={:.3}",
            h.ts, h.harness, h.project, s8, h.block_type, score
        );
        let _ = writeln!(out, "  → get {} {}{}", h.session_id, h.turn_uuid, store_arg);
        let preview: String = h.text.chars().take(400).collect();
        let _ = writeln!(out, "{preview}");
        for n in &h.neighbors {
            let np: String = n.text.chars().take(160).collect();
            let _ = writeln!(out, "  ~ [{} {} seq{}] {}", n.role, n.block_type, n.seq, np);
        }
        let _ = writeln!(out, "---");
    }
    out
}

/// The human `recall` list: `N  date  agent  project  scent`, one line per hit, fitted to
/// `width`. Metadata dims when `color` is set; `now` anchors the date labels (years show only
/// when some hit is from another year).
pub fn recall_human(note: &str, hits: &[(Hit, f64)], color: bool, width: usize, now: DateTime<Utc>) -> String {
    let mut out = note.to_string();
    // 4 = the ordinal prefix each line carries below.
    for (i, row) in hit_rows(hits, color, width, now, 4).iter().enumerate() {
        let _ = writeln!(out, "{:>2}  {}", i + 1, row);
    }
    out
}

/// The human list rows without ordinals — one per hit, for a picker whose pointer does the
/// numbering.
pub fn recall_rows(hits: &[(Hit, f64)], color: bool, width: usize, now: DateTime<Utc>) -> Vec<String> {
    hit_rows(hits, color, width, now, 0)
}

/// One aligned row per hit — dim `date  agent  project` columns, then the scent, fitted to
/// `width` minus `indent` (the caller's own per-line prefix).
fn hit_rows(hits: &[(Hit, f64)], color: bool, width: usize, now: DateTime<Utc>, indent: usize) -> Vec<String> {
    let with_year = hits.iter().any(|(h, _)| {
        DateTime::parse_from_rfc3339(&h.ts)
            .map(|t| t.year() != now.year())
            .unwrap_or(false)
    });
    let rows: Vec<(String, &str, String)> = hits
        .iter()
        .map(|(h, _)| {
            (
                date_label(&h.ts, with_year),
                harness_display(&h.harness),
                ellipsize(project_display(&h.project), 24),
            )
        })
        .collect();
    let dw = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    let hw = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0);
    let pw = rows.iter().map(|r| r.2.chars().count()).max().unwrap_or(0);

    hits.iter()
        .zip(&rows)
        .map(|((h, _), (date, har, proj))| {
            let meta = format!("{date:<dw$}  {har:<hw$}  {proj:<pw$}");
            let budget = width.saturating_sub(indent + meta.chars().count() + 2).max(20);
            let line = ellipsize(&scent(&h.block_type, &h.text), budget);
            format!("{}  {}", dim(&meta, color), line)
        })
        .collect()
}

/// The agent `get` format: `[ts] role seqN turn=…` headers over reassembled blocks. Byte-stable.
pub fn get_agent(note: &str, turns: &[Turn]) -> String {
    let mut out = note.to_string();
    for t in turns {
        let _ = writeln!(out, "[{}] {} seq{} turn={}", t.ts, t.role, t.seq, t.turn_uuid);
        let _ = writeln!(out, "{}", t.blocks.join("\n\n"));
        let _ = writeln!(out, "---");
    }
    out
}

/// The human `get` view: a dim `── time · role ──` header per turn, prose blocks wrapped in full,
/// tool blocks compressed to a one-liner plus the payload head (an Edit's `new_string`, a Write's
/// `content`, a Bash command).
pub fn get_human(note: &str, turns: &[Turn], color: bool, width: usize) -> String {
    let mut out = note.to_string();
    for t in turns {
        let stamp: String = t.ts.chars().take(16).collect::<String>().replace('T', " ");
        let _ = writeln!(out, "{}", dim(&format!("── {stamp} · {} ──", t.role), color));
        for b in &t.blocks {
            if b.starts_with("[tool_use") {
                let payload = tool_payload(b);
                // With a payload shown below, the one-liner stays a headline; without one it
                // carries the detail itself.
                let line = match payload {
                    Some(_) => tool_use_headline(b).unwrap_or_else(|| scent("tool_use", b)),
                    None => scent("tool_use", b),
                };
                let _ = writeln!(out, "  {}", dim(&ellipsize(&line, width.saturating_sub(2)), color));
                if let Some(p) = payload {
                    let lines: Vec<&str> = p.lines().collect();
                    for l in lines.iter().take(PAYLOAD_LINES) {
                        write_wrapped(&mut out, l, width.saturating_sub(2), "  ");
                    }
                    if lines.len() > PAYLOAD_LINES {
                        let _ = writeln!(out, "  {}", dim("…", color));
                    }
                }
            } else if b.starts_with("[tool_result") {
                let line = scent("tool_result", b);
                let _ = writeln!(out, "  {}", dim(&ellipsize(&line, width.saturating_sub(2)), color));
            } else {
                for l in b.lines() {
                    write_wrapped(&mut out, l, width, "");
                }
            }
            out.push('\n');
        }
    }
    out
}

/// `s` dimmed with ANSI escapes when `color` is set, verbatim otherwise.
pub fn dim(s: &str, color: bool) -> String {
    if color {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// One line of scent for a chunk: tool chunks compress to `verb target — detail`, prose collapses
/// to its head. `text` is the chunk as stored (`[tool_use Name] {json}` for tool blocks; a split
/// fragment carries no label and reads as prose).
fn scent(block_type: &str, text: &str) -> String {
    match block_type {
        "tool_use" => tool_use_scent(text).unwrap_or_else(|| prose_head(text)),
        "tool_result" => tool_result_scent(text).unwrap_or_else(|| prose_head(text)),
        _ => prose_head(text),
    }
}

/// `Edit docs/functions.md — <new text>`, `Bash — <description> — <command>`, or `Name — <args>`;
/// None when the `[tool_use …]` label is absent (a split fragment).
fn tool_use_scent(text: &str) -> Option<String> {
    let (name, args) = tool_use_parts(text)?;
    Some(join_scent(
        &headline(name, args),
        detail(name, args).map(|d| collapse(&d)),
    ))
}

/// The headline half of a tool one-liner — verb and target, no content.
fn tool_use_headline(text: &str) -> Option<String> {
    let (name, args) = tool_use_parts(text)?;
    Some(headline(name, args))
}

/// Split `[tool_use Name] {json}` into name and args.
fn tool_use_parts(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("[tool_use ")?;
    let (name, args) = rest.split_once(']')?;
    Some((name, args.trim_start()))
}

fn headline(name: &str, args: &str) -> String {
    match json_str(args, "file_path").as_deref().map(short_path) {
        Some(p) => format!("{name} {p}"),
        None => join_scent(name, json_str(args, "description").map(|d| collapse(&d))),
    }
}

/// The content payload of a tool call, newlines intact — what the human `get` view prints under
/// the headline.
fn detail(name: &str, args: &str) -> Option<String> {
    match name {
        "Edit" | "MultiEdit" => json_str(args, "new_string").or_else(|| json_str(args, "old_string")),
        "Write" => json_str(args, "content"),
        "Bash" => json_str(args, "command"),
        // The headline already names the target file; otherwise fall back to the raw args.
        _ => match json_str(args, "file_path") {
            Some(_) => None,
            None => {
                let a = collapse(args);
                (!a.is_empty()).then_some(a)
            }
        },
    }
}

/// `Name ⇒ <first of the result>`; None when the `[tool_result…]` label is absent.
fn tool_result_scent(text: &str) -> Option<String> {
    let rest = text.strip_prefix("[tool_result")?;
    let (label, body) = rest.split_once(']')?;
    let name = label.trim();
    let head = collapse(body);
    Some(if name.is_empty() {
        format!("⇒ {head}")
    } else {
        format!("{name} ⇒ {head}")
    })
}

/// The payload a tool block shows in the human `get` view; None for tools whose args aren't
/// content (Read, unknown tools).
fn tool_payload(text: &str) -> Option<String> {
    let (name, args) = tool_use_parts(text)?;
    match name {
        "Edit" | "MultiEdit" | "Write" | "Bash" => detail(name, args).filter(|d| !d.trim().is_empty()),
        _ => None,
    }
}

fn join_scent(head: &str, detail: Option<String>) -> String {
    match detail.filter(|d| !d.is_empty()) {
        Some(d) => format!("{} — {}", head.trim(), d),
        None => head.trim().to_string(),
    }
}

/// The string value of `"key"` in a JSON-ish blob, decoded loosely and tolerant of truncation: a
/// chunk split can cut the JSON mid-string, so a missing closing quote returns the partial value.
fn json_str(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let at = s.find(&pat)? + pat.len();
    let rest = s[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(e) => out.push(e),
                None => break,
            },
            _ => out.push(c),
        }
    }
    Some(out)
}

/// The last two path components — enough to place a file without the machine prefix.
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.rsplit('/').filter(|s| !s.is_empty()).take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

/// Whitespace-collapsed head of `s`, capped at [`SCENT_CAP`].
fn collapse(s: &str) -> String {
    let mut out = String::new();
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
        if out.chars().count() >= SCENT_CAP {
            break;
        }
    }
    out
}

/// Prose scent: whitespace-collapsed head. A split fragment of a tool chunk carries JSON-escaped
/// whitespace as literal `\n`/`\t` — treat those as separators too.
fn prose_head(s: &str) -> String {
    collapse(&s.replace("\\n", " ").replace("\\t", " "))
}

/// Truncate to `max` chars, backing up to a word boundary when one lands past the midpoint,
/// appending `…`.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    let kept = match cut.rfind(' ') {
        Some(i) if i * 2 > cut.len() => &cut[..i],
        _ => cut.as_str(),
    };
    format!("{}…", kept.trim_end())
}

/// `Jun 19`, or `Jun 19 2025` when years matter; an unparseable ts falls back to its date prefix.
fn date_label(ts: &str, with_year: bool) -> String {
    match DateTime::parse_from_rfc3339(ts) {
        Ok(t) if with_year => t.format("%b %e %Y").to_string(),
        Ok(t) => t.format("%b %e").to_string(),
        Err(_) => ts.chars().take(10).collect(),
    }
}

/// The stored harness facet in its short spelling.
fn harness_display(h: &str) -> &str {
    match h {
        "claude_code" => "claude",
        other => other,
    }
}

/// A munged absolute path (Claude project dirs turn `/` into `-`, so they start with `-`) shows
/// its last segment; any other project name shows whole.
fn project_display(p: &str) -> &str {
    if p.starts_with('-') {
        p.rsplit('-').find(|s| !s.is_empty()).unwrap_or(p)
    } else {
        p
    }
}

/// Write one logical line word-wrapped to `width`, each row prefixed with `indent`; a blank line
/// stays blank. Lines already within `width` pass through verbatim (indentation intact).
fn write_wrapped(out: &mut String, line: &str, width: usize, indent: &str) {
    if line.trim().is_empty() {
        out.push('\n');
        return;
    }
    if line.chars().count() <= width {
        let _ = writeln!(out, "{indent}{line}");
        return;
    }
    let width = width.max(20);
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in line.split_whitespace() {
        let wlen = word.chars().count();
        if wlen > width {
            if cur_len > 0 {
                let _ = writeln!(out, "{indent}{cur}");
            }
            let cs: Vec<char> = word.chars().collect();
            let mut i = 0;
            while cs.len() - i > width {
                let row: String = cs[i..i + width].iter().collect();
                let _ = writeln!(out, "{indent}{row}");
                i += width;
            }
            cur = cs[i..].iter().collect();
            cur_len = cur.chars().count();
            continue;
        }
        if cur_len > 0 && cur_len + 1 + wlen > width {
            let _ = writeln!(out, "{indent}{cur}");
            cur.clear();
            cur_len = 0;
        }
        if cur_len > 0 {
            cur.push(' ');
            cur_len += 1;
        }
        cur.push_str(word);
        cur_len += wlen;
    }
    if !cur.is_empty() {
        let _ = writeln!(out, "{indent}{cur}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::Neighbor;
    use chrono::TimeZone;

    fn hit(ts: &str, block_type: &str, text: &str) -> Hit {
        Hit {
            text: text.to_string(),
            session_id: "0123456789abcdef".to_string(),
            project: "-home-u-funes".to_string(),
            turn_uuid: "aaaa-bbbb".to_string(),
            seq: 7,
            ts: ts.to_string(),
            block_type: block_type.to_string(),
            harness: "claude_code".to_string(),
            neighbors: vec![],
        }
    }

    // The agent format is a published contract — its `→ get` line is parsed. Pin it
    // byte-for-byte.
    #[test]
    fn agent_format_is_byte_stable() {
        let mut h = hit("2026-06-19T01:29:59.000Z", "text", "the decision was made");
        h.neighbors.push(Neighbor {
            seq: 5,
            role: "assistant".to_string(),
            block_type: "text".to_string(),
            text: "hello".to_string(),
        });
        let out = recall_agent("", " --store hf://datasets/acme/kb", &[(h, 0.5781)]);
        assert_eq!(
            out,
            "[2026-06-19T01:29:59.000Z] claude_code -home-u-funes/01234567 text  score=0.578\n\
             \x20 → get 0123456789abcdef aaaa-bbbb --store hf://datasets/acme/kb\n\
             the decision was made\n\
             \x20 ~ [assistant text seq5] hello\n\
             ---\n"
        );
        // The built-in guide has no store to name: an empty suffix keeps the hint bare.
        let bare = recall_agent("", "", &[(hit("2026-06-19T01:29:59.000Z", "text", "x"), 0.5)]);
        assert!(bare.contains("  → get 0123456789abcdef aaaa-bbbb\n"), "got: {bare}");
    }

    #[test]
    fn agent_prepends_note_and_truncates_preview() {
        let long: String = "x".repeat(500);
        let out = recall_agent("remote down\n", "", &[(hit("bad-ts", "text", &long), 1.0)]);
        assert!(out.starts_with("remote down\n[bad-ts]"));
        // 400-char preview cap.
        assert!(out.contains(&"x".repeat(400)));
        assert!(!out.contains(&"x".repeat(401)));
    }

    #[test]
    fn get_agent_is_byte_stable() {
        let t = Turn {
            seq: 3,
            turn_uuid: "t-1".to_string(),
            ts: "2026-06-19T01:29:59.000Z".to_string(),
            role: "assistant".to_string(),
            blocks: vec!["first".to_string(), "second".to_string()],
        };
        assert_eq!(
            get_agent("", &[t]),
            "[2026-06-19T01:29:59.000Z] assistant seq3 turn=t-1\nfirst\n\nsecond\n---\n"
        );
    }

    #[test]
    fn scent_compresses_an_edit() {
        let s = scent(
            "tool_use",
            r####"[tool_use Edit] {"file_path":"/home/lane/MythosMini/docs/functions.md","old_string":"a","new_string":"### Attention.attend_selected\n\n**Purpose:** Computes sparse attention"}"####,
        );
        assert_eq!(
            s,
            "Edit docs/functions.md — ### Attention.attend_selected **Purpose:** Computes sparse attention"
        );
    }

    #[test]
    fn scent_bash_prefers_description() {
        let s = scent(
            "tool_use",
            r#"[tool_use Bash] {"command":"cargo test -q","description":"Run the suite"}"#,
        );
        assert_eq!(s, "Bash — Run the suite — cargo test -q");
    }

    #[test]
    fn scent_survives_json_cut_mid_string() {
        // A chunk split can end the stored text in the middle of a JSON string.
        let s = scent(
            "tool_use",
            r#"[tool_use Edit] {"file_path":"/a/b/c.rs","new_string":"let x = compute("#,
        );
        assert_eq!(s, "Edit b/c.rs — let x = compute(");
    }

    #[test]
    fn scent_unlabeled_fragment_reads_as_prose() {
        // split_idx > 0 chunks carry no [tool_use …] label.
        let s = scent("tool_use", "  continuation   of a\nsplit blob  ");
        assert_eq!(s, "continuation of a split blob");
    }

    #[test]
    fn scent_flattens_escaped_whitespace_in_fragments() {
        // A fragment cut from inside a JSON string carries \n as two literal characters.
        let s = scent("tool_use", r"ormed input.\n\n**Callers:** `Block.forward`");
        assert_eq!(s, "ormed input. **Callers:** `Block.forward`");
    }

    #[test]
    fn scent_tool_result_takes_the_head() {
        let s = scent("tool_result", "[tool_result Edit] The file was updated\nmore lines");
        assert_eq!(s, "Edit ⇒ The file was updated more lines");
        assert_eq!(scent("tool_result", "[tool_result] ok"), "⇒ ok");
    }

    #[test]
    fn scent_prose_collapses_whitespace() {
        assert_eq!(scent("thinking", " a\n b\t c "), "a b c");
    }

    #[test]
    fn human_is_one_line_per_hit_with_columns() {
        let now = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap();
        let hits = vec![
            (
                hit(
                    "2026-06-19T01:29:59.000Z",
                    "thinking",
                    "apply top-k selection per query token",
                ),
                0.9,
            ),
            (
                hit(
                    "2026-06-18T01:00:00.000Z",
                    "tool_use",
                    r#"[tool_use Read] {"file_path":"/x/y.rs"}"#,
                ),
                0.8,
            ),
        ];
        let out = recall_human("", &hits, false, 100, now);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            " 1  Jun 19  claude  funes  apply top-k selection per query token"
        );
        assert_eq!(lines[1], " 2  Jun 18  claude  funes  Read x/y.rs");
        // No agent plumbing in the human list.
        assert!(!out.contains("score=") && !out.contains("→ get"));
    }

    #[test]
    fn human_shows_years_when_hits_span_them() {
        let now = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap();
        let hits = vec![(hit("2025-12-09T01:00:00.000Z", "text", "old"), 0.5)];
        let out = recall_human("", &hits, false, 100, now);
        assert!(out.contains("Dec  9 2025"), "got: {out}");
    }

    #[test]
    fn human_dims_metadata_when_colored() {
        let now = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap();
        let hits = vec![(hit("2026-06-19T01:29:59.000Z", "text", "x"), 0.5)];
        assert!(recall_human("", &hits, true, 100, now).contains("\x1b[2m"));
        assert!(!recall_human("", &hits, false, 100, now).contains("\x1b[2m"));
    }

    #[test]
    fn human_fits_width() {
        let now = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap();
        let long = "word ".repeat(100);
        let hits = vec![(hit("2026-06-19T01:29:59.000Z", "text", &long), 0.5)];
        for line in recall_human("", &hits, false, 80, now).lines() {
            assert!(line.chars().count() <= 80, "overlong: {line}");
        }
    }

    #[test]
    fn get_human_compresses_tool_blocks_and_wraps_prose() {
        let t = Turn {
            seq: 3,
            turn_uuid: "t-1".to_string(),
            ts: "2026-06-19T01:29:59.000Z".to_string(),
            role: "assistant".to_string(),
            blocks: vec![
                "plain reasoning text".to_string(),
                r#"[tool_use Edit] {"file_path":"/a/docs/f.md","new_string":"line one\nline two"}"#.to_string(),
                "[tool_result Edit] The file was updated".to_string(),
            ],
        };
        let out = get_human("", &[t], false, 100);
        assert!(out.contains("── 2026-06-19 01:29 · assistant ──"));
        assert!(out.contains("plain reasoning text"));
        // Headline plus payload lines, not raw JSON.
        assert!(out.contains("  Edit docs/f.md"));
        assert!(out.contains("  line one"));
        assert!(out.contains("  line two"));
        assert!(!out.contains("file_path"));
        assert!(out.contains("  Edit ⇒ The file was updated"));
    }

    #[test]
    fn get_human_folds_long_payloads() {
        let payload: String = (1..=10).map(|i| format!("l{i}\\n")).collect();
        let t = Turn {
            seq: 1,
            turn_uuid: "t".to_string(),
            ts: "2026-06-19T01:29:59.000Z".to_string(),
            role: "assistant".to_string(),
            blocks: vec![format!(
                r#"[tool_use Write] {{"file_path":"/a.rs","content":"{payload}"}}"#
            )],
        };
        let out = get_human("", &[t], false, 100);
        assert!(out.contains("l6"));
        assert!(!out.contains("l7"));
        assert!(out.contains('…'));
    }

    #[test]
    fn project_display_unmangles_claude_dirs() {
        assert_eq!(project_display("-Users-dcorvoysier-dev-funes"), "funes");
        assert_eq!(project_display("Fable-5-traces"), "Fable-5-traces");
    }

    #[test]
    fn ellipsize_prefers_word_boundaries() {
        assert_eq!(ellipsize("short", 10), "short");
        let e = ellipsize("alpha beta gamma delta", 15);
        assert!(e.chars().count() <= 15);
        assert!(e.ends_with('…'));
        assert_eq!(e, "alpha beta…");
    }
}
