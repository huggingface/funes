//! `funes ask` with no agent CLI on PATH: a hard error naming the missing binary and suggesting
//! the other agent — unlike `add`, ask has no partial success to fall back on. Its own binary
//! because it clears the process-global `$PATH`, and cargo runs a file's tests concurrently —
//! any other test resolving binaries would race it.

#[tokio::test]
async fn ask_errors_when_the_agent_cli_is_missing() {
    std::env::set_var("PATH", "");

    let err = funes::ask::claude("q".into(), None).await.unwrap_err().to_string();
    assert!(err.contains("`claude` isn't on PATH"), "{err}");
    assert!(err.contains("funes ask codex"), "suggests the other agent: {err}");

    let err = funes::ask::preflight("codex").unwrap_err().to_string();
    assert!(err.contains("`codex` isn't on PATH"), "{err}");
    assert!(err.contains("funes ask claude"), "suggests the other agent: {err}");
}
