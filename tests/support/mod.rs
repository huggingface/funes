use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub fn fake_cli(root: &Path, name: &str) -> PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let path = bin.join(name);
    fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FUNES_TEST_CLI_LOG\"\n").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    bin
}

pub fn run_remove(home: &Path, bin: &Path, log: &Path, agent: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_funes"))
        .args(["remove", agent])
        .env("HOME", home)
        .env("PATH", bin)
        .env("FUNES_TEST_CLI_LOG", log)
        .output()
        .unwrap()
}

pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
