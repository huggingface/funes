# Cursor fixture provenance

`cursor_session.json` is an allowlisted projection of seven ordered message headers and their
referenced bubble records from a native macOS Cursor global `state.vscdb`, inspected on
2026-09-14. The export preserves numeric `_v`/`type` fields, header references, ISO timestamp
shape, object-form `thinking.text`, and an empty assistant bubble. Session/bubble identifiers,
timestamps, message text, and reasoning text were replaced with fixed synthetic values. All
other fields were removed; no original conversation content, file paths, or credentials remain.

This is a sanitized native-format fixture, not an unmodified transcript or a complete database
export. It covers the currently supported text and object-form thinking schema. String-form
thinking and tool payloads are not covered by this fixture.

`tests/support/cursor_fixture.rs` restores the JSON rows into a temporary SQLite database, mixing
TEXT and BLOB values and inserting them in reverse order. The source database is never needed
by the tests. A prefix of the headers can be restored first and then extended to exercise
append-only incremental indexing.
