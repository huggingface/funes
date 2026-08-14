//! End-to-end: build a real index from a tiny transcript in a temp dir, then exercise
//! the read surface (recall / get / sessions / scan / status). No mocking — this runs the real
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
        &format!(
            r#"{{"type":"assistant","uuid":"t4","parentUuid":"t3","timestamp":"2026-01-01T00:00:15Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{}"}}]}}}}"#,
            seam_block()
        ),
    ];
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
    (session.to_string(), workdir.to_string())
}

/// A block long enough to split (over the 1200-char chunk cap), with `SPLITSEAMMARKER` placed in
/// the 150-char region consecutive chunks overlap — so the marker is stored in two chunks. A scan
/// that matched raw chunks would report it twice.
fn seam_block() -> String {
    let filler = |from: usize, to: usize| (from..to).map(|i| format!("w{i:03} ")).collect::<String>();
    format!("{}SPLITSEAMMARKER {}", filler(0, 220), filler(220, 320))
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

    // get: reassemble the assistant turn by its uuid.
    let got = funes::commands::recall::get(
        funes::memory::Memory::local(),
        session.clone(),
        funes::commands::recall::TurnRange::default(),
    )
    .await
    .unwrap();
    assert!(got.contains("typed blocks"), "get should return the turn text: {got}");

    // sessions: the memory enumerates to the one indexed session, counted in turns not rows.
    let listed = funes::commands::recall::sessions(funes::memory::Memory::local())
        .await
        .unwrap();
    assert!(
        listed.contains(&format!("{workdir}/{session} 4 turns")),
        "sessions should list the session with its distinct turn count: {listed}"
    );
    assert!(
        listed.trim_end().ends_with("1 sessions"),
        "the listing should close with the total: {listed}"
    );

    // scan: an exhaustive literal search of one session, and a zero that names the needle it
    // cleared.
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
        "a scan hit should carry a runnable range around its turn, clamped at the start: {scanned}"
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
        "a zero should echo the needle and the session it cleared: {zero}"
    );

    // A session that isn't in the memory is an error, not a clearance — a mistyped id must never
    // read as "the term is absent".
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
    let err = unknown.expect_err("an unknown session must be an error").to_string();
    assert!(err.contains("no session no-such-session"), "names the session: {err}");

    // A window scopes what a zero clears: the same needle is present in the session and absent from
    // a stretch that excludes it, and the reply says which stretch it cleared.
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
        windowed.starts_with(&format!("no matches for \"typed blocks\" in {session} turns 2 on")),
        "a window must scope the clearance it reports: {windowed}"
    );
    let covering = funes::commands::recall::scan(
        funes::memory::Memory::local(),
        "typed blocks".into(),
        session.clone(),
        Some(0),
        Some(1),
        false,
        40,
    )
    .await
    .unwrap();
    assert!(
        covering.contains("turns 0-1 — 1 hits"),
        "a window that covers the hit still finds it: {covering}"
    );

    // A range outside the session is not a clearance — it reports the session's size instead.
    let outside = funes::commands::recall::scan(
        funes::memory::Memory::local(),
        "typed blocks".into(),
        session.clone(),
        Some(900),
        Some(910),
        false,
        40,
    )
    .await
    .unwrap();
    assert!(
        outside.contains("no turns in that range"),
        "an empty window must not read as absence: {outside}"
    );

    // A needle inside the region two chunks overlap is one hit, not one per chunk — splits are
    // stitched back into their block before anything is matched.
    let seam = funes::commands::recall::scan(
        funes::memory::Memory::local(),
        "SPLITSEAMMARKER".into(),
        session.clone(),
        None,
        None,
        false,
        40,
    )
    .await
    .unwrap();
    assert!(
        seam.contains("— 1 hits"),
        "a split block should report one hit, not one per chunk: {seam}"
    );

    // --ignore-case covers case variation; the same needle misses without it.
    let folded = funes::commands::recall::scan(
        funes::memory::Memory::local(),
        "TYPED BLOCKS".into(),
        session.clone(),
        None,
        None,
        true,
        40,
    )
    .await
    .unwrap();
    assert!(folded.contains("— 1 hits"), "ignore_case should fold: {folded}");
    let exact = funes::commands::recall::scan(
        funes::memory::Memory::local(),
        "TYPED BLOCKS".into(),
        session.clone(),
        None,
        None,
        false,
        40,
    )
    .await
    .unwrap();
    assert!(
        exact.starts_with("no matches for"),
        "a literal scan is case-sensitive by default: {exact}"
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
