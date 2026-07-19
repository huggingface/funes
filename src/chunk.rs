//! Render blocks to text and split into chunks.
//! All indexing is by Unicode code point (char), not bytes.

use crate::trace::Turn;
use arrow_array::{Array, Int64Array, RecordBatch, StringArray};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::HashMap;

const MAX_CHARS: usize = 1200;
/// Consecutive splits of one block share this many leading/trailing chars, so reassembly
/// (`recall::stitch`) never needs to match a longer overlap than this.
pub const OVERLAP: usize = 150;

/// Indexing tiers, cheapest-and-highest-value first: L1 `text` (onboarding), L2 `tool_use`, L3
/// `tool_result` (bulky, lowest value). A block's tier decides *when* it's indexed. `thinking` and
/// any unknown block type fold into L1 so nothing is ever dropped by tiering.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum Tier {
    Text,
    ToolUse,
    ToolResult,
}

impl Tier {
    /// Every tier, in index order — the block-type set a full index emits.
    pub const ALL: [Tier; 3] = [Tier::Text, Tier::ToolUse, Tier::ToolResult];

    /// The tier a block type belongs to; unknown types fold into L1.
    pub fn of_block(block_type: &str) -> Tier {
        match block_type {
            "tool_use" => Tier::ToolUse,
            "tool_result" => Tier::ToolResult,
            _ => Tier::Text,
        }
    }
}

#[derive(Clone)]
pub struct Chunk {
    pub id: String,
    pub text: String,
    pub session_id: String,
    pub workdir: String,
    pub turn_uuid: String,
    pub parent_uuid: Option<String>,
    pub seq: i64,
    pub ts: String,
    pub role: String,
    pub block_type: String,
    pub tool_name: Option<String>,
    pub source_path: String,
    pub block_idx: i64,
    pub split_idx: i64,
    pub harness: String,
    /// The session's source repo(s) as `owner/name`, space-joined; empty when unresolvable.
    pub repo: String,
}

/// A missing tool name renders as the literal "None".
fn py_opt(o: &Option<String>) -> &str {
    o.as_deref().unwrap_or("None")
}

fn render(block_type: &str, text: &str, tool_name: &Option<String>) -> String {
    match block_type {
        "tool_use" => format!("[tool_use {}] {}", py_opt(tool_name), text).trim().to_string(),
        "tool_result" => {
            let label = match tool_name {
                Some(n) => format!("tool_result {n}"),
                None => "tool_result".to_string(),
            };
            format!("[{label}] {text}").trim().to_string()
        }
        _ => text.to_string(),
    }
}

/// Last code-point index of `target` in `chars[start..end)`, or -1 if absent.
fn rfind(chars: &[char], target: char, start: usize, end: usize) -> i64 {
    (start..end)
        .rev()
        .find(|&i| chars[i] == target)
        .map(|i| i as i64)
        .unwrap_or(-1)
}

fn split(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.trim().chars().collect();
    let n = chars.len();
    if n == 0 {
        return vec![];
    }
    if n <= MAX_CHARS {
        return vec![chars.iter().collect()];
    }
    let half = (MAX_CHARS / 2) as i64;
    let mut pieces = Vec::new();
    let mut start = 0usize;
    while start < n {
        let mut end = (start + MAX_CHARS).min(n);
        if end < n {
            let mut brk = rfind(&chars, '\n', start, end);
            if brk <= start as i64 + half {
                brk = rfind(&chars, ' ', start, end);
            }
            if brk > start as i64 + half {
                end = brk as usize;
            }
        }
        let piece: String = chars[start..end].iter().collect::<String>().trim().to_string();
        if !piece.is_empty() {
            pieces.push(piece);
        }
        if end >= n {
            break;
        }
        start = end.saturating_sub(OVERLAP).max(start + 1);
    }
    pieces
}

fn cid(session_id: &str, turn_uuid: &str, block_idx: i64, split_idx: i64) -> String {
    let raw = format!("{session_id}:{turn_uuid}:{block_idx}:{split_idx}");
    let mut h = Sha1::new();
    h.update(raw.as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

/// Join two consecutive splits of one block, dropping the shared [`OVERLAP`] region once. The
/// inverse of the overlap [`split`] introduces; the seam never exceeds `OVERLAP`, so a longer
/// spurious match in periodic text can't swallow real content.
pub(crate) fn stitch(a: &str, b: &str) -> String {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let max_k = ac.len().min(bc.len()).min(OVERLAP);
    for k in (1..=max_k).rev() {
        if ac[ac.len() - k..] == bc[..k] {
            return ac.iter().chain(bc[k..].iter()).collect();
        }
    }
    format!("{a}{b}")
}

/// Group `chunks` into their blocks, keyed by (session, turn, block_idx). Returns, in first-seen
/// block order, the indices into `chunks` for each block, ordered by `split_idx` — so a caller can
/// [`reconstruct`] each block and map a per-block result back to its rows.
fn group_blocks(chunks: &[Chunk]) -> Vec<Vec<usize>> {
    let mut order: Vec<(String, String, i64)> = Vec::new();
    let mut groups: HashMap<(String, String, i64), Vec<usize>> = HashMap::new();
    for (i, c) in chunks.iter().enumerate() {
        let key = (c.session_id.clone(), c.turn_uuid.clone(), c.block_idx);
        let entry = groups.entry(key.clone()).or_default();
        if entry.is_empty() {
            order.push(key);
        }
        entry.push(i);
    }
    order
        .into_iter()
        .map(|k| {
            let mut idxs = groups.remove(&k).unwrap();
            idxs.sort_by_key(|&i| chunks[i].split_idx);
            idxs
        })
        .collect()
}

/// Reconstruct a block's contiguous text from its `pieces` (in `split_idx` order), undoing the
/// overlap [`split`] added. The inverse of `split`: a secret that `split` cut across two chunks is
/// whole again here, so a scanner sees it. `pieces` must be one block's splits, ordered.
fn reconstruct(pieces: &[&str]) -> String {
    let mut iter = pieces.iter();
    match iter.next() {
        None => String::new(),
        Some(first) => iter.fold(first.to_string(), |acc, p| stitch(&acc, p)),
    }
}

/// Reconstruct [`Chunk`]s from stored rows (all columns), so the store can be rewritten without its
/// source. The `vector` column is dropped — rewritten rows are re-embedded.
pub(crate) fn chunks_from_batches(batches: &[RecordBatch]) -> Vec<Chunk> {
    let sv = |b: &RecordBatch, name: &str, i: usize| -> String {
        b.column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .map(|c| c.value(i).to_string())
            .unwrap_or_default()
    };
    let so = |b: &RecordBatch, name: &str, i: usize| -> Option<String> {
        b.column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .filter(|c| !c.is_null(i))
            .map(|c| c.value(i).to_string())
    };
    let iv = |b: &RecordBatch, name: &str, i: usize| -> i64 {
        b.column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
            .map(|c| c.value(i))
            .unwrap_or(0)
    };
    let mut out = Vec::new();
    for b in batches {
        for i in 0..b.num_rows() {
            out.push(Chunk {
                id: sv(b, "id", i),
                text: sv(b, "text", i),
                session_id: sv(b, "session_id", i),
                workdir: sv(b, "workdir", i),
                turn_uuid: sv(b, "turn_uuid", i),
                parent_uuid: so(b, "parent_uuid", i),
                seq: iv(b, "seq", i),
                ts: sv(b, "ts", i),
                role: sv(b, "role", i),
                block_type: sv(b, "block_type", i),
                tool_name: so(b, "tool_name", i),
                source_path: sv(b, "source_path", i),
                block_idx: iv(b, "block_idx", i),
                split_idx: iv(b, "split_idx", i),
                harness: sv(b, "harness", i),
                repo: sv(b, "repo", i),
            });
        }
    }
    out
}

/// Group `chunks` into blocks and reconstruct each block's contiguous text in one step — the single
/// entry point both the push gate and `scrub` use to scan whole blocks. Returns, in first-seen block
/// order, each block's chunk indices (ordered by `split_idx`) paired with its reconstructed text, so
/// a per-block scan result maps straight back to the rows it covers.
pub(crate) fn reconstruct_blocks(chunks: &[Chunk]) -> Vec<(Vec<usize>, String)> {
    group_blocks(chunks)
        .into_iter()
        .map(|idxs| {
            let pieces: Vec<&str> = idxs.iter().map(|&i| chunks[i].text.as_str()).collect();
            let text = reconstruct(&pieces);
            (idxs, text)
        })
        .collect()
}

/// Re-chunk a block's (already rendered) `text` after it changed — e.g. a secret was redacted out —
/// reusing the block's identity (session, turn, block_idx, role, …) from `template`. Ids and
/// `split_idx` are regenerated from the new split count, so they stay coordinate-stable. An empty
/// result means nothing indexable survived.
pub(crate) fn resplit(template: &Chunk, text: &str) -> Vec<Chunk> {
    split(text)
        .into_iter()
        .enumerate()
        .map(|(si, piece)| Chunk {
            id: cid(&template.session_id, &template.turn_uuid, template.block_idx, si as i64),
            text: piece,
            split_idx: si as i64,
            ..(*template).clone()
        })
        .collect()
}

pub fn chunks_from_turns(turns: &[Turn], tiers: &[Tier], include_thinking: bool) -> Vec<Chunk> {
    let mut out = Vec::new();
    for turn in turns {
        for (bi, block) in turn.blocks.iter().enumerate() {
            // block_idx counts every block, so skipping one here (tier or thinking) never
            // renumbers the rest.
            if !tiers.contains(&Tier::of_block(&block.block_type)) {
                continue;
            }
            if block.block_type == "thinking" && !include_thinking {
                continue;
            }
            let rendered = render(&block.block_type, &block.text, &block.tool_name);
            for (si, piece) in split(&rendered).into_iter().enumerate() {
                out.push(Chunk {
                    id: cid(&turn.session_id, &turn.turn_uuid, bi as i64, si as i64),
                    text: piece,
                    session_id: turn.session_id.clone(),
                    workdir: turn.workdir.clone(),
                    turn_uuid: turn.turn_uuid.clone(),
                    parent_uuid: turn.parent_uuid.clone(),
                    seq: turn.seq,
                    ts: turn.ts.clone(),
                    role: turn.role.clone(),
                    block_type: block.block_type.clone(),
                    tool_name: block.tool_name.clone(),
                    source_path: turn.source_path.clone(),
                    block_idx: bi as i64,
                    split_idx: si as i64,
                    harness: turn.harness.clone(),
                    repo: String::new(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Block, Turn};

    fn block(block_type: &str, text: &str, tool_name: Option<&str>) -> Block {
        Block {
            block_type: block_type.into(),
            text: text.into(),
            tool_name: tool_name.map(str::to_string),
            tool_use_id: None,
        }
    }

    fn turn(blocks: Vec<Block>) -> Turn {
        Turn {
            session_id: "sess".into(),
            workdir: "proj".into(),
            turn_uuid: "uuid".into(),
            parent_uuid: None,
            seq: 7,
            ts: "2026-01-01T00:00:00Z".into(),
            role: "assistant".into(),
            blocks,
            source_path: "/x.jsonl".into(),
            harness: "claude_code".into(),
        }
    }

    #[test]
    fn render_labels_tool_blocks() {
        assert_eq!(
            render("tool_use", r#"{"a":1}"#, &Some("Bash".into())),
            r#"[tool_use Bash] {"a":1}"#
        );
        assert_eq!(render("tool_use", "{}", &None), "[tool_use None] {}");
        assert_eq!(
            render("tool_result", "out", &Some("Read".into())),
            "[tool_result Read] out"
        );
        assert_eq!(render("tool_result", "out", &None), "[tool_result] out");
        assert_eq!(render("text", "hello", &None), "hello");
    }

    #[test]
    fn split_keeps_short_text_whole() {
        assert_eq!(split("just a line"), vec!["just a line".to_string()]);
        assert!(split("   ").is_empty());
    }

    #[test]
    fn split_overlaps_long_text_within_max() {
        let text: String = (0..600).map(|i| format!("word{i} ")).collect();
        let pieces = split(&text);
        assert!(pieces.len() > 1, "long text should split");
        for p in &pieces {
            assert!(p.chars().count() <= MAX_CHARS, "piece exceeds MAX_CHARS");
        }
        // Consecutive pieces share the overlap region: the start of piece[1] (which
        // begins OVERLAP chars before piece[0] ended) appears inside piece[0].
        let head: String = pieces[1].chars().take(20).collect();
        assert!(pieces[0].contains(&head), "consecutive pieces should overlap");
    }

    #[test]
    fn cid_is_stable_and_distinct() {
        assert_eq!(cid("s", "u", 0, 0), cid("s", "u", 0, 0));
        assert_ne!(cid("s", "u", 0, 0), cid("s", "u", 0, 1));
        assert_eq!(cid("s", "u", 0, 0).len(), 16);
    }

    #[test]
    fn stitch_drops_overlap_once() {
        let overlap = "the quick brown fox jumps";
        let a = format!("HEAD {overlap}");
        let b = format!("{overlap} TAIL");
        assert_eq!(stitch(&a, &b), "HEAD the quick brown fox jumps TAIL");
    }

    #[test]
    fn stitch_does_not_over_merge_repetitive_text() {
        // Periodic text: many suffix==prefix lengths match. The seam is bounded by OVERLAP, so a
        // longer spurious match must not swallow real content.
        let unit = "abcabcabc ";
        let a = unit.repeat(60); // > OVERLAP chars, all periodic
        let b = unit.repeat(60);
        let joined = stitch(&a, &b);
        // Reassembly must not be shorter than the longer input (no content dropped).
        assert!(joined.chars().count() >= a.chars().count());
    }

    #[test]
    fn stitch_no_overlap_concatenates() {
        assert_eq!(stitch("alpha", "beta"), "alphabeta");
    }

    #[test]
    fn stitch_recovers_overlap_with_trimmed_seam_whitespace() {
        // The real shared region is "the quick brown fox " (trailing space), but split() trims it
        // off `a`'s end and the leading space off `b`. stitch must still match the shorter overlap
        // and not duplicate it (no "...foxfox..." and no doubled "the quick brown fox").
        let a = "HEAD the quick brown fox"; // trailing space trimmed
        let b = "the quick brown fox TAIL"; // leading already flush
        assert_eq!(stitch(a, b), "HEAD the quick brown fox TAIL");
    }

    #[test]
    fn reconstruct_inverts_split() {
        // A long block that splits into several overlapping pieces must reassemble to the original
        // (trimmed) text — so a scanner sees a secret split() had cut across chunk boundaries.
        let text: String = (0..600).map(|i| format!("word{i} ")).collect();
        let pieces = split(&text);
        assert!(pieces.len() > 1, "precondition: text must split");
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        assert_eq!(reconstruct(&refs), text.trim());
    }

    #[test]
    fn reconstruct_handles_single_and_empty() {
        assert_eq!(reconstruct(&["whole block"]), "whole block");
        assert_eq!(reconstruct(&[]), "");
    }

    #[test]
    fn resplit_regenerates_ids_matching_a_fresh_chunking() {
        // Re-chunking a redacted block must yield exactly the ids/split_idx that chunks_from_turns
        // would produce for that text — so a scrub's delete-by-id + append stays coordinate-stable.
        let long: String = (0..600).map(|i| format!("word{i} ")).collect();
        let t = turn(vec![block("text", &long, None)]);
        let fresh = chunks_from_turns(std::slice::from_ref(&t), &Tier::ALL, true);
        let template = fresh[0].clone();
        let rendered = render("text", &long, &None);
        let re = resplit(&template, &rendered);
        assert_eq!(re.len(), fresh.len());
        for (a, b) in re.iter().zip(&fresh) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.split_idx, b.split_idx);
            assert_eq!(a.text, b.text);
        }
    }

    #[test]
    fn block_idx_counts_dropped_thinking_blocks() {
        // text(0), thinking(1), text(2). With thinking excluded, the surviving text
        // blocks must keep block_idx 0 and 2 — dropping a block does not renumber.
        let t = turn(vec![
            block("text", "first", None),
            block("thinking", "secret", None),
            block("text", "third", None),
        ]);
        let with = chunks_from_turns(std::slice::from_ref(&t), &Tier::ALL, true);
        assert_eq!(with.len(), 3);
        assert_eq!(with.iter().map(|c| c.block_idx).collect::<Vec<_>>(), vec![0, 1, 2]);

        let without = chunks_from_turns(std::slice::from_ref(&t), &Tier::ALL, false);
        assert_eq!(without.len(), 2);
        assert_eq!(without.iter().map(|c| c.block_idx).collect::<Vec<_>>(), vec![0, 2]);
        assert!(without.iter().all(|c| c.block_type != "thinking"));
    }

    #[test]
    fn tier_of_block_maps_and_orders() {
        assert_eq!(Tier::of_block("text"), Tier::Text);
        assert_eq!(Tier::of_block("thinking"), Tier::Text);
        assert_eq!(Tier::of_block("tool_use"), Tier::ToolUse);
        assert_eq!(Tier::of_block("tool_result"), Tier::ToolResult);
        assert_eq!(Tier::of_block("mystery"), Tier::Text); // unknown folds into L1, never dropped
        assert!(Tier::Text < Tier::ToolUse && Tier::ToolUse < Tier::ToolResult);
    }

    #[test]
    fn tiers_filter_selects_only_requested_block_types() {
        let t = turn(vec![
            block("text", "decision", None),
            block("tool_use", r#"{"cmd":1}"#, Some("Bash")),
            block("tool_result", "output", Some("Bash")),
        ]);
        let kinds = |cs: &[Chunk]| cs.iter().map(|c| c.block_type.clone()).collect::<Vec<_>>();
        assert_eq!(
            kinds(&chunks_from_turns(std::slice::from_ref(&t), &[Tier::Text], true)),
            vec!["text"]
        );
        assert_eq!(
            kinds(&chunks_from_turns(std::slice::from_ref(&t), &[Tier::ToolUse], true)),
            vec!["tool_use"]
        );
        assert_eq!(
            kinds(&chunks_from_turns(std::slice::from_ref(&t), &[Tier::ToolResult], true)),
            vec!["tool_result"]
        );
        assert_eq!(
            kinds(&chunks_from_turns(std::slice::from_ref(&t), &Tier::ALL, true)),
            vec!["text", "tool_use", "tool_result"]
        );
    }

    #[test]
    fn chunk_ids_are_tier_pass_independent() {
        // The core invariant behind tier backfill: a block's id/coordinate is the same whether a
        // pass emits every tier or only that block's tier — so a later L3 pass dedups exactly
        // against what a full index would have written, with no gaps or renumbering.
        let t = turn(vec![
            block("text", "decision", None),
            block("tool_use", r#"{"cmd":1}"#, Some("Bash")),
            block("tool_result", "output", Some("Bash")),
        ]);
        let all = chunks_from_turns(std::slice::from_ref(&t), &Tier::ALL, true);
        let only_result = chunks_from_turns(std::slice::from_ref(&t), &[Tier::ToolResult], true);
        let from_all: Vec<&Chunk> = all.iter().filter(|c| c.block_type == "tool_result").collect();
        assert_eq!(only_result.len(), from_all.len());
        assert!(!only_result.is_empty());
        for (a, b) in only_result.iter().zip(from_all) {
            assert_eq!(a.id, b.id, "tool_result id must match across tier passes");
            assert_eq!(a.block_idx, b.block_idx);
            assert_eq!(a.split_idx, b.split_idx);
        }
    }
}
