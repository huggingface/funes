//! `funes mcp --socket`: one server serving every agent on the host, and `--proxy` reaching it from
//! an integration that can only spawn a stdio server. Each connection is its own MCP session on the
//! one process, which is what keeps a second agent from loading a second copy of the models.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const FUNES: &str = env!("CARGO_BIN_EXE_funes");

/// A server child, killed when the test ends.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start `funes mcp [MEMORY] --socket PATH` and wait for it to listen. The child is killed by the
/// returned guard, which leaves the socket file behind: the next server on that path has to clear it.
fn serve(path: &Path, memory: Option<&str>) -> Server {
    let mut args = vec!["mcp"];
    if let Some(memory) = memory {
        args.push(memory);
    }
    args.extend(["--socket", path.to_str().unwrap()]);
    let child = Command::new(FUNES)
        .args(&args)
        .env("FUNES_HOME", path.parent().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the server");
    // Answering, not merely present: a killed server leaves its socket file behind, and a
    // connection to one on its way out can be accepted and then dropped.
    let hello = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "probe", "version": "0"}}});
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(mut probe) = UnixStream::connect(path) {
            probe.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut line = String::new();
            let answered = writeln!(probe, "{hello}").is_ok()
                && BufReader::new(probe.try_clone().unwrap()).read_line(&mut line).is_ok()
                && !line.is_empty();
            if answered {
                break;
            }
        }
        assert!(Instant::now() < deadline, "no server answering on {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
    Server(child)
}

/// One agent on the socket. Its request ids are odd and far apart, so a reply carrying the other
/// agent's id cannot pass for this one's.
struct Agent {
    out: UnixStream,
    incoming: BufReader<UnixStream>,
    next: u64,
    tag: &'static str,
}

impl Agent {
    fn connect(path: &Path, tag: &'static str) -> Self {
        let out = UnixStream::connect(path).expect("connecting to the socket");
        out.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
        let incoming = BufReader::new(out.try_clone().unwrap());
        Self {
            out,
            incoming,
            next: if tag == "a" { 11 } else { 101 },
            tag,
        }
    }

    fn send(&mut self, method: &str, params: serde_json::Value) -> u64 {
        let id = self.next;
        self.next += 2;
        let request = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.out, "{request}").unwrap();
        id
    }

    fn notify(&mut self, method: &str) {
        let note = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": {}});
        writeln!(self.out, "{note}").unwrap();
    }

    /// The reply to `id`, which is the next line this agent is sent.
    fn reply(&mut self, id: u64) -> serde_json::Value {
        let mut line = String::new();
        let read = self.incoming.read_line(&mut line).unwrap();
        assert!(read > 0, "agent {}: the server closed the connection", self.tag);
        let reply: serde_json::Value = serde_json::from_str(&line).expect("a JSON reply");
        assert_eq!(
            reply["id"].as_u64(),
            Some(id),
            "agent {}: a reply for someone else: {reply}",
            self.tag
        );
        reply
    }

    /// Initialize, and take the notification that follows it.
    fn initialize(&mut self) -> serde_json::Value {
        let name = self.tag.to_string();
        let id = self.send(
            "initialize",
            serde_json::json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": name, "version": "0"}}),
        );
        let reply = self.reply(id);
        assert_eq!(reply["result"]["serverInfo"]["name"], "funes", "the server's name");
        self.notify("notifications/initialized");
        reply
    }

    /// A `tools/list` round trip.
    fn tools(&mut self) {
        let id = self.send("tools/list", serde_json::json!({}));
        let reply = self.reply(id);
        assert!(reply["result"]["tools"].is_array(), "a tool list: {reply}");
    }
}

#[test]
fn one_socket_serves_agent_after_agent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let server = serve(&path, None);

    // The socket is the memory's owner's alone.
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the socket's mode");

    // Two agents, two sessions on the one server, each answered with its own id.
    let mut a = Agent::connect(&path, "a");
    let mut b = Agent::connect(&path, "b");
    a.initialize();
    b.initialize();
    let (ia, ib) = (
        a.send("tools/list", serde_json::json!({})),
        b.send("tools/list", serde_json::json!({})),
    );
    a.reply(ia);
    b.reply(ib);

    // An agent going away takes its own session with it, and no one else's.
    drop(a);
    b.tools();

    // A launcher may start-or-join with the same command: this exits on a path already served.
    let joined = Command::new(FUNES)
        .args(["mcp", "--socket", path.to_str().unwrap()])
        .env("FUNES_HOME", dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("running the second server");
    assert!(joined.success(), "a second server on a served path exits quietly");
    b.tools();

    // Agents coming and going, with the memory named before the flag as the docs have it.
    for _ in 0..4 {
        let mut c = Agent::connect(&path, "a");
        c.initialize();
        c.tools();
    }
    b.tools();

    // A server that was killed leaves its socket file behind: the next one takes the path.
    drop(server);
    assert!(path.exists(), "a killed server leaves its socket file");
    let _second = serve(&path, Some("acme/funes-memory"));
    let mut d = Agent::connect(&path, "b");
    d.initialize();
    d.tools();
}

/// Whatever the agent types goes to the socket and every line back comes out, so a stdio-only
/// integration reaches the shared server.
#[test]
fn the_proxy_relays_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let _server = serve(&path, None);

    let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "p", "version": "0"}}});

    let mut proxy = Command::new(FUNES)
        .args(["mcp", "--proxy", path.to_str().unwrap()])
        .env("FUNES_HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the proxy");
    let mut stdin = proxy.stdin.take().unwrap();
    let mut stdout = BufReader::new(proxy.stdout.take().unwrap());

    writeln!(stdin, "{init}").unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&line).expect("a JSON reply");
    assert_eq!(reply["id"].as_u64(), Some(1));
    assert_eq!(reply["result"]["serverInfo"]["name"], "funes");

    // The agent's end closing ends the proxy.
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = proxy.try_wait().unwrap() {
            assert!(status.success(), "the proxy exited {status}");
            break;
        }
        assert!(Instant::now() < deadline, "the proxy outlived its stdin");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// No server on the path: the proxy says so instead of hanging.
#[test]
fn the_proxy_without_a_server_fails() {
    let dir = tempfile::tempdir().unwrap();
    let missing: PathBuf = dir.path().join("nobody.sock");
    let out = Command::new(FUNES)
        .args(["mcp", "--proxy", missing.to_str().unwrap()])
        .env("FUNES_HOME", dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("running the proxy");
    assert!(!out.status.success(), "a proxy with nothing to connect to fails");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("connecting to"), "it says what failed: {stderr}");
}
