//! An install this funes disagrees with is said on every read, CLI and MCP: a hook that asked for a
//! spool nothing writes (an install from before the spool), and a registered integration speaking
//! another contract (an install by another binary). Own test binary: it drives the CLI with its own
//! `$HOME` and `$FUNES_HOME`, handed to the child only.

mod support;

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn funes(home: &Path, funes_home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_funes"))
        .args(args)
        .env("HOME", home)
        .env("FUNES_HOME", funes_home)
        .env("HF_HOME", support::hf_home())
        .env_remove("CODEX_HOME")
        // Piped, so the child sees no terminal: what a hook's run looks like.
        .stdin(Stdio::piped())
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Speak MCP to `funes mcp` over stdio: initialize, then call `status`. Returns the server's
/// instructions and the tool's text.
fn mcp_status(home: &Path, funes_home: &Path) -> (String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_funes"))
        .arg("mcp")
        .env("HOME", home)
        .env("FUNES_HOME", funes_home)
        .env("HF_HOME", support::hf_home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut send = |message: Value| {
        writeln!(stdin, "{message}").unwrap();
    };
    let mut reply = |id: u64| -> Value {
        for line in lines.by_ref() {
            let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if message["id"] == json!(id) {
                return message["result"].clone();
            }
        }
        panic!("no reply to request {id}");
    };

    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2024-11-05", "capabilities": {},
        "clientInfo": {"name": "test", "version": "0"}}}));
    let instructions = reply(1)["instructions"].as_str().unwrap().to_string();
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "status", "arguments": {}}}));
    let status = reply(2)["content"][0]["text"].as_str().unwrap().to_string();

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    (instructions, status)
}

#[test]
fn a_stale_install_is_said_on_every_read_until_funes_add_runs_again() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let funes_home = tmp.path().join("funes");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&funes_home).unwrap();
    let stamp = funes_home.join("spool/codex.missing");

    // Nothing to say on a machine where nothing is installed.
    let quiet = funes(&home, &funes_home, &["status"]);
    support::assert_success(&quiet);
    assert!(!stderr(&quiet).contains("note:"), "{}", stderr(&quiet));

    // An old hook: `funes index --harness codex` off a terminal, with no spool to drain.
    let refused = funes(&home, &funes_home, &["index", "--harness", "codex"]);
    assert!(!refused.status.success());
    assert_eq!(
        stderr(&refused).lines().next(),
        Some(
            "Error: the codex integration does not match this version of funes. Re-run `funes add codex` to update it."
        )
    );
    assert!(stamp.is_file(), "the refusal leaves its stamp");

    // An integration another binary installed: its manifest speaks a contract this funes does not.
    let clyde = home.join(".funes/agents/clyde");
    fs::create_dir_all(&clyde).unwrap();
    fs::write(
        clyde.join("manifest.json"),
        r#"{"contract_version": 99, "id": "clyde", "label": "Clyde", "repo": "acme/funes-clyde"}"#,
    )
    .unwrap();

    // The CLI says both on stderr and keeps stdout as it was.
    let status = funes(&home, &funes_home, &["status"]);
    support::assert_success(&status);
    let err = stderr(&status);
    let notes = "note: the codex integration does not match this version of funes. Re-run `funes add codex` to update it.\n\
                 note: the clyde integration does not match this version of funes. Re-run `funes add clyde` to update it.\n";
    assert_eq!(err, notes);
    assert!(!stdout(&status).contains("note:"), "{}", stdout(&status));

    // The MCP server says both in its instructions and ahead of every tool's text.
    let (instructions, text) = mcp_status(&home, &funes_home);
    assert!(instructions.contains(notes), "{instructions}");
    assert!(text.starts_with(notes), "{text}");
    assert!(
        text.contains("\n\nmemory: "),
        "the status text follows the notes: {text}"
    );

    // `funes add codex` creates the spool; the next hook finds it and the stamp goes. `funes add
    // clyde` replaces the manifest.
    fs::create_dir_all(funes_home.join("spool/codex")).unwrap();
    let found = funes(&home, &funes_home, &["index", "--harness", "codex"]);
    support::assert_success(&found);
    assert!(!stamp.exists(), "found, so no longer missing");
    fs::write(
        clyde.join("manifest.json"),
        r#"{"contract_version": 1, "id": "clyde", "label": "Clyde", "repo": "acme/funes-clyde"}"#,
    )
    .unwrap();
    let quiet = funes(&home, &funes_home, &["status"]);
    support::assert_success(&quiet);
    assert!(!stderr(&quiet).contains("note:"), "{}", stderr(&quiet));
}
