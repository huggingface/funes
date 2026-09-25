//! `funes remove` of an id nothing knows: a no-op that says what it found, settled without the
//! network when `$FUNES_INTEGRATIONS` answers, and against the release bucket when it does not.
//! What `remove` does for each shipped agent is that bundle's own test, under `integrations/`.

mod support;

/// Nothing installed and nowhere to fetch from is what `remove` leaves behind, so meeting it is a
/// no-op that says what it found — and settles the id locally, off the network.
#[test]
fn an_agent_nothing_knows_is_named_rather_than_fetched() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    // `$FUNES_INTEGRATIONS` is authoritative, so the unknown id is settled locally, off the network.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_funes"))
        .args(["remove", "clyde"])
        .env("HOME", &home)
        .env(
            "FUNES_INTEGRATIONS",
            concat!(env!("CARGO_MANIFEST_DIR"), "/integrations"),
        )
        .env_remove("FUNES_HOME")
        .output()
        .unwrap();
    support::assert_success(&out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("nothing to remove"), "{err}");
    assert!(err.contains("no clyde integration on this machine"), "{err}");
    assert!(err.contains("docs/add.md"), "{err}");
}

/// The same id with `$FUNES_INTEGRATIONS` unset is asked of the release bucket, as a released binary
/// asks: an integration it never published is nothing to remove too, not a failed download.
#[test]
fn an_agent_the_release_bucket_never_published_is_nothing_to_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_funes"))
        .args(["remove", "clyde"])
        .env("HOME", &home)
        .env_remove("FUNES_INTEGRATIONS")
        .env_remove("FUNES_HOME")
        .output()
        .unwrap();
    support::assert_success(&out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nothing to remove"), "{err}");
    assert!(err.contains("publishes no clyde integration for contract"), "{err}");
}
