use std::io::Write;
use std::process::{Command, Stdio};

use arrow_array::{Int64Array, StringArray};

const SESSION: &str = r#"{"version":1,"harness":"opencode","session_id":"ses_1","cwd":"/work/project","turns":[{"turn_uuid":"msg_1","parent_uuid":null,"seq":0,"ts":"2026-09-17T12:00:00Z","role":"user","blocks":[{"block_type":"text","text":"external memory text","tool_name":null,"tool_use_id":null},{"block_type":"thinking","text":"private chain of thought","tool_name":null,"tool_use_id":null},{"block_type":"tool_use","text":"cargo test","tool_name":"bash","tool_use_id":"call_1"}]}]}"#;

const COLLISION: &str = concat!(
    r#"{"version":1,"harness":"opencode","session_id":"a","cwd":"/work","turns":[{"turn_uuid":"x:opencode:y","parent_uuid":null,"seq":0,"ts":"2026-09-17T12:00:00Z","role":"user","blocks":[{"block_type":"text","text":"first collision sentinel","tool_name":null,"tool_use_id":null}]}]}"#,
    "\n",
    r#"{"version":1,"harness":"opencode","session_id":"a:opencode:x","cwd":"/work","turns":[{"turn_uuid":"y","parent_uuid":null,"seq":0,"ts":"2026-09-17T12:00:00Z","role":"user","blocks":[{"block_type":"text","text":"second collision sentinel","tool_name":null,"tool_use_id":null}]}]}"#,
    "\n"
);

fn session(harness: &str, session_id: &str, text: &str) -> String {
    let mut value: serde_json::Value = serde_json::from_str(SESSION).unwrap();
    value["harness"] = harness.into();
    value["session_id"] = session_id.into();
    value["turns"][0]["turn_uuid"] = format!("msg_{session_id}").into();
    value["turns"][0]["blocks"] = serde_json::json!([{
        "block_type": "text",
        "text": text,
        "tool_name": null,
        "tool_use_id": null
    }]);
    serde_json::to_string(&value).unwrap()
}

fn grown_session(turns: usize) -> String {
    let mut value: serde_json::Value = serde_json::from_str(&session("opencode", "grown", "turn 0")).unwrap();
    for seq in 1..turns {
        value["turns"].as_array_mut().unwrap().push(serde_json::json!({
            "turn_uuid": format!("msg_grown_{seq}"),
            "parent_uuid": "msg_grown",
            "seq": seq,
            "ts": format!("2026-09-17T12:00:{seq:02}Z"),
            "role": "assistant",
            "blocks": [{
                "block_type": "text",
                "text": format!("turn {seq}"),
                "tool_name": null,
                "tool_use_id": null
            }]
        }));
    }
    serde_json::to_string(&value).unwrap()
}

async fn stored_rows(home: &std::path::Path) -> Vec<(String, String, String, i64, String)> {
    let memory = funes::memory::Memory::parse(&home.join("memory").to_string_lossy());
    let dataset = memory.open().await.unwrap();
    let batches =
        funes::memory::dataset::scan_rows(&dataset, &["id", "session_id", "turn_uuid", "seq", "text"], None, None)
            .await
            .unwrap();
    let mut rows = Vec::new();
    for batch in batches {
        let strings = |name| {
            batch
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
        };
        let ids = strings("id");
        let sessions = strings("session_id");
        let turns = strings("turn_uuid");
        let texts = strings("text");
        let seqs = batch
            .column_by_name("seq")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        for row in 0..batch.num_rows() {
            rows.push((
                ids.value(row).into(),
                sessions.value(row).into(),
                turns.value(row).into(),
                seqs.value(row),
                texts.value(row).into(),
            ));
        }
    }
    rows.sort();
    rows
}

fn duplicate_turn(same_id: bool) -> String {
    let mut value: serde_json::Value = serde_json::from_str(SESSION).unwrap();
    let mut turn = value["turns"][0].clone();
    if same_id {
        turn["seq"] = 1.into();
    } else {
        turn["turn_uuid"] = "different_turn".into();
    }
    value["turns"].as_array_mut().unwrap().push(turn);
    serde_json::to_string(&value).unwrap()
}

fn run(home: &std::path::Path, args: &[&str], stdin: Option<&str>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_funes"));
    command
        .args(args)
        .env("FUNES_HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = command.spawn().unwrap();
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn assert_success(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ingest_help_describes_external_jsonl_input() {
    let out = Command::new(env!("CARGO_BIN_EXE_funes"))
        .args(["ingest", "--help"])
        .output()
        .unwrap();

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[PATH|-]"), "{stdout}");
    assert!(stdout.contains("--no-thinking"), "{stdout}");
}

#[test]
fn ingest_indexes_namespaced_session_once_and_honors_no_thinking() {
    let home = tempfile::tempdir().unwrap();
    let first = run(home.path(), &["ingest", "--no-thinking"], Some(SESSION));
    assert_success(&first);

    let get = run(home.path(), &["get", "opencode:ses_1"], None);
    assert_success(&get);
    let text = String::from_utf8(get.stdout).unwrap();
    assert!(text.contains("external memory text"), "{text}");
    assert!(text.contains("[tool_use bash] cargo test"), "{text}");
    assert!(!text.contains("private chain of thought"), "{text}");

    let second = run(home.path(), &["ingest", "--no-thinking"], Some(SESSION));
    assert_success(&second);
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("chunks=0"),
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );
}

#[test]
fn ingest_bulk_incremental_and_cross_harness_replay_are_append_only() {
    let home = tempfile::tempdir().unwrap();
    let opencode = session("opencode", "same", "from opencode");
    let codex = session("codex", "same", "from codex");
    let bulk = format!("{opencode}\n{codex}\n");

    let first = run(home.path(), &["ingest"], Some(&bulk));
    assert_success(&first);
    let first_stdout = String::from_utf8(first.stdout).unwrap();
    assert!(first_stdout.contains("sessions=2"), "{first_stdout}");
    assert!(first_stdout.contains("chunks=2"), "{first_stdout}");

    for (id, text) in [("opencode:same", "from opencode"), ("codex:same", "from codex")] {
        let get = run(home.path(), &["get", id], None);
        assert_success(&get);
        assert!(String::from_utf8_lossy(&get.stdout).contains(text));
    }

    let rewritten = session("opencode", "same", "rewritten content is ignored");
    let replay_input = format!("{rewritten}\n{codex}\n");
    let replay = run(home.path(), &["ingest"], Some(&replay_input));
    assert_success(&replay);
    assert!(String::from_utf8_lossy(&replay.stdout).contains("chunks=0"));
    let get = run(home.path(), &["get", "opencode:same"], None);
    assert_success(&get);
    let original = String::from_utf8_lossy(&get.stdout);
    assert!(original.contains("from opencode"));
    assert!(!original.contains("rewritten content is ignored"));

    let incremental = session("opencode", "new", "incremental row");
    let added = run(home.path(), &["ingest"], Some(&incremental));
    assert_success(&added);
    assert!(String::from_utf8_lossy(&added.stdout).contains("chunks=1"));
    let get = run(home.path(), &["get", "opencode:new"], None);
    assert_success(&get);
    assert!(String::from_utf8_lossy(&get.stdout).contains("incremental row"));
}

#[tokio::test]
async fn incremental_same_session_matches_fresh_bulk_rows_and_stable_ids() {
    let incremental_home = tempfile::tempdir().unwrap();
    let first = grown_session(1);
    let final_session = grown_session(2);

    assert_success(&run(incremental_home.path(), &["ingest"], Some(&first)));
    let first_rows = stored_rows(incremental_home.path()).await;
    assert_eq!(first_rows.len(), 1);

    assert_success(&run(incremental_home.path(), &["ingest"], Some(&final_session)));
    let incremental_rows = stored_rows(incremental_home.path()).await;
    assert_eq!(incremental_rows.len(), 2);
    assert!(incremental_rows.iter().any(|row| row.0 == first_rows[0].0));
    assert_eq!(
        incremental_rows
            .iter()
            .map(|row| &row.0)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        2
    );

    assert_success(&run(incremental_home.path(), &["ingest"], Some(&final_session)));
    assert_eq!(stored_rows(incremental_home.path()).await, incremental_rows);

    let bulk_home = tempfile::tempdir().unwrap();
    assert_success(&run(bulk_home.path(), &["ingest"], Some(&final_session)));
    assert_eq!(stored_rows(bulk_home.path()).await, incremental_rows);
}

#[test]
fn ingest_rejects_whole_input_before_creating_memory() {
    let invalid = [
        format!("{SESSION}\n{}\n", SESSION.replace("\"version\":1", "\"version\":2")),
        SESSION.replace("\"role\":\"user\"", "\"role\":\"system\""),
        SESSION.replace("\"block_type\":\"text\"", "\"block_type\":\"image\""),
        SESSION.replace("2026-09-17T12:00:00Z", "not-a-timestamp"),
        SESSION.replace("\"seq\":0", "\"seq\":-1"),
        SESSION.replace("\"cwd\":\"/work/project\"", "\"cwd\":\"relative\""),
        SESSION.replace("\"version\":1", "\"version\":1,\"unknown\":true"),
        duplicate_turn(true),
        duplicate_turn(false),
        " ".repeat(64 * 1024 * 1024 + 1),
    ];
    for input in invalid {
        let home = tempfile::tempdir().unwrap();
        let out = run(home.path(), &["ingest"], Some(&input));
        assert!(!out.status.success());
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }
}

#[test]
fn ingest_rejects_colon_session_collision_before_creating_memory() {
    let home = tempfile::tempdir().unwrap();
    let out = run(home.path(), &["ingest"], Some(COLLISION));
    assert!(!out.status.success());
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn ingest_empty_input_is_a_noop() {
    let home = tempfile::tempdir().unwrap();
    let out = run(home.path(), &["ingest"], Some(""));
    assert_success(&out);
    assert!(!home.path().join("memory.lance").exists());
    assert!(!home.path().join("state.json").exists());
}

#[test]
fn ingest_uses_shared_secret_redaction() {
    if funes::scan::Trufflehog::find().is_err() || Command::new("ssh-keygen").arg("-V").output().is_err() {
        eprintln!("skip: trufflehog or ssh-keygen not found");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let keys = tempfile::tempdir().unwrap();
    let key = keys.path().join("id_ed25519");
    let generated = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(generated.success());
    let secret = std::fs::read_to_string(key).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(SESSION).unwrap();
    value["turns"][0]["blocks"][0]["text"] = secret.into();
    let input = serde_json::to_string(&value).unwrap();

    let indexed = run(home.path(), &["ingest"], Some(&input));
    assert_success(&indexed);
    let get = run(home.path(), &["get", "opencode:ses_1"], None);
    assert_success(&get);
    assert!(!String::from_utf8_lossy(&get.stdout).contains("PRIVATE KEY"));
}
