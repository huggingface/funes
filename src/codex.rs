//! `funes add codex`: register funes recall as an MCP server with Codex.
//!
//! Codex has a native MCP client, so funes is consumed as its stdio MCP server —
//! `codex mcp add funes -- funes mcp`. The command is `funes` from PATH (override with
//! `FUNES_BIN`). Codex's `mcp add` always writes the user config (`~/.codex/config.toml`); it
//! has no project scope, and re-adding an existing server overwrites it (idempotent).

use anyhow::Result;
use std::process::Command;

pub fn install() -> Result<()> {
    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let status = match Command::new("codex")
        .args(["mcp", "add", "funes", "--", &funes, "mcp"])
        .status()
    {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`codex` isn't on PATH — once it is, run:  codex mcp add funes -- {funes} mcp");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `codex mcp add`")),
    };

    if status.success() {
        println!(
            "installed funes recall into Codex — `recall`/`get` are now available (restart Codex if it's running)."
        );
        Ok(())
    } else {
        anyhow::bail!(
            "`codex mcp add funes` failed (exit {:?}); run `codex mcp add funes -- {funes} mcp` manually to see why.",
            status.code()
        );
    }
}
