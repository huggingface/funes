//! Rendering for the read commands, over `recall`'s structured results.
//!
//! [`recall_agent`]/[`get_agent`]/[`sessions_agent`]/[`scan_agent`] are the machine format —
//! byte-stable, its layout is a published contract (the `→ get` lines are parsed) and must not
//! change. Every read verb answers an agent, so that is the only format here.

use crate::commands::recall::{Hit, ScanCut, ScanResult, Session, Turn};
use std::fmt::Write as _;

/// Turns each side of a hit that a `→ get` line reaches for.
const HINT_WINDOW: i64 = 3;

/// The `--from a --to b` a `→ get` line carries for a hit at `seq`, clamped at the session's start.
fn hint_range(seq: i64) -> String {
    format!(" --from {} --to {}", (seq - HINT_WINDOW).max(0), seq + HINT_WINDOW)
}

/// The agent `recall` format: provenance header with score, a `→ get` line carrying `memory_arg`
/// (the pre-rendered ` --memory <label>` suffix, empty for the built-in guide), the full chunk
/// text, and truncated neighbor lines per hit. The chunk is never clipped — the ranking scored
/// all of it, so a preview could hide exactly the span that made it a hit; the chunker's size
/// cap bounds the payload instead. Byte-stable — the layout is a published contract.
pub fn recall_agent(note: &str, memory_arg: &str, hits: &[(Hit, f64)]) -> String {
    let mut out = note.to_string();
    for (h, score) in hits {
        let s8 = &h.session_id[..h.session_id.len().min(8)];
        let _ = writeln!(
            out,
            "[{}] {} {}/{} {}  score={:.3}",
            h.ts, h.harness, h.workdir, s8, h.block_type, score
        );
        let _ = writeln!(out, "  → get {}{}{}", h.session_id, hint_range(h.seq), memory_arg);
        let _ = writeln!(out, "{}", h.text);
        for n in &h.neighbors {
            let np: String = n.text.chars().take(160).collect();
            let _ = writeln!(out, "  ~ [{} {} seq{}] {}", n.role, n.block_type, n.seq, np);
        }
        let _ = writeln!(out, "---");
    }
    out
}

/// The agent `scan` format: a header naming the needle, the session and the window, then one line
/// per carrying block with the `→ get` that reads it. A cap is stated with the coordinate that
/// continues past it. Byte-stable.
pub fn scan_agent(note: &str, memory_arg: &str, r: &ScanResult, context: usize) -> String {
    let mut out = note.to_string();
    let scanned = match (r.from, r.to) {
        (None, None) => String::new(),
        (Some(a), Some(b)) => format!(" turns {a}-{b}"),
        (Some(a), None) => format!(" turns {a} on"),
        (None, Some(b)) => format!(" turns up to {b}"),
    };
    if r.hits.is_empty() {
        let _ = writeln!(out, "no matches for {:?} in {}{scanned}\n---", r.needle, r.session_id);
        return out;
    }
    let _ = writeln!(
        out,
        "scan {:?} in {}{scanned} — {} hits",
        r.needle,
        r.session_id,
        r.hits.len() + r.dropped
    );
    for h in &r.hits {
        let _ = writeln!(out, "[{}] {} seq{}", h.ts, h.block_type, h.seq);
        let _ = writeln!(out, "  → get {}{}{}", r.session_id, hint_range(h.seq), memory_arg);
        let _ = writeln!(out, "  {}", excerpt(&h.text, h.at, h.len, context));
    }
    match &r.cut {
        Some(ScanCut::Resume(seq)) => {
            let _ = writeln!(out, "{} more hits not shown — continue with --from {seq}", r.dropped);
        }
        Some(ScanCut::Crowded(seq)) => {
            let _ = writeln!(
                out,
                "{} more hits not shown, all in turn {seq} — read it with --from {seq} --to {seq}",
                r.dropped
            );
        }
        None => {}
    }
    let _ = writeln!(out, "---");
    out
}

/// `context` chars of the block on each side of the match, whitespace-collapsed onto one line. A
/// `…` marks each end the block runs past.
fn excerpt(text: &str, at: usize, len: usize, context: usize) -> String {
    // Stepping back `context` chars means taking the (context - 1)th char in reverse; a context of
    // zero steps nowhere and starts at the match.
    let start = if context == 0 {
        at
    } else {
        text[..at].char_indices().rev().nth(context - 1).map_or(0, |(j, _)| j)
    };
    let end = text[at..]
        .char_indices()
        .nth(len + context)
        .map_or(text.len(), |(j, _)| at + j);
    format!(
        "{}{}{}",
        if start > 0 { "… " } else { "" },
        text[start..end].split_whitespace().collect::<Vec<_>>().join(" "),
        if end < text.len() { " …" } else { "" }
    )
}

/// Characters of a session's opening prompt a listing shows.
const PROMPT_CHARS: usize = 120;

/// Characters a `sessions` listing renders. A row renders whole, so a listing bounded by `limit`
/// can still stop short of it.
const SESSIONS_BUDGET: usize = 40_000;

/// `s` collapsed onto one line and cut to `max` chars, `…` marking the cut.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// The agent `sessions` format: a row per session, oldest first, each carrying the prompt it opened
/// with on an indented second line, closed by the total. The session id is printed whole — it is the
/// payload here, not a pointer into a hint line. `total` is every session the filter matched and
/// `offset` where this page started, so the trailer can name the offset that continues.
/// Byte-stable.
pub fn sessions_agent(note: &str, sessions: &[Session], total: usize, offset: usize) -> String {
    let mut out = note.to_string();
    // `offset` walks back from the recent end, so a budget cut has to keep that same end of the page:
    // take the newest rows that fit, then print them back in reading order.
    let mut budget = SESSIONS_BUDGET.saturating_sub(out.len());
    let mut rows = Vec::new();
    for s in sessions.iter().rev() {
        let mut row = String::new();
        let _ = writeln!(
            row,
            "[{}] {} {} {} turns {}",
            s.date(),
            s.harness,
            s.origin(),
            s.turns,
            s.session_id
        );
        if !s.first_prompt.is_empty() {
            let _ = writeln!(row, "  {}", one_line(&s.first_prompt, PROMPT_CHARS));
        }
        if !rows.is_empty() && row.len() > budget {
            break;
        }
        budget = budget.saturating_sub(row.len());
        rows.push(row);
    }
    let shown = rows.len();
    out.extend(rows.into_iter().rev());
    // What is left is older than this page: `offset + shown` names the row it starts at, and the
    // ordering is total, so that offset is the same row on every call.
    let remaining = total.saturating_sub(offset + shown);
    if remaining == 0 && offset == 0 {
        let _ = writeln!(out, "---\n{total} sessions");
    } else if remaining == 0 {
        let _ = writeln!(out, "---\n{shown} of {total} sessions — the oldest match reached");
    } else {
        let _ = writeln!(
            out,
            "---\n{shown} of {total} sessions — {remaining} older: continue with --offset {}, or narrow with --repo/--since/--until",
            offset + shown
        );
    }
    out
}

/// Characters a `get` renders. A turn renders whole, so a single turn larger than this is the one
/// thing that can exceed it.
const GET_BUDGET: usize = 40_000;

/// The agent `get` format: `[ts] role seqN turn=…` headers over reassembled blocks, closed by the
/// range read and the session's size. Byte-stable.
pub fn get_agent(note: &str, turns: &[Turn], total: usize) -> String {
    let mut out = note.to_string();
    if turns.is_empty() {
        return out;
    }
    let mut shown = 0;
    for t in turns {
        let mut turn = String::new();
        let _ = writeln!(turn, "[{}] {} seq{} turn={}", t.ts, t.role, t.seq, t.turn_uuid);
        let _ = writeln!(turn, "{}", t.blocks.join("\n\n"));
        let _ = writeln!(turn, "---");
        // The first turn renders whatever it weighs; after that, only turns that fit.
        if shown > 0 && out.len() + turn.len() > GET_BUDGET {
            break;
        }
        out.push_str(&turn);
        shown += 1;
    }
    let (first, last) = (turns[0].seq, turns[shown - 1].seq);
    let _ = writeln!(out, "turns {first}-{last} of {total}");
    if shown < turns.len() {
        let _ = writeln!(
            out,
            "{} more turn(s) in range not shown — read them with --from {}",
            turns.len() - shown,
            last + 1
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::recall::Neighbor;

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
        let out = recall_agent("", " --memory hf://datasets/acme/kb", &[(h, 0.5781)]);
        assert_eq!(
            out,
            "[2026-06-19T01:29:59.000Z] claude_code -home-u-funes/01234567 text  score=0.578\n\
             \x20 → get 0123456789abcdef --from 4 --to 10 --memory hf://datasets/acme/kb\n\
             the decision was made\n\
             \x20 ~ [assistant text seq5] hello\n\
             ---\n"
        );
        // The built-in guide has no memory to name: an empty suffix keeps the hint bare.
        let bare = recall_agent("", "", &[(hit("2026-06-19T01:29:59.000Z", "text", "x"), 0.5)]);
        assert!(
            bare.contains("  → get 0123456789abcdef --from 4 --to 10\n"),
            "got: {bare}"
        );
    }

    #[test]
    fn a_hint_range_never_reaches_before_the_session() {
        // seq 0 has no turns behind it; the hint must stay runnable rather than ask for -3.
        assert_eq!(hint_range(0), " --from 0 --to 3");
        assert_eq!(hint_range(2), " --from 0 --to 5");
        assert_eq!(hint_range(9), " --from 6 --to 12");
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
    fn excerpt_with_no_context_shows_the_match_alone() {
        // A context of zero asks for the match and nothing around it, not the char before it.
        assert_eq!(excerpt("xbeta gamma", 1, 4, 0), "… beta …");
        // One char of context reaches the start of the block, so nothing is elided on the left.
        assert_eq!(excerpt("xbeta gamma", 1, 4, 1), "xbeta …");
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
            get_agent("", &[t], 70),
            "[2026-06-19T01:29:59.000Z] assistant seq3 turn=t-1\nfirst\n\nsecond\n---\n\
             turns 3-3 of 70\n"
        );
    }

    fn turn_at(seq: i64, body: &str) -> Turn {
        Turn {
            seq,
            turn_uuid: format!("t-{seq}"),
            ts: "2026-06-19T01:29:59.000Z".to_string(),
            role: "assistant".to_string(),
            blocks: vec![body.to_string()],
        }
    }

    #[test]
    fn get_agent_leaves_a_turn_that_does_not_fit() {
        // The range is bounded by rendered bytes, not by a turn count: two of these cannot both
        // fit, so the second is left for the next read with the coordinate to ask for it.
        let big = "x".repeat(GET_BUDGET * 3 / 4);
        let turns: Vec<Turn> = (0..3).map(|i| turn_at(i, &big)).collect();
        let out = get_agent("", &turns, 100);
        assert!(
            out.len() <= GET_BUDGET,
            "a read of fitting turns stays inside: {}",
            out.len()
        );
        assert!(
            out.contains("\nturns 0-0 of 100\n"),
            "states what it read: {}",
            &out[out.len() - 120..]
        );
        assert!(
            out.contains("2 more turn(s) in range not shown — read them with --from 1"),
            "names the resume coordinate: {}",
            &out[out.len() - 120..]
        );
    }

    #[test]
    fn get_agent_fits_every_turn_it_can() {
        // The stop is the byte limit, not a turn count: small turns all render.
        let turns: Vec<Turn> = (0..6).map(|i| turn_at(i, "small")).collect();
        let out = get_agent("", &turns, 6);
        assert!(out.contains("\nturns 0-5 of 6\n"), "got: {out}");
        assert!(!out.contains("not shown"), "nothing was left out: {out}");
    }

    #[test]
    fn get_agent_always_renders_one_turn() {
        // One turn always renders, however large.
        let huge = "x".repeat(GET_BUDGET * 2);
        let out = get_agent("", &[turn_at(4, &huge)], 9);
        assert!(out.contains(&huge), "the turn is rendered whole");
        assert!(
            out.trim_end().ends_with("turns 4-4 of 9"),
            "got: {}",
            &out[out.len() - 60..]
        );
    }

    fn session_at(i: usize) -> Session {
        Session {
            session_id: format!("session-{i:03}"),
            ts: "2026-06-19T01:29:59.000Z".to_string(),
            workdir: "/home/u/funes".to_string(),
            harness: "codex".to_string(),
            repo: format!("owner/{}", "r".repeat(100)),
            turns: 1,
            first_prompt: "x".repeat(PROMPT_CHARS * 2),
        }
    }

    fn listed_session_ids(out: &str) -> Vec<usize> {
        out.lines()
            .filter(|line| line.starts_with('['))
            .map(|line| line.rsplit_once("session-").unwrap().1.parse().unwrap())
            .collect()
    }

    #[test]
    fn sessions_agent_budget_keeps_the_recent_end_of_a_page() {
        let sessions: Vec<Session> = (0..250).map(session_at).collect();
        let page = |offset: usize| {
            let end = sessions.len().saturating_sub(offset);
            &sessions[end.saturating_sub(200)..end]
        };

        let first = listed_session_ids(&sessions_agent("", page(0), sessions.len(), 0));
        assert!(first.len() < page(0).len(), "the fixture must cross the byte budget");
        assert_eq!(first.last(), Some(&249), "a cut must retain the newest session");

        let second = listed_session_ids(&sessions_agent("", page(first.len()), sessions.len(), first.len()));
        assert!(
            first.iter().all(|id| !second.contains(id)),
            "successive pages must not overlap: {first:?} then {second:?}"
        );
        assert_eq!(second.last().map(|id| id + 1), first.first().copied());
    }
}
