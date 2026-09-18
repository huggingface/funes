//! Cursor append-only indexing must be idempotent and converge to a fresh index of the same
//! conversation. Separate test binary isolates its process-wide FUNES_HOME from other tests.

use std::collections::BTreeMap;

use arrow_array::{Int64Array, StringArray};
use funes::{chunk, memory::Memory, traces::cursor};

#[path = "support/cursor_fixture.rs"]
mod cursor_fixture;

type Rows = BTreeMap<String, (String, String, i64)>;

async fn stored_rows() -> Rows {
    let ds = Memory::local().open().await.unwrap();
    let batches = funes::memory::dataset::scan_rows(&ds, &["id", "text", "block_type", "seq"], None, None)
        .await
        .unwrap();
    let mut rows = Rows::new();
    for batch in batches {
        let strings = |name| {
            batch
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
        };
        let ids = strings("id");
        let texts = strings("text");
        let kinds = strings("block_type");
        let seqs = batch
            .column_by_name("seq")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            assert!(
                rows.insert(
                    ids.value(i).into(),
                    (texts.value(i).into(), kinds.value(i).into(), seqs.value(i))
                )
                .is_none(),
                "duplicate stored chunk ID"
            );
        }
    }
    rows
}

#[tokio::test]
async fn cursor_growth_matches_fresh_index_without_duplicates_or_loss() {
    let source = tempfile::tempdir().unwrap();
    let db = source.path().join("state.vscdb");
    let incremental_home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", incremental_home.path());

    cursor_fixture::write(&db, 4);
    let initial_turns = cursor::turns_from_state_db(&db, cursor_fixture::SESSION_ID).unwrap();
    funes::commands::index::run_index(&db, false, None).await.unwrap();
    let initial = stored_rows().await;
    assert_eq!(initial.len(), 5, "one user and two assistant text/thinking pairs");
    let state_path = incremental_home.path().join("state.json");
    let state = std::fs::read(&state_path).unwrap();
    funes::commands::index::run_index(&db, false, None).await.unwrap();
    assert_eq!(stored_rows().await, initial, "unchanged re-index is idempotent");
    assert_eq!(std::fs::read(&state_path).unwrap(), state);

    cursor_fixture::write(&db, cursor_fixture::HEADER_COUNT);
    let final_turns = cursor::turns_from_state_db(&db, cursor_fixture::SESSION_ID).unwrap();
    for (before, after) in initial_turns.iter().zip(&final_turns) {
        assert_eq!((&before.turn_uuid, before.seq), (&after.turn_uuid, after.seq));
    }
    let source_before = std::fs::read(&db).unwrap();
    funes::commands::index::run_index(&db, false, None).await.unwrap();
    let grown = stored_rows().await;
    assert_eq!(grown.len(), 9, "all six text and three thinking blocks are stored");
    for (id, row) in &initial {
        assert_eq!(grown.get(id), Some(row), "previously stored content is preserved");
    }
    assert_ne!(
        std::fs::read(&state_path).unwrap(),
        state,
        "changed watermark must be recorded"
    );
    funes::commands::index::run_index(&db, false, None).await.unwrap();
    assert_eq!(stored_rows().await, grown);

    let fresh_home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", fresh_home.path());
    funes::commands::index::run_index(&db, false, None).await.unwrap();
    assert_eq!(
        stored_rows().await,
        grown,
        "incremental and fresh indexes have identical IDs and content"
    );
    let expected: Rows = chunk::chunks_from_turns(&final_turns, &chunk::Tier::ALL, true)
        .into_iter()
        .map(|c| (c.id, (c.text, c.block_type, c.seq)))
        .collect();
    assert_eq!(grown, expected, "every parsed chunk is present, including thinking");
    assert_eq!(
        std::fs::read(&db).unwrap(),
        source_before,
        "indexing must not modify Cursor's source database"
    );
}
