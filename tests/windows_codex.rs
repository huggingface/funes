//! Native process and configuration regression tests; one test owns its environment variables.
#![cfg(windows)]

use funes::agents::codex;
use serde_json::json;
use std::{fs, process::Command};

#[test]
fn native_codex_registration_and_independent_cleanup() {
    let root = tempfile::Builder::new().prefix("funes 用户 & (x) ").tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fixture = bin.join("fixture.rs");
    fs::write(&fixture, r#"
use std::{env, fs::OpenOptions, io::Write};
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut log = OpenOptions::new().create(true).append(true).open(env::var_os("FUNES_TEST_CLI_LOG").unwrap()).unwrap();
    writeln!(log, "START\n{}\nEND", args.join("\n")).unwrap();
    if args.first().map(String::as_str) == Some("doctor") {
        println!("{}", env::var("FUNES_TEST_REPORT").unwrap());
    } else if args.first().map(String::as_str) == Some("--version") {
        println!("codex-cli 0.151.0");
    } else if args.get(1).map(String::as_str) == Some("remove") && env::var_os("FUNES_TEST_REMOVE_FAIL").is_some() {
        eprintln!("fixture unregister failure");
        std::process::exit(1);
    }
}
"#).unwrap();
    assert!(Command::new("rustc")
        .arg(&fixture)
        .args(["--edition", "2021", "-o"])
        .arg(bin.join("codex.exe"))
        .status()
        .unwrap()
        .success());
    let home = root.path().join("profile");
    let codex_home = root.path().join("custom Codex");
    fs::create_dir_all(&codex_home).unwrap();
    let log = root.path().join("calls.txt");
    std::env::set_var("USERPROFILE", &home);
    std::env::remove_var("HOME");
    std::env::remove_var("FUNES_HOME");
    assert_eq!(funes::platform::user_home(), Some(home.clone()));
    assert_eq!(funes::memory::dataset::funes_dir(), home.join(".funes"));
    let state = root.path().join("custom memory");
    std::env::set_var("FUNES_HOME", &state);
    assert_eq!(funes::memory::dataset::funes_dir(), state);

    // Exercise cache fallback with invented token files, never the user's credentials.
    for key in [
        "HF_HOME",
        "HF_TOKEN_PATH",
        "HF_TOKEN",
        "HUGGING_FACE_HUB_TOKEN",
        "HUGGINGFACE_TOKEN",
    ] {
        std::env::remove_var(key);
    }
    let cache = home.join(".cache/huggingface");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("token"), "profile-fixture").unwrap();
    assert_eq!(funes::hub::hf_token().as_deref(), Some("profile-fixture"));
    let cache_override = root.path().join("custom HF cache");
    fs::create_dir_all(&cache_override).unwrap();
    fs::write(cache_override.join("token"), "override-fixture").unwrap();
    std::env::set_var("HF_HOME", &cache_override);
    assert_eq!(funes::hub::hf_token().as_deref(), Some("override-fixture"));

    std::env::set_var("CODEX_HOME", root.path().join("wrong fallback"));
    std::env::set_var("PATH", &bin);
    std::env::set_var("FUNES_TEST_CLI_LOG", &log);
    std::env::set_var(
        "FUNES_TEST_REPORT",
        json!({"checks":{"config.load":{"details":{"CODEX_HOME":codex_home}}}}).to_string(),
    );
    let funes = root.path().join("funes path & (x) %.exe");
    std::env::set_var("FUNES_BIN", &funes);
    let hooks = codex_home.join("hooks.json");
    fs::write(
        &hooks,
        r#"{"theme":"dark","hooks":{"PreToolUse":[{"hooks":[{"command":"user-hook"}]}]}}"#,
    )
    .unwrap();
    codex::install(Some("acme/kb".into())).unwrap();
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains(&format!("mcp\nadd\nfunes\n--\n{}\nmcp\nacme/kb", funes.display())),
        "{calls}"
    );
    assert!(codex_home.join("hooks/funes-index.ps1").is_file());
    let skill = codex_home.join("skills/funes/SKILL.md");
    assert!(skill.is_file());
    codex::install(None).unwrap();
    let config: serde_json::Value = serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(config["theme"], "dark");
    assert_eq!(config["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert!(config["hooks"].get("SessionEnd").is_none());
    codex::uninstall().unwrap();
    assert!(!skill.exists());
    let config: serde_json::Value = serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(config["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "user-hook");

    codex::install(None).unwrap();
    fs::write(&hooks, "malformed configuration").unwrap();
    assert!(codex::uninstall().is_err());
    assert_eq!(fs::read_to_string(&hooks).unwrap(), "malformed configuration");
    assert!(!skill.exists(), "malformed hooks must not strand the skill");
    fs::write(&hooks, "{}").unwrap();
    codex::install(None).unwrap();
    std::env::set_var("FUNES_TEST_REMOVE_FAIL", "1");
    assert!(codex::uninstall().is_err());
    assert!(!skill.exists(), "failed unregister must not strand the skill");
}
