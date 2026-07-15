//! `funes add hermes`: register funes recall as an MCP server with hermes.
//!
//! hermes has a native MCP client, so funes is consumed as its stdio MCP server —
//! `hermes mcp add funes --command funes --args mcp [store]`. hermes' `--args` takes the whole
//! served arg list, so a non-local `store` rides along as an extra token (`--args mcp <store>`),
//! binding this agent's recall to it. The command is `funes` from PATH (override with `FUNES_BIN`).
//! hermes' `mcp add` writes the user config (no project scope).

use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// The `hermes mcp add` argument vector registering `funes mcp [store]`. hermes' `--args` is
/// variadic, so a non-local `store` is appended after `mcp` as another `--args` value.
fn mcp_add_args(funes: &str, store: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["mcp", "add", "funes", "--command", funes, "--args", "mcp"]
        .into_iter()
        .map(String::from)
        .collect();
    if let Some(s) = store {
        args.push(s.to_string());
    }
    args
}

pub fn install(store: Option<String>) -> Result<()> {
    let funes = std::env::var("FUNES_BIN").unwrap_or_else(|_| "funes".to_string());
    let args = mcp_add_args(&funes, store.as_deref());
    let manual = format!("hermes {}", args.join(" "));
    let mut child = match Command::new("hermes").args(&args).stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("`hermes` isn't on PATH — once it is, run:  {manual}");
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
            "`hermes mcp add funes` failed (exit {:?}); run `{manual}` manually to see why.",
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
            ["mcp", "add", "funes", "--command", "funes", "--args", "mcp"]
        );
        assert_eq!(
            mcp_add_args("funes", Some("acme/kb")),
            ["mcp", "add", "funes", "--command", "funes", "--args", "mcp", "acme/kb"]
        );
    }
}
