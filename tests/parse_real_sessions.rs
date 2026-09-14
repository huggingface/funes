//! End-to-end parsing over small, real, secret-scanned native sessions committed under
//! `tests/fixtures/` (public Hub traces or sanitized native exports; one per harness as parsers land).
//! Deterministic — no network — so it runs in CI on every commit and guards the parsers against
//! real-format drift the synthetic unit tests can't see: real skip-line types, real tool chains,
//! and `turn_uuid` stability across a re-parse (the property incremental "only new turns" dedup
//! relies on).

use std::collections::BTreeSet;
use std::path::PathBuf;

use funes::traces::Turn;

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
    let turns = funes::traces::codex::turns_from_jsonl_file(&p, "proj").expect("parse codex");
    assert!(!turns.is_empty());
    // The full block vocabulary is exercised on real records.
    for want in ["text", "thinking", "tool_use", "tool_result"] {
        assert!(
            block_kinds(&turns).contains(want),
            "codex fixture missing {want}: {:?}",
            block_kinds(&turns)
        );
    }
    assert!(
        turns.iter().all(|t| t.harness == "codex"),
        "codex turns are tagged codex"
    );
    // Codex tool results carry the `tool` role; the `session_meta` line produced no turn.
    assert!(roles(&turns).is_subset(&BTreeSet::from(["user", "assistant", "tool", "developer"])));
    assert!(roles(&turns).contains("tool"));
    matched_results_are_named(&turns);
    // Codex synthesizes `<session_id>-<seq>`; the session id (from the session_meta line) is
    // constant across the file, so every turn_uuid shares one prefix and ends in its seq.
    let (prefix, _) = turns[0].turn_uuid.rsplit_once('-').expect("turn_uuid is <id>-<seq>");
    for (i, t) in turns.iter().enumerate() {
        assert_eq!(t.turn_uuid, format!("{prefix}-{i}"));
    }
    ids_are_stable(
        &turns,
        &funes::traces::codex::turns_from_jsonl_file(&p, "proj").unwrap(),
    );
}

#[test]
fn parse_real_pi_session() {
    let p = fixture("pi_session.jsonl");
    let turns = funes::traces::pi::turns_from_jsonl_file(&p, "sess", "proj").expect("parse pi");
    assert!(!turns.is_empty());
    for want in ["text", "thinking", "tool_use", "tool_result"] {
        assert!(
            block_kinds(&turns).contains(want),
            "pi fixture missing {want}: {:?}",
            block_kinds(&turns)
        );
    }
    assert!(turns.iter().all(|t| t.harness == "pi"), "pi turns are tagged pi");
    // Control lines (session/model_change/thinking_level_change) produce no turn; a result is `tool`.
    assert!(roles(&turns).is_subset(&BTreeSet::from(["user", "assistant", "tool"])));
    matched_results_are_named(&turns);
    // Pi uses the native line `id` as `turn_uuid`; stable across a re-parse.
    ids_are_stable(
        &turns,
        &funes::traces::pi::turns_from_jsonl_file(&p, "sess", "proj").unwrap(),
    );
}

#[test]
fn parse_real_claude_session() {
    let p = fixture("claude_session.jsonl");
    let turns = funes::traces::claude::turns_from_jsonl_file(&p, "sess", "proj").expect("parse claude");
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
    assert!(
        turns.iter().all(|t| t.harness == "claude_code"),
        "claude turns are tagged claude_code"
    );
    // Only user/assistant records become turns — the real `queue-operation` line is skipped.
    assert!(roles(&turns).is_subset(&BTreeSet::from(["user", "assistant"])));
    matched_results_are_named(&turns);
    // Claude uses its native `uuid` as `turn_uuid`; stable across a re-parse.
    ids_are_stable(
        &turns,
        &funes::traces::claude::turns_from_jsonl_file(&p, "sess", "proj").unwrap(),
    );
}

#[path = "support/cursor_fixture.rs"]
mod cursor_fixture;

#[test]
fn parse_sanitized_native_cursor_session() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.vscdb");
    cursor_fixture::write(&db, cursor_fixture::HEADER_COUNT);
    let before = std::fs::read(&db).unwrap();

    let units = funes::traces::cursor::sessions_with_watermark(&db).unwrap();
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].session_id, cursor_fixture::SESSION_ID);
    assert_eq!(units[0].watermark, 1_767_225_607_000);
    let turns = funes::traces::cursor::turns_from_state_db(&db, cursor_fixture::SESSION_ID).unwrap();
    // The empty assistant bubble is not indexable, but its position remains part of sequence IDs.
    assert_eq!(turns.iter().map(|t| t.seq).collect::<Vec<_>>(), vec![0, 1, 3, 4, 5, 6]);
    assert_eq!(roles(&turns), BTreeSet::from(["user", "assistant"]));
    assert_eq!(block_kinds(&turns), BTreeSet::from(["text", "thinking"]));
    for t in &turns {
        assert_eq!(t.session_id, cursor_fixture::SESSION_ID);
        assert_eq!(t.harness, "cursor");
        assert_eq!(t.turn_uuid, format!("bubble-{}", t.seq));
        assert_eq!(t.ts, format!("2026-01-01T00:00:0{}.000Z", t.seq));
        assert_eq!(t.source_path, db.to_string_lossy());
        if t.role == "assistant" {
            assert_eq!(t.blocks.len(), 2);
            assert_eq!(t.blocks[0].block_type, "thinking");
            assert_eq!(
                t.blocks[0].text,
                format!("Fixture reasoning {}: verify parsing before indexing.", t.seq)
            );
            assert_eq!(t.blocks[1].block_type, "text");
            assert_eq!(
                t.blocks[1].text,
                format!(
                    "Fixture turn {}: The session parser uses ordered headers and reads each referenced message.",
                    t.seq
                )
            );
        } else {
            assert_eq!(t.blocks.len(), 1);
            assert_eq!(t.blocks[0].block_type, "text");
            assert_eq!(
                t.blocks[0].text,
                format!(
                    "Fixture turn {}: check the session parser and preserve stable message identifiers.",
                    t.seq
                )
            );
        }
    }
    ids_are_stable(
        &turns,
        &funes::traces::cursor::turns_from_state_db(&db, cursor_fixture::SESSION_ID).unwrap(),
    );
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "discovery and parsing must not modify the source"
    );
}
