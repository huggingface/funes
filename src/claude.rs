//! `funes add claude`: register funes recall as an MCP server with Claude Code.
//!
//! Claude Code has a native MCP client, so funes is consumed as its stdio MCP server —
//! `claude mcp add funes -s user -- funes mcp [store]`, registered at `user` scope so recall is
//! available across all your projects. `store` (when not local) binds this agent's recall to that
//! store. The command is `funes` from PATH (override with `FUNES_BIN`).

use anyhow::Result;
use std::process::{Command, Stdio};

/// The `claude mcp add` argument vector registering `funes mcp [store]` at user scope. A non-local
/// `store` is appended as `funes mcp <store>`, pinning this agent's recall to it.
fn mcp_add_args(funes: &str, store: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["mcp", "add", "funes", "-s", "user", "--", funes, "mcp"]
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
    // settings.json edit — no `claude` binary needed — so install them first, regardless of whether
    // the MCP registration below can reach the CLI.
    crate::hooks::install(crate::hooks::Agent::Claude, store.as_deref())?;

    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let args = mcp_add_args(&funes, store.as_deref());
    let manual = format!("claude {}", args.join(" "));

    // `claude mcp add` errors if `funes` is already registered, so a re-run — e.g. to change the
    // store — would fail. Remove any existing registration first (silenced and ignored: it errors
    // when absent), so add always succeeds and picks up the current store. Skipped when `claude`
    // isn't on PATH — the add below handles that with a manual hint.
    let _ = Command::new("claude")
        .args(["mcp", "remove", "funes"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let status = match Command::new("claude").args(&args).status() {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`claude` isn't on PATH — once it is, run:  {manual}");
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("running `claude mcp add`")),
    };

    if status.success() {
        println!("installed funes recall into Claude Code (user scope) — `recall`/`get` are now available (restart Claude Code if it's running).");
        Ok(())
    } else {
        anyhow::bail!(
            "`claude mcp add funes` failed (exit {:?}) — run `{manual}` manually to see why.",
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
            ["mcp", "add", "funes", "-s", "user", "--", "funes", "mcp"]
        );
        assert_eq!(
            mcp_add_args("funes", Some("acme/kb")),
            ["mcp", "add", "funes", "-s", "user", "--", "funes", "mcp", "acme/kb"]
        );
    }
}
