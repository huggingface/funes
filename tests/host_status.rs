//! `funes status` with no memory named: the local memory, then each bound memory with the agents
//! bound to it, then the agents installed with no memory recorded.

use funes::commands::recall::host_status;

#[tokio::test]
async fn host_status_lists_each_bound_memory_and_the_unrecorded() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let missing = home.path().join("no-such-memory").to_string_lossy().into_owned();

    let out = host_status(
        &[(missing, vec!["claude".to_string(), "codex".to_string()])],
        &["pi".to_string()],
    )
    .await
    .unwrap();

    assert!(out.starts_with("memory: "), "the local memory comes first: {out}");
    assert!(out.contains("no index yet"), "{out}");
    assert!(
        out.contains("(bound: claude, codex)"),
        "a bound memory names the agents bound to it: {out}"
    );
    assert!(
        out.contains("installed with no memory recorded: pi"),
        "an install funes has no record of is named: {out}"
    );
}
