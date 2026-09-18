//! Exercise the real Funes binary with the official MCP client.
//! Direct HTTP requests cover endpoint policy and interrupted uploads.

use anyhow::{Context, Result};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo, ProtocolVersion, Tool},
    service::{ClientLifecycleMode, ClientServiceExt},
    transport::{streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport},
    ServiceExt,
};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

#[tokio::test]
async fn modern_http_discovers_and_calls_the_existing_tools() -> Result<()> {
    let home = tempfile::tempdir()?;
    let bound = home.path().join("bound");
    let server = HttpServer::new(home.path(), Some(&bound), &[]).await?;
    let transport = StreamableHttpClientTransport::with_client(
        server.client.clone(),
        StreamableHttpClientTransportConfig::with_uri(server.url.clone()),
    );
    let client = ClientInfo::default()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;
    assert_eq!(
        client.peer_info().unwrap().protocol_version,
        ProtocolVersion::V_2026_07_28
    );
    assert_tools(&client.list_all_tools().await?);
    for (args, expected) in [
        (json!({}), bound),
        (
            json!({"memory":home.path().join("override")}),
            home.path().join("override"),
        ),
    ] {
        let result = client
            .call_tool(CallToolRequestParams::new("status").with_arguments(args.as_object().unwrap().clone()))
            .await?;
        assert_ne!(result.is_error, Some(true));
        assert!(result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains(expected.to_str().unwrap()));
    }
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn existing_stdio_handshake_and_tools_still_work() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut child = command(home.path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let transport = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
    let client = ClientInfo::default()
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .serve(transport)
        .await?;
    assert_eq!(
        client.peer_info().unwrap().protocol_version,
        ProtocolVersion::V_2024_11_05
    );
    assert_tools(&client.list_all_tools().await?);
    client.cancel().await?;
    assert!(timeout(WAIT, child.wait()).await??.success());
    Ok(())
}

#[tokio::test]
async fn http_validates_hosts_and_origins_and_allows_explicit_additions() -> Result<()> {
    let home = tempfile::tempdir()?;
    let server = HttpServer::new(
        home.path(),
        None,
        &[
            "--allowed-host",
            "memory.example",
            "--allowed-origin",
            "https://client.example",
        ],
    )
    .await?;
    let message = request(1, "tools/list", json!({}));
    for (header, value, expected) in [
        ("Host", "untrusted.example", 403),
        ("Origin", "https://untrusted.example", 403),
        ("Host", "memory.example", 200),
        ("Origin", "https://client.example", 200),
        ("Origin", server.url.strip_suffix("/mcp").unwrap(), 200),
    ] {
        let response = server.request(&message).header(header, value).send().await?;
        assert_eq!(response.status(), expected, "{header}: {value}");
        assert!(!response.headers().contains_key("mcp-session-id"));
    }
    assert_eq!(server.request(&message).send().await?.status(), 200); // Origin is optional.
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn http_shutdown_releases_the_process_and_listener() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let home = tempfile::tempdir()?;
    for signal in ["-TERM", "-INT"] {
        let mut server = HttpServer::new(home.path(), None, &[]).await?;
        let address = server
            .url
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/mcp")
            .unwrap();
        let mut unfinished = TcpStream::connect(address).await?;
        let headers = format!(
            concat!(
                "POST /mcp HTTP/1.1\r\n",
                "Host: {address}\r\n",
                "Content-Type: application/json\r\n",
                "Accept: application/json, text/event-stream\r\n",
                "MCP-Protocol-Version: {version}\r\n",
                "Mcp-Method: tools/list\r\n",
                "Expect: 100-continue\r\n",
                "Content-Length: 1000\r\n",
                "\r\n",
            ),
            address = address,
            version = VERSION,
        );
        unfinished.write_all(headers.as_bytes()).await?;
        // Wait until the server accepts the upload, then leave its body unfinished.
        const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
        let mut interim = [0; CONTINUE.len()];
        timeout(WAIT, unfinished.read_exact(&mut interim)).await??;
        assert_eq!(interim, CONTINUE);
        unfinished.write_all(b"{").await?;
        assert!(Command::new("kill")
            .args([signal, &server.process.id().unwrap().to_string()])
            .status()
            .await?
            .success());
        assert!(timeout(WAIT, server.process.wait()).await??.success());
        assert!(TcpStream::connect(address).await.is_err());
    }
    Ok(())
}

// Test subprocesses and raw JSON-RPC messages.

const VERSION: &str = "2026-07-28";
const WAIT: Duration = Duration::from_secs(60);

fn request(id: u64, method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "funes-wire-test", "version": "1"}
    });
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_funes"));
    command.env("FUNES_HOME", home);
    command.env_remove("FUNES_MEMORY");
    command.kill_on_drop(true);
    command
}

struct HttpServer {
    process: Child,
    url: String,
    client: reqwest::Client,
}

impl HttpServer {
    async fn new(home: &Path, memory: Option<&Path>, extra: &[&str]) -> Result<Self> {
        let mut command = command(home);
        command.arg("mcp");
        if let Some(memory) = memory {
            command.arg(memory);
        }
        let mut process = command
            .args(["--transport", "streamable-http", "--bind", "127.0.0.1:0"])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut output = BufReader::new(process.stderr.as_mut().unwrap());
        let mut line = String::new();
        timeout(WAIT, output.read_line(&mut line)).await??;
        let url = line
            .trim()
            .strip_prefix("MCP listening at ")
            .context("HTTP listener starts")?
            .to_owned();
        let client = reqwest::Client::builder().timeout(WAIT).no_proxy().build()?;
        Ok(Self { process, url, client })
    }

    fn request(&self, message: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", VERSION)
            .header("Mcp-Method", message["method"].as_str().unwrap())
            .json(message)
    }
}

fn assert_tools(tools: &[Tool]) {
    let names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(names, ["get", "recall", "scan", "sessions", "sketch", "status"]);
}
