//! `funes add claude`: register funes recall as an MCP server with Claude Code.
//!
//! Claude Code has a native MCP client, so funes is consumed as its stdio MCP server —
//! `claude mcp add funes -- funes mcp`. The command is `funes` from PATH (override with
//! `FUNES_BIN`). Default scope is `local` (this project, just you); `--global` registers it at
//! `user` scope so recall is available across all your projects.

use anyhow::Result;
use std::process::Command;

pub fn install(global: bool) -> Result<()> {
    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let mut cmd = Command::new("claude");
    cmd.args(["mcp", "add", "funes"]);
    if global {
        cmd.args(["-s", "user"]);
    }
    cmd.args(["--", &funes, "mcp"]);

    let status = match cmd.status() {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let scope = if global { " -s user" } else { "" };
            println!("`claude` isn't on PATH — once it is, run:  claude mcp add funes{scope} -- {funes} mcp");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `claude mcp add`")),
    };

    if status.success() {
        let scope = if global { "user" } else { "local" };
        println!("installed funes recall into Claude Code ({scope} scope) — `recall`/`get` are now available (restart Claude Code if it's running).");
        Ok(())
    } else {
        // `claude mcp add` fails when a server named `funes` is already registered; point there
        // rather than re-emitting a bare exit code.
        let scope = if global { " -s user" } else { "" };
        anyhow::bail!(
            "`claude mcp add funes` failed (exit {:?}) — it may already be registered (see `claude mcp list`), else run `claude mcp add funes{scope} -- {funes} mcp` manually.",
            status.code()
        );
    }
}
