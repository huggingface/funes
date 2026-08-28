//! End-to-end: build a real index from a tiny transcript in a temp dir, then exercise
//! the read surface (recall / get / status). No mocking — this runs the real
//! BGE embedder + reranker (downloaded to the fastembed cache on first run) against a
//! real Lance memory under a temp `$FUNES_HOME`.

use std::io::Write;

/// Write a `<source>/projects/<project>/<session>.jsonl` transcript so `workdir_of` /
/// `session_id_of` resolve the way they do for real Claude Code projects.
fn write_transcript(source: &std::path::Path) -> (String, String) {
    let workdir = "-home-u-dev-demo";
    let session = "test-session-0001";
    let dir = source.join("projects").join(workdir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
    let lines = [
        r#"{"type":"user","uuid":"t1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"how do we parse transcripts into turns"}}"#,
        r#"{"type":"assistant","uuid":"t2","parentUuid":"t1","timestamp":"2026-01-01T00:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"We parse each JSONL line into a turn with typed blocks."},{"type":"tool_use","id":"c1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
        r#"{"type":"user","uuid":"t3","parentUuid":"t2","timestamp":"2026-01-01T00:00:10Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":[{"type":"text","text":"22 passed"}]}]}}"#,
    ];
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
    (session.to_string(), workdir.to_string())
}

#[tokio::test]
async fn index_then_read_surface() {
    let db_dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    // db::funes_dir() reads $FUNES_HOME; point the whole read/write surface at the temp dir.
    std::env::set_var("FUNES_HOME", db_dir.path());
    let (session, workdir) = write_transcript(source.path());

    // Build the index for real: parse → chunk → embed → Lance + FTS.
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();

    // status: non-empty chunk count.
    let status = funes::commands::recall::status(funes::memory::Memory::local())
        .await
        .unwrap();
    assert!(status.contains("chunks:"), "status missing chunk count: {status}");

    // recall: the parsing turn surfaces, and the `→ get` line carries the full session id.
    let out = funes::commands::recall::recall(
        funes::memory::Memory::local(),
        "parse transcripts into turns".into(),
        5,
        30,
        30.0,
        1,
        None,
        None,
    )
    .await
    .unwrap();
    assert_ne!(out, "no results", "recall returned nothing");
    assert!(
        out.contains(&session),
        "recall should surface the indexed session: {out}"
    );
    assert!(
        out.contains(&workdir),
        "recall provenance should name the workdir: {out}"
    );

    // type filter: restrict to tool_use → the Bash call.
    let tu = funes::commands::recall::recall(
        funes::memory::Memory::local(),
        "cargo test".into(),
        5,
        30,
        0.0,
        0,
        Some("tool_use".into()),
        None,
    )
    .await
    .unwrap();
    assert!(tu.contains("tool_use"), "type filter should keep tool_use rows: {tu}");

    // scan: an exhaustive literal search of one session, and a zero that names what it cleared.
    let scanned = funes::commands::recall::scan(
        funes::memory::Memory::local(),
        "typed blocks".into(),
        session.clone(),
        None,
        None,
        false,
        40,
    )
    .await
    .unwrap();
    assert!(
        scanned.contains(&format!("scan \"typed blocks\" in {session} — 1 hits")),
        "scan should find the literal exactly once: {scanned}"
    );
    assert!(
        scanned.contains(&format!("→ get {session} --from 0 --to 4")),
        "a hit carries a runnable range around its turn, clamped at the start: {scanned}"
    );
    let zero = funes::commands::recall::scan(
        funes::memory::Memory::local(),
        "nothing anywhere says this".into(),
        session.clone(),
        None,
        None,
        false,
        40,
    )
    .await
    .unwrap();
    assert!(
        zero.contains(&format!("no matches for \"nothing anywhere says this\" in {session}")),
        "a zero echoes the needle and the session it cleared: {zero}"
    );

    // An unknown session is an error, not a clearance: a mistyped id must never read as absence.
    let unknown = funes::commands::recall::scan(
        funes::memory::Memory::local(),
        "typed blocks".into(),
        "no-such-session".into(),
        None,
        None,
        false,
        40,
    )
    .await;
    assert!(
        unknown.is_err_and(|e| e.to_string().contains("no session no-such-session")),
        "an unknown session must be an error"
    );

    // A window scopes what a zero clears, and the reply says which stretch it scanned.
    let windowed = funes::commands::recall::scan(
        funes::memory::Memory::local(),
        "typed blocks".into(),
        session.clone(),
        Some(2),
        None,
        false,
        40,
    )
    .await
    .unwrap();
    assert!(
        windowed.contains("turns 2 on"),
        "a windowed scan names the stretch it covered: {windowed}"
    );

    // sessions: the memory enumerates to the one indexed session, counted in turns not rows.
    let listed = funes::commands::recall::sessions(funes::memory::Memory::local(), Default::default())
        .await
        .unwrap();
    assert!(
        listed.contains(&session) && listed.contains(" turns "),
        "sessions should list the session with its turn count: {listed}"
    );
    assert!(
        listed.trim_end().ends_with("1 sessions"),
        "a complete listing closes with the total: {listed}"
    );

    // A limit of zero would render nothing, so it is an error that names the bounds.
    let zero = funes::commands::recall::sessions(
        funes::memory::Memory::local(),
        funes::commands::recall::SessionFilter {
            limit: Some(0),
            ..Default::default()
        },
    )
    .await;
    assert!(
        zero.is_err_and(|e| e.to_string().contains("would list nothing")),
        "a limit of 0 should error"
    );

    // A date filter narrows the population; one that excludes everything says so.
    let none = funes::commands::recall::sessions(
        funes::memory::Memory::local(),
        funes::commands::recall::SessionFilter {
            since: Some("2099-01-01".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(none.contains("matches"), "an empty filter result says so: {none}");

    // get: a session id alone reads from the start, and says what range it read.
    let got = funes::commands::recall::get(
        funes::memory::Memory::local(),
        session.clone(),
        funes::commands::recall::TurnRange::default(),
    )
    .await
    .unwrap();
    assert!(got.contains("typed blocks"), "get should return the turn text: {got}");
    assert!(got.contains("turns "), "get should close with the range it read: {got}");

    // sketch: what the session contains, asked without a query — bounded, addressable, and
    // explicit about what it left out.
    let sketched = funes::commands::sketch::run(
        funes::memory::Memory::local(),
        session.clone(),
        None,
        None,
        Some(3),
        Some(2_000),
    )
    .await
    .unwrap();
    assert!(
        sketched.starts_with(&format!("sketch {session} — ")),
        "a sketch names the session it digested: {sketched}"
    );
    assert!(
        sketched.contains(&format!("→ get {session} --from 0 --to 3")),
        "every place must be addressable, in the same coordinate as get: {sketched}"
    );
    assert!(
        sketched.trim_end().ends_with("scan a literal for that"),
        "a sketch must not read as a clearance: {sketched}"
    );

    // A clamped request is reported: a silent clamp reads as the whole digest.
    let clamped = funes::commands::sketch::run(
        funes::memory::Memory::local(),
        session.clone(),
        None,
        None,
        Some(40),
        Some(1_000),
    )
    .await
    .unwrap();
    assert!(
        clamped.contains("(units clamped to 4)"),
        "a clamp must be stated, not applied quietly: {clamped}"
    );

    // An unknown session is an error, not an empty digest.
    let missing = funes::commands::sketch::run(
        funes::memory::Memory::local(),
        "no-such-session".into(),
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        missing.is_err_and(|e| e.to_string().contains("no session no-such-session")),
        "a mistyped session must not read as an empty session"
    );

    // An unknown session id is absent, not an empty range.
    let missing = funes::commands::recall::get(
        funes::memory::Memory::local(),
        "no-such-session".into(),
        funes::commands::recall::TurnRange::default(),
    )
    .await;
    assert!(
        missing.is_err_and(|e| e.to_string().contains("no session no-such-session")),
        "an unknown session id should error"
    );

    // Every hit names the memory it was read from — the default memory and an explicit one alike.
    let default_hint = format!("--memory {}", db_dir.path().join("memory").display());
    assert!(
        out.contains(&default_hint),
        "hits should carry the read memory `{default_hint}`: {out}"
    );
    let memory2 = db_dir.path().join("memory2");
    copy_dir(&db_dir.path().join("memory"), &memory2);
    let out2 = funes::commands::recall::recall(
        funes::memory::Memory::parse(&memory2.to_string_lossy()),
        "parse transcripts into turns".into(),
        5,
        30,
        30.0,
        1,
        None,
        None,
    )
    .await
    .unwrap();
    let hint = format!("--memory {}", memory2.display());
    assert!(
        out2.contains(&hint),
        "explicit-memory hits should carry `{hint}`: {out2}"
    );
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), &to).unwrap();
        }
    }
}
