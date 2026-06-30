//! `funes install hermes`: register funes recall as an MCP server with hermes.
//!
//! hermes has a native MCP client, so funes is consumed as its stdio MCP server —
//! `hermes mcp add funes --command funes --args mcp`. The command is `funes` from
//! PATH (override with `FUNES_BIN`), so it resolves to whatever funes the agent runs
//! with — e.g. a sandbox wrapper that points FUNES_HOME at a mounted index. hermes'
//! `mcp add` writes the user config (no project scope).

use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

pub fn install() -> Result<()> {
    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let mut child = match Command::new("hermes")
        .args(["mcp", "add", "funes", "--command", &funes, "--args", "mcp"])
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`hermes` isn't on PATH — once it is, run:  hermes mcp add funes --command {funes} --args mcp");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `hermes mcp add`")),
    };

    // After probing the server, `mcp add` prompts "Enable all N tools?" on stdin and
    // cancels on EOF — feed it "y" so funes' tools are enabled non-interactively.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"y\ny\n");
    }

    let status = child.wait().context("waiting for `hermes mcp add`")?;
    if status.success() {
        println!("installed funes recall into hermes — `mcp_funes_recall`/`_get` are now available.");
        Ok(())
    } else {
        anyhow::bail!(
            "`hermes mcp add funes` failed (exit {:?}); run it manually to see why.",
            status.code()
        );
    }
}
