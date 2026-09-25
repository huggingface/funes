//! `funes remove` of an id nothing knows: a no-op that says what it found, settled without a
//! source — `remove` runs the installed copy and fetches nothing. What `remove` does for each
//! maintained agent is that bundle's own test, in huggingface/funes-integrations.

mod support;

/// Nothing installed is what `remove` leaves behind, so meeting it is a no-op that says so — with
/// `$FUNES_INTEGRATIONS` set or not, since no source is consulted either way.
#[test]
fn an_agent_nothing_knows_is_nothing_to_remove() {
    for redirected in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_funes"));
        cmd.args(["remove", "clyde"])
            .env("HOME", &home)
            .env_remove("FUNES_HOME");
        if redirected {
            cmd.env("FUNES_INTEGRATIONS", tmp.path().join("integrations"));
        } else {
            cmd.env_remove("FUNES_INTEGRATIONS");
        }
        let out = cmd.output().unwrap();
        support::assert_success(&out);
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "nothing to remove — no clyde integration is installed.\n",
            "redirected: {redirected}"
        );
    }
}
