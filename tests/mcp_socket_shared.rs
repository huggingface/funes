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

/// Wait for `child` to exit, and return whether it succeeded.
fn exits(child: &mut Child, within: Duration, what: &str) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.success();
        }
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An agent that writes its requests and closes its end still gets every reply.
#[test]
fn the_proxy_answers_requests_sent_before_stdin_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let _server = serve(&path, None);

    let mut proxy = Command::new(FUNES)
        .args(["mcp", "--proxy", path.to_str().unwrap()])
        .env("FUNES_HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the proxy");
    let mut stdin = proxy.stdin.take().unwrap();
    let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "p", "version": "0"}}});
    let initialized = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}});
    let list = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    write!(stdin, "{init}\n{initialized}\n{list}\n").unwrap();
    drop(stdin);

    let ids: Vec<_> = BufReader::new(proxy.stdout.take().unwrap())
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(&l.unwrap()).unwrap()["id"].as_u64())
        .collect();
    assert_eq!(ids, [Some(1), Some(2)], "every reply is relayed before the proxy exits");
    assert!(exits(
        &mut proxy,
        Duration::from_secs(10),
        "the proxy outlived its session"
    ));
}

/// The server going away ends the proxy, though the agent's stdin stays open.
#[test]
fn the_proxy_exits_with_its_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let server = serve(&path, None);

    let mut proxy = Command::new(FUNES)
        .args(["mcp", "--proxy", path.to_str().unwrap()])
        .env("FUNES_HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the proxy");
    let _stdin = proxy.stdin.take().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    drop(server);
    exits(&mut proxy, Duration::from_secs(10), "the proxy outlived its server");
}

/// A path that holds something other than a socket is refused, and left as it was.
#[test]
fn a_server_never_clears_a_file_that_is_not_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.md");
    std::fs::write(&notes, "keep me").unwrap();
    let out = Command::new(FUNES)
        .args(["mcp", "--socket", notes.to_str().unwrap()])
        .env("FUNES_HOME", dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("running the server");
    assert!(!out.status.success(), "a server on a regular file fails");
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "keep me");
}

/// The version the server on `path` reports, polled until one answers.
fn answering_version(path: &Path) -> String {
    let hello = serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "probe", "version": "0"}}});
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(mut probe) = UnixStream::connect(path) {
            probe.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut line = String::new();
            if writeln!(probe, "{hello}").is_ok() && BufReader::new(probe).read_line(&mut line).is_ok() {
                if let Ok(reply) = serde_json::from_str::<serde_json::Value>(&line) {
                    return reply["result"]["serverInfo"]["version"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                }
            }
        }
        assert!(Instant::now() < deadline, "no server answering on {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `funes mcp --socket PATH`, its output discarded.
fn spawn_server(path: &Path) -> Server {
    Server(
        Command::new(FUNES)
            .args(["mcp", "--socket", path.to_str().unwrap()])
            .env("FUNES_HOME", path.parent().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning the server"),
    )
}

/// Servers started together leave one serving, and every other one exits: none is left running on
/// a socket nobody can reach.
#[test]
fn servers_started_together_leave_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let mut racers: Vec<Server> = (0..4).map(|_| spawn_server(&path)).collect();
    answering_version(&path);
    std::thread::sleep(Duration::from_secs(1));
    // Not a `filter`: it hands the closure a shared reference, and `try_wait` needs `&mut`.
    let mut running = 0;
    for server in racers.iter_mut() {
        if server.0.try_wait().unwrap().is_none() {
            running += 1;
        }
    }
    assert_eq!(running, 1, "one server serves the path");
}

/// A server of another version is replaced, so an upgrade reaches the agents that connect after it.
#[test]
fn a_server_of_another_version_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let old = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for stream in old.incoming().flatten() {
            let mut line = String::new();
            let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
            let reply = serde_json::json!({"jsonrpc": "2.0", "id": 0,
                "result": {"serverInfo": {"name": "funes", "version": "0.0.0-old"}}});
            let _ = writeln!(&stream, "{reply}");
        }
    });
    assert_eq!(answering_version(&path), "0.0.0-old");
    let _new = spawn_server(&path);
    let deadline = Instant::now() + Duration::from_secs(60);
    while answering_version(&path) == "0.0.0-old" {
        assert!(Instant::now() < deadline, "the old server kept the path");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A server whose socket is gone — replaced or removed — keeps serving the agent it has, and leaves
/// with it: an upgrade never leaves a second copy of the models resident.
#[test]
fn a_server_without_its_socket_serves_out_its_agent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let mut server = serve(&path, None);
    let mut agent = Agent::connect(&path, "a");
    agent.initialize();

    // Taking the path is what a replacement does under the old server.
    std::fs::remove_file(&path).unwrap();
    std::thread::sleep(Duration::from_secs(6)); // past the server's next check of the path

    agent.tools();
    assert!(
        server.0.try_wait().unwrap().is_none(),
        "a server with an agent still serves it"
    );

    drop(agent);
    assert!(
        exits(
            &mut server.0,
            Duration::from_secs(10),
            "the replaced server outlived its last agent"
        ),
        "it leaves with its last agent"
    );
}
