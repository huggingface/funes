//! `funes ask <agent>`: one grounded answer from a coding agent, nothing installed.
//!
//! `ask` is `add`'s read-only sibling — borrow the agent for a single question instead of wiring
//! it permanently. Claude mounts funes as session-only MCP tools and recalls on its own; codex
//! cannot run MCP tools headless, so recall runs in-process and the passages ride in the prompt.

use anyhow::{anyhow, bail, Result};
use serde_json::json;
use std::process::{Command, Stdio};

use crate::hub::Store;
use crate::recall::{check_readable, recall_hits, store_hint};
use crate::render;

// Recall's CLI defaults; ask exposes no tuning of its own.
const K: usize = 8;
const CANDIDATES: usize = 30;
const HALF_LIFE: f64 = 30.0;
const NEIGHBORS: i64 = 1;

/// The prompt must precede the variadic flags, which would otherwise swallow it; both tools are
/// pre-allowed because print mode cannot prompt for permissions.
fn claude_args(funes: &str, store: Option<&str>, prompt: &str) -> Vec<String> {
    let mut server = vec!["mcp".to_string()];
    if let Some(s) = store {
        server.push(s.to_string());
    }
    let config = json!({ "mcpServers": { "funes": { "command": funes, "args": server } } });
    vec![
        "-p".to_string(),
        prompt.to_string(),
        "--strict-mcp-config".to_string(),
        "--mcp-config".to_string(),
        config.to_string(),
        "--allowedTools".to_string(),
        "mcp__funes__recall,mcp__funes__get".to_string(),
    ]
}

/// codex exec cannot run MCP tools, so `mcp_servers={}` silences any registered ones; `--`
/// guards a dash-leading prompt; `--skip-git-repo-check` lets ask run outside a trusted repo.
fn codex_args(prompt: &str) -> Vec<String> {
    ["exec", "--skip-git-repo-check", "-c", "mcp_servers={}", "--", prompt]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Instruction first, question after: a question must never lead the argv token, where a dash
/// would parse as a flag.
fn claude_prompt(question: &str) -> String {
    format!(
        "Answer the question below using your funes recall tool; drill into hits with get when \
         you need the surrounding turns. Ground the answer in what you retrieve and name the \
         sessions it came from.\n\nQuestion: {question}"
    )
}

/// Warns the model off the `→ get` hints in the passages — they name tools this session
/// doesn't have.
fn codex_prompt(question: &str, passages: &str) -> String {
    format!(
        "Answer the question below from the recalled memory passages that follow. The passages \
         are your complete context — you have no recall tools in this session, and the `→ get` \
         lines inside them are hints for other tools, not commands you can run. Name the sessions \
         you drew from.\n\nQuestion: {question}\n\n== RECALLED PASSAGES ==\n{passages}"
    )
}

/// `funes ask claude`: mount funes as a session-only MCP server and let claude recall on its own.
pub async fn claude(question: String, store: Option<String>) -> Result<()> {
    // A bad store must fail as a funes error — in-session it becomes an LLM apology with exit 0.
    if let Some(spec) = store.as_deref() {
        check_readable(&Store::parse(spec)).await?;
    }
    let funes = funes_bin()?;
    let args = claude_args(&funes, store.as_deref(), &claude_prompt(&question));
    run_agent("claude", &args)
}

/// The grounding for a codex ask. The store is checked first: one that can't be read must fail
/// rather than silently answer from another corpus.
pub async fn codex_grounding(store: Store, question: &str, progress: &(dyn Fn(&str) + Sync)) -> Result<String> {
    check_readable(&store).await?;
    let (note, label, hits) = recall_hits(
        store,
        question.to_string(),
        K,
        CANDIDATES,
        HALF_LIFE,
        NEIGHBORS,
        None,
        None,
        progress,
    )
    .await?;
    if !note.is_empty() {
        // A note despite the check above: the remote dropped mid-recall.
        bail!("the named store went unreachable during recall — try again once you're back online");
    }
    if hits.is_empty() {
        bail!("nothing recalled for that question — no passages to ground an answer in");
    }
    let passages = render::recall_agent("", &store_hint(label.as_deref()), &hits);
    Ok(codex_prompt(question, &passages))
}

/// Spawn codex on an assembled prompt.
pub fn run_codex(prompt: &str) -> Result<()> {
    run_agent("codex", &codex_args(prompt))
}

/// Probe that an agent CLI exists before any expensive work is done on its behalf.
pub fn preflight(agent: &str) -> Result<()> {
    match Command::new(agent)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(missing_agent(agent)),
        Err(e) => Err(anyhow::Error::new(e).context(format!("probing `{agent}`"))),
    }
}

/// `FUNES_BIN`, else this very executable — ask's wiring lives for one process, so the exact
/// running binary is the right default (a persistent registration wants a PATH name instead).
fn funes_bin() -> Result<String> {
    if let Ok(bin) = std::env::var("FUNES_BIN") {
        return Ok(bin);
    }
    Ok(std::env::current_exe()?.display().to_string())
}

/// stdin is closed — an open pipe would feed the child's prompt — and the error never quotes the
/// argv, which embeds the full prompt.
fn run_agent(agent: &str, args: &[String]) -> Result<()> {
    let status = match Command::new(agent).args(args).stdin(Stdio::null()).status() {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing_agent(agent)),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("running `{agent}`"))),
    };
    if status.success() {
        Ok(())
    } else {
        bail!(
            "`{agent}` failed (exit {:?}); its output above should say why",
            status.code()
        )
    }
}

fn missing_agent(agent: &str) -> anyhow::Error {
    let other = if agent == "claude" { "codex" } else { "claude" };
    anyhow!("`{agent}` isn't on PATH — install it, or try `funes ask {other} …`")
}

#[cfg(test)]
mod tests {
    use super::{claude_args, claude_prompt, codex_args, codex_prompt};

    #[test]
    fn bakes_the_store_only_when_present() {
        assert_eq!(
            claude_args("funes", None, "p"),
            [
                "-p",
                "p",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{"funes":{"command":"funes","args":["mcp"]}}}"#,
                "--allowedTools",
                "mcp__funes__recall,mcp__funes__get",
            ]
        );
        assert_eq!(
            claude_args("/opt/funes", Some("acme/kb"), "p"),
            [
                "-p",
                "p",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{"funes":{"command":"/opt/funes","args":["mcp","acme/kb"]}}}"#,
                "--allowedTools",
                "mcp__funes__recall,mcp__funes__get",
            ]
        );
    }

    #[test]
    fn codex_gets_no_servers_and_a_guarded_prompt() {
        assert_eq!(
            codex_args("p"),
            ["exec", "--skip-git-repo-check", "-c", "mcp_servers={}", "--", "p"]
        );
    }

    #[test]
    fn prompts_lead_with_the_instruction() {
        let q = "--help means what?";
        for p in [claude_prompt(q), codex_prompt(q, "passages")] {
            assert!(p.starts_with("Answer the question below"));
            assert!(p.contains(q));
        }
        let codex = codex_prompt(q, "== the passages ==");
        assert!(codex.contains("== the passages =="));
        assert!(codex.contains("not commands you can run"));
    }
}
