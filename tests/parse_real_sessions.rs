//! End-to-end parsing over small, real, secret-scanned native sessions committed under
//! `tests/fixtures/` (trimmed from public Hub trace datasets; one per harness as parsers land).
//! Deterministic — no network — so it runs in CI on every commit and guards the parsers against
//! real-format drift the synthetic unit tests can't see: real skip-line types, real tool chains,
//! and `turn_uuid` stability across a re-parse (the property incremental "only new turns" dedup
//! relies on).

use std::collections::BTreeSet;
use std::path::PathBuf;

use funes::trace::Turn;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn block_kinds(turns: &[Turn]) -> BTreeSet<&str> {
    turns
        .iter()
        .flat_map(|t| &t.blocks)
        .map(|b| b.block_type.as_str())
        .collect()
}

fn roles(turns: &[Turn]) -> BTreeSet<&str> {
    turns.iter().map(|t| t.role.as_str()).collect()
}

/// Every `tool_result` whose `call_id` matches a `tool_use` in the same file must have had its name
/// back-filled — the correlation the parsers run.
fn matched_results_are_named(turns: &[Turn]) {
    let call_ids: BTreeSet<&str> = turns
        .iter()
        .flat_map(|t| &t.blocks)
        .filter(|b| b.block_type == "tool_use")
        .filter_map(|b| b.tool_use_id.as_deref())
        .collect();
    let results = turns
        .iter()
        .flat_map(|t| &t.blocks)
        .filter(|b| b.block_type == "tool_result");
    let mut checked = 0;
    for b in results {
        if let Some(id) = b.tool_use_id.as_deref() {
            if call_ids.contains(id) {
                assert!(b.tool_name.is_some(), "tool_result for {id} was not name-correlated");
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "fixture has no correlated tool_result to check");
}

/// Re-parsing the same file yields identical `(turn_uuid, seq)` — id stability across a grown
/// append-only log is what makes chunk-id dedup skip already-indexed turns.
fn ids_are_stable(turns: &[Turn], reparse: &[Turn]) {
    assert_eq!(turns.len(), reparse.len());
    for (a, b) in turns.iter().zip(reparse) {
        assert_eq!(a.turn_uuid, b.turn_uuid);
        assert_eq!(a.seq, b.seq);
    }
}

#[test]
fn parse_real_codex_session() {
    let p = fixture("codex_session.jsonl");
    let turns = funes::codex_traces::turns_from_jsonl_file(&p, "sess", "proj").expect("parse codex");
    assert!(!turns.is_empty());
    // The full block vocabulary is exercised on real records.
    for want in ["text", "thinking", "tool_use", "tool_result"] {
        assert!(
            block_kinds(&turns).contains(want),
            "codex fixture missing {want}: {:?}",
            block_kinds(&turns)
        );
    }
    // Codex tool results carry the `tool` role; the `session_meta` line produced no turn.
    assert!(roles(&turns).is_subset(&BTreeSet::from(["user", "assistant", "tool", "developer"])));
    assert!(roles(&turns).contains("tool"));
    matched_results_are_named(&turns);
    // Codex synthesizes stable ids as `session_id-seq`.
    for (i, t) in turns.iter().enumerate() {
        assert_eq!(t.turn_uuid, format!("sess-{i}"));
    }
    ids_are_stable(
        &turns,
        &funes::codex_traces::turns_from_jsonl_file(&p, "sess", "proj").unwrap(),
    );
}

#[test]
fn parse_real_pi_session() {
    let p = fixture("pi_session.jsonl");
    let turns = funes::pi_traces::turns_from_jsonl_file(&p, "sess", "proj").expect("parse pi");
    assert!(!turns.is_empty());
    for want in ["text", "thinking", "tool_use", "tool_result"] {
        assert!(
            block_kinds(&turns).contains(want),
            "pi fixture missing {want}: {:?}",
            block_kinds(&turns)
        );
    }
    // Control lines (session/model_change/thinking_level_change) produce no turn; a result is `tool`.
    assert!(roles(&turns).is_subset(&BTreeSet::from(["user", "assistant", "tool"])));
    matched_results_are_named(&turns);
    // Pi uses the native line `id` as `turn_uuid`; stable across a re-parse.
    ids_are_stable(
        &turns,
        &funes::pi_traces::turns_from_jsonl_file(&p, "sess", "proj").unwrap(),
    );
}

#[test]
fn parse_real_claude_session() {
    let p = fixture("claude_session.jsonl");
    let turns = funes::claude_traces::turns_from_jsonl_file(&p, "sess", "proj").expect("parse claude");
    assert!(!turns.is_empty());
    // This Fable-derived dataset redacts thinking (empty `thinking` field), so the real vocabulary
    // here is text/tool_use/tool_result; thinking-with-content is covered by the unit test.
    for want in ["text", "tool_use", "tool_result"] {
        assert!(
            block_kinds(&turns).contains(want),
            "claude fixture missing {want}: {:?}",
            block_kinds(&turns)
        );
    }
    // Only user/assistant records become turns — the real `queue-operation` line is skipped.
    assert!(roles(&turns).is_subset(&BTreeSet::from(["user", "assistant"])));
    matched_results_are_named(&turns);
    // Claude uses its native `uuid` as `turn_uuid`; stable across a re-parse.
    ids_are_stable(
        &turns,
        &funes::claude_traces::turns_from_jsonl_file(&p, "sess", "proj").unwrap(),
    );
}
