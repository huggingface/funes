//! `funes add hermes` installs the plugin whose hooks drive the automation, leaves the memory in a
//! file beside its scripts, and lets hermes enable it and register the MCP server. Own test binary:
//! it sets `$HOME` and `$PATH` (both process-global) so the script runs against a fake `hermes`.

mod support;

use funes::agents::registry;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn add_hermes_installs_the_plugin_and_leaves_a_pre_plugin_config_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "hermes");

    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    std::env::set_var("FUNES_TEST_CLI_LOG", &log);
    // The variables that name real state elsewhere: hermes's own home, which the script installs
    // into, the memory this install would seed, and the funes binary it records.
    std::env::remove_var("HERMES_HOME");
    std::env::remove_var("FUNES_HOME");
    std::env::remove_var("FUNES_BIN");

    // An install from before the plugin: its hook entries in the file that also holds the user's
    // configuration, and its scripts beside a script of the user's.
    let hermes_dir = home.join(".hermes");
    let hooks = hermes_dir.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("funes-index.sh"), "owned").unwrap();
    fs::write(hooks.join("user-hook.sh"), "keep").unwrap();
    let config = hermes_dir.join("config.yaml");
    fs::write(
        &config,
        "model: hermes-4\n\
         hooks:\n  \
           post_llm_call:\n  \
           - command: bash \"/old/funes-index.sh\" \"hermes\"\n",
    )
    .unwrap();

    let root = registry::default_root().unwrap();
    registry::provision(&root, "hermes", false).await.unwrap();
    registry::open(&root, "hermes").unwrap().add(Some("acme/kb")).unwrap();

    // hermes discovers user plugins under its own home only, so funes's lives there.
    let plugin = hermes_dir.join("plugins/funes");
    let manifest = fs::read_to_string(plugin.join("plugin.yaml")).unwrap();
    for event in ["post_llm_call", "on_session_start", "on_session_finalize"] {
        assert!(manifest.contains(event), "{manifest}");
    }
    let register = fs::read_to_string(plugin.join("__init__.py")).unwrap();
    assert!(register.contains("def register(ctx)"), "{register}");

    // The scripts the hooks drive are symlinks in the checkout and must arrive as executable files.
    for name in ["funes-index.sh", "funes-push.sh"] {
        let path = plugin.join(name);
        assert!(
            !path.symlink_metadata().unwrap().file_type().is_symlink(),
            "{name} is a file"
        );
        assert!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o111 != 0,
            "{name} executable"
        );
    }
    // The memory rides in a file the push hook reads, so the plugin stays a static file.
    assert_eq!(fs::read_to_string(plugin.join("memory")).unwrap(), "acme/kb\n");

    // The pre-plugin hooks are in the user's own file, so funes names them rather than editing it —
    // but their approvals are revoked and their scripts gone, so they do nothing beside the plugin.
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "model: hermes-4\nhooks:\n  post_llm_call:\n  - command: bash \"/old/funes-index.sh\" \"hermes\"\n"
    );
    assert!(!hooks.join("funes-index.sh").exists(), "funes's own script goes");
    assert!(hooks.join("user-hook.sh").exists(), "the user's stays");

    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "config path\n\
         hooks revoke bash \"/old/funes-index.sh\" \"hermes\"\n\
         plugins enable funes\n\
         mcp add funes --command funes --args mcp acme/kb\n"
    );
}
