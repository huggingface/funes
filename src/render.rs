//! Rendering for the read commands, over `recall`'s structured results.
//!
//! [`recall_agent`]/[`get_agent`] are the machine format — byte-stable, its layout is a published
//! contract (the `→ get` lines are parsed) and must not change. [`get_human`] renders a turn for
//! a terminal: tool chunks compressed to a deterministic one-liner, prose wrapped, marks
//! highlighted.

use crate::recall::{Hit, Turn};
use std::fmt::Write as _;

/// Longest scent ever built — beyond any line budget, so final truncation is [`ellipsize`]'s job.
const SCENT_CAP: usize = 240;

/// Payload lines a tool block shows in the human `get` view before folding.
const PAYLOAD_LINES: usize = 6;

/// The agent `recall` format: provenance header with score, a `→ get` line carrying `store_arg`
/// (the pre-rendered ` --store <label>` suffix, empty for the built-in guide), the full chunk
/// text, and truncated neighbor lines per hit. The chunk is never clipped — the ranking scored
/// all of it, so a preview could hide exactly the span that made it a hit; the chunker's size
/// cap bounds the payload instead. Byte-stable — the layout is a published contract.
pub fn recall_agent(note: &str, store_arg: &str, hits: &[(Hit, f64)]) -> String {
    let mut out = note.to_string();
    for (h, score) in hits {
        let s8 = &h.session_id[..h.session_id.len().min(8)];
        let _ = writeln!(
            out,
            "[{}] {} {}/{} {}  score={:.3}",
            h.ts, h.harness, h.workdir, s8, h.block_type, score
        );
        let _ = writeln!(out, "  → get {} {}{}", h.session_id, h.turn_uuid, store_arg);
        let _ = writeln!(out, "{}", h.text);
        for n in &h.neighbors {
            let np: String = n.text.chars().take(160).collect();
            let _ = writeln!(out, "  ~ [{} {} seq{}] {}", n.role, n.block_type, n.seq, np);
        }
        let _ = writeln!(out, "---");
    }
    out
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
/// `content`, a Bash command). A `mark` (a matched chunk, whitespace-collapsed) is located in the
/// prose whitespace-insensitively and reverse-videoed — marks render regardless of `color`, since
/// highlighting is their whole point.
pub fn get_human(note: &str, turns: &[Turn], color: bool, width: usize, mark: Option<&str>) -> String {
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
            } else if let Some(span) = mark.and_then(|m| word_span(b, m)) {
                write_wrapped_marked(&mut out, b, width, span);
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

/// The global word-index range where `mark`'s word sequence occurs in `text`, matched
/// whitespace-insensitively. The match anchors on the first 6 words — or the whole mark when
/// shorter — and extends as far as the sequences agree. A chunk can be cut mid-word at its start,
/// so a long mark is also tried with its first word dropped; a short explicit mark must match
/// whole. None when nothing anchors — the mark belongs to some other turn.
fn word_span(text: &str, mark: &str) -> Option<std::ops::Range<usize>> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mark_words: Vec<&str> = mark.split_whitespace().collect();
    for skip in 0..=1usize.min(mark_words.len()) {
        let needle = &mark_words[skip..];
        if needle.is_empty() || (skip > 0 && needle.len() < 6) {
            break;
        }
        let anchor = needle.len().min(6);
        if let Some(i) = words.windows(anchor).position(|w| w == &needle[..anchor]) {
            let mut n = anchor;
            while n < needle.len() && i + n < words.len() && words[i + n] == needle[n] {
                n += 1;
            }
            return Some(i..i + n);
        }
    }
    None
}

/// Word-wrap a whole block to `width`, reverse-videoing the words whose global index falls in
/// `mark`. Blank lines survive; unlike [`write_wrapped`], every line is re-flowed (word indices
/// must line up with [`word_span`]'s counting).
fn write_wrapped_marked(out: &mut String, text: &str, width: usize, mark: std::ops::Range<usize>) {
    let width = width.max(20);
    let mut wi = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for word in line.split_whitespace() {
            let wlen = word.chars().count();
            if cur_len > 0 && cur_len + 1 + wlen > width {
                let _ = writeln!(out, "{cur}");
                cur.clear();
                cur_len = 0;
            }
            if cur_len > 0 {
                cur.push(' ');
                cur_len += 1;
            }
            if mark.contains(&wi) {
                let _ = write!(cur, "\x1b[7m{word}\x1b[0m");
            } else {
                cur.push_str(word);
            }
            cur_len += wlen;
            wi += 1;
        }
        if !cur.is_empty() {
            let _ = writeln!(out, "{cur}");
        }
    }
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

    fn hit(ts: &str, block_type: &str, text: &str) -> Hit {
        Hit {
            text: text.to_string(),
            session_id: "0123456789abcdef".to_string(),
            workdir: "-home-u-funes".to_string(),
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
    fn agent_prepends_note_and_keeps_full_chunk() {
        let long: String = "x".repeat(1200);
        let out = recall_agent("remote down\n", "", &[(hit("bad-ts", "text", &long), 1.0)]);
        assert!(out.starts_with("remote down\n[bad-ts]"));
        // The matched chunk is never clipped.
        assert!(out.contains(&long));
    }

    #[test]
    fn get_human_marks_the_matched_chunk() {
        let turn = |block: &str| Turn {
            seq: 1,
            turn_uuid: "t".to_string(),
            ts: "2026-06-19T01:29:59.000Z".to_string(),
            role: "assistant".to_string(),
            blocks: vec![block.to_string()],
        };
        let block = "The scores are computed first. The test checks that no future positions \
                     are selected, so the implementation must mask out future keys. Then we go on.";
        // A real chunk is cut mid-word at its start ("st" from "test") — the first mark word is
        // dropped and the rest anchors.
        let mark = "st checks that no future positions are selected, so the implementation";
        let out = get_human("", &[turn(block)], false, 100, Some(mark));
        assert!(out.contains("\u{1b}[7mchecks\u{1b}[0m"), "got: {out}");
        assert!(out.contains("\u{1b}[7mimplementation\u{1b}[0m"));
        assert!(out.contains("The scores are computed"));
        // A mark that anchors nowhere renders the turn plain.
        let plain = get_human(
            "",
            &[turn(block)],
            false,
            100,
            Some("entirely unrelated words that never anchor anywhere at all"),
        );
        assert!(!plain.contains("\u{1b}[7m"));
    }

    #[test]
    fn word_span_anchors_short_and_cut_marks() {
        let text = "alpha beta gamma delta epsilon zeta eta theta";
        // Shorter than the 6-word anchor: the whole mark must match.
        assert_eq!(word_span(text, "gamma delta"), Some(2..4));
        assert_eq!(word_span(text, "theta"), Some(7..8));
        // A short mark is not retried with its first word dropped.
        assert_eq!(word_span(text, "nope delta"), None);
        // A long mark cut mid-word at its start still anchors via the skip.
        assert_eq!(word_span(text, "pha beta gamma delta epsilon zeta eta"), Some(1..7));
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
        let out = get_human("", &[t], false, 100, None);
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
        let out = get_human("", &[t], false, 100, None);
        assert!(out.contains("l6"));
        assert!(!out.contains("l7"));
        assert!(out.contains('…'));
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
