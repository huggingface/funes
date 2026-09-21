//! `funes add pi` installs the pi integration from the checkout into the registry and lets its
//! `setup` register the extension with pi. Own test binary: it sets `$HOME` and `$PATH` (both
//! process-global) so the script runs against a fake `pi` and writes nowhere real.

mod support;

use funes::agents::registry;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn add_pi_installs_the_integration_and_registers_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let log = tmp.path().join("cli.log");
    let bin = support::fake_cli(tmp.path(), "pi");

    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    std::env::set_var("FUNES_TEST_CLI_LOG", &log);
    // The memory this install would otherwise seed.
    std::env::remove_var("FUNES_HOME");

    let root = registry::default_root().unwrap();
    registry::provision(&root, "pi", false).unwrap();
    registry::open(&root, "pi").unwrap().add(Some("acme/kb")).unwrap();

    let dir = home.join(".funes/agents/pi");
    assert!(dir.join("index.ts").exists(), "the extension itself");
    assert_eq!(
        fs::read_to_string(dir.join("memory")).unwrap(),
        "acme/kb\n",
        "the memory index.ts reads at startup"
    );

    // The script and the automation it drives, executable — the shared scripts are symlinks in the
    // checkout and must arrive as files.
    for name in ["setup", "scripts/funes-index.sh", "scripts/funes-push.sh"] {
        let path = dir.join(name);
        assert!(
            !path.symlink_metadata().unwrap().file_type().is_symlink(),
            "{name} is a file"
        );
        assert!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o111 != 0,
            "{name} executable"
        );
    }

    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("--version\ninstall {}\n", dir.display()),
        "the version gate, then the registration"
    );

    // Re-running binds the local memory: the file index.ts reads is gone, not left stale.
    registry::open(&root, "pi").unwrap().add(None).unwrap();
    assert!(!dir.join("memory").exists());
}
