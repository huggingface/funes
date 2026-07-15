//! `funes add codex`: register funes recall as an MCP server with Codex.
//!
//! Codex has a native MCP client, so funes is consumed as its stdio MCP server —
//! `codex mcp add funes -- funes mcp [store]`. A non-local `store` binds this agent's recall to it.
//! The command is `funes` from PATH (override with `FUNES_BIN`). Codex's `mcp add` always writes the
//! user config (`~/.codex/config.toml`); it has no project scope, and re-adding an existing server
//! overwrites it (idempotent).

use anyhow::Result;
use std::process::Command;

/// The `codex mcp add` argument vector registering `funes mcp [store]`. A non-local `store` is
/// appended as `funes mcp <store>`, pinning this agent's recall to it.
fn mcp_add_args(funes: &str, store: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["mcp", "add", "funes", "--", funes, "mcp"]
        .into_iter()
        .map(String::from)
        .collect();
    if let Some(s) = store {
        args.push(s.to_string());
    }
    args
}

pub fn install(store: Option<String>) -> Result<()> {
    // The automation hooks (index every turn, publish at session boundaries) are files + a
    // hooks.json edit — no `codex` binary needed — so install them first, regardless of whether the
    // MCP registration below can reach the CLI.
    crate::hooks::install(crate::hooks::Agent::Codex, store.as_deref())?;

    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let args = mcp_add_args(&funes, store.as_deref());
    let manual = format!("codex {}", args.join(" "));
    let status = match Command::new("codex").args(&args).status() {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`codex` isn't on PATH — once it is, run:  {manual}");
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
            "`codex mcp add funes` failed (exit {:?}); run `{manual}` manually to see why.",
            status.code()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::mcp_add_args;

    #[test]
    fn bakes_the_store_only_when_present() {
        assert_eq!(
            mcp_add_args("funes", None),
            ["mcp", "add", "funes", "--", "funes", "mcp"]
        );
        assert_eq!(
            mcp_add_args("funes", Some("acme/kb")),
            ["mcp", "add", "funes", "--", "funes", "mcp", "acme/kb"]
        );
    }
}
