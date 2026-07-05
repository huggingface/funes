//! `funes add claude`: register funes recall as an MCP server with Claude Code.
//!
//! Claude Code has a native MCP client, so funes is consumed as its stdio MCP server —
//! `claude mcp add funes -s user -- funes mcp`, registered at `user` scope so recall is
//! available across all your projects. The command is `funes` from PATH (override with
//! `FUNES_BIN`).

use anyhow::Result;
use std::process::Command;

pub fn install() -> Result<()> {
    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let status = match Command::new("claude")
        .args(["mcp", "add", "funes", "-s", "user", "--", &funes, "mcp"])
        .status()
    {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`claude` isn't on PATH — once it is, run:  claude mcp add funes -s user -- {funes} mcp");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `claude mcp add`")),
    };

    if status.success() {
        println!("installed funes recall into Claude Code (user scope) — `recall`/`get` are now available (restart Claude Code if it's running).");
        Ok(())
    } else {
        // `claude mcp add` fails when a server named `funes` is already registered; point there
        // rather than re-emitting a bare exit code.
        anyhow::bail!(
            "`claude mcp add funes` failed (exit {:?}) — it may already be registered (see `claude mcp list`), else run `claude mcp add funes -s user -- {funes} mcp` manually.",
            status.code()
        );
    }
}
