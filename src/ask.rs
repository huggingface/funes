//! `funes ask <agent>`: one grounded answer from a coding agent, nothing installed.
//!
//! `ask` is `add`'s read-only sibling — borrow the agent for a single question instead of wiring
//! it permanently. Claude gets funes recall/get as session-only MCP tools (`--strict-mcp-config`
//! keeps every persistent registration out). Codex cannot run MCP tools headless — its exec mode
//! auto-cancels the tool-approval elicitation — so recall runs in-process here and the passages
//! ride in the prompt. Neither child reads stdin: the grounding must be exactly what was built
//! here, and codex would otherwise block appending a piped stdin to its prompt.

use anyhow::{anyhow, bail, Result};
use serde_json::json;
use std::process::{Command, Stdio};

use crate::hub::Store;
use crate::recall::{check_readable, recall_hits, store_hint};
use crate::render;

// Recall's CLI defaults (main.rs clap attributes); ask exposes no tuning of its own.
const K: usize = 8;
const CANDIDATES: usize = 30;
const HALF_LIFE: f64 = 30.0;
const NEIGHBORS: i64 = 1;

/// The claude one-shot argument vector: print mode with the prompt bound first (claude's variadic
/// flags would otherwise swallow it), funes mounted as the session's only MCP server, and both
/// tools pre-allowed so print mode never stalls on a permission prompt.
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

/// The codex one-shot argument vector. No server is mounted (exec can't run MCP tools) and
/// `mcp_servers={}` empties any registered ones — their tools would only dangle. `--` guards a
/// dash-leading prompt; `--skip-git-repo-check` lets ask run outside a trusted repo.
fn codex_args(prompt: &str) -> Vec<String> {
    ["exec", "--skip-git-repo-check", "-c", "mcp_servers={}", "--", prompt]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Both prompts lead with the instruction and put the question after it — a question must never
/// be the argv token's first byte (a dash-leading one would parse as a flag).
fn claude_prompt(question: &str) -> String {
    format!(
        "Answer the question below using your funes recall tool; drill into hits with get when \
         you need the surrounding turns. Ground the answer in what you retrieve and name the \
         sessions it came from.\n\nQuestion: {question}"
    )
}

/// The codex prompt carries the grounding inline: the passages are the session's complete
/// context, and the `→ get` hints inside them belong to tools this session doesn't have.
fn codex_prompt(question: &str, passages: &str) -> String {
    format!(
        "Answer the question below from the recalled memory passages that follow. The passages \
         are your complete context — you have no recall tools in this session, and the `→ get` \
         lines inside them are hints for other tools, not commands you can run. Name the sessions \
         you drew from.\n\nQuestion: {question}\n\n== RECALLED PASSAGES ==\n{passages}"
    )
}

/// `funes ask claude`: mount funes as a session-only MCP server and let claude answer, recalling
/// and drilling down on its own.
pub async fn claude(question: String, store: Option<String>) -> Result<()> {
    // A store the child can't read must fail here as a funes error — inside the session it only
    // surfaces as a failed tool call, ending as an LLM apology with exit 0. The open also warms
    // the read cache the child's server is about to use.
    if let Some(spec) = store.as_deref() {
        check_readable(&Store::parse(spec)).await?;
    }
    let funes = funes_bin()?;
    let args = claude_args(&funes, store.as_deref(), &claude_prompt(&question));
    run_agent("claude", &args)
}

/// The grounding for a codex ask: recall over `store`, rendered agent-shaped, wrapped in the
/// prompt. The store is checked up front — one that can't be read (or reached) must fail rather
/// than silently answer from another corpus. Callers probe the codex binary first ([`preflight`])
/// so a missing CLI doesn't cost the model load, and own any spinner around this call.
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
        // Only a named remote degrades (the default local store has nothing to degrade to), and
        // check_readable passed just above — the connection dropped in between.
        bail!("the named store went unreachable during recall — try again once you're back online");
    }
    if hits.is_empty() {
        bail!("nothing recalled for that question — no passages to ground an answer in");
    }
    let passages = render::recall_agent("", &store_hint(label.as_deref()), &hits);
    Ok(codex_prompt(question, &passages))
}

/// Spawn codex on an assembled prompt and stream its answer.
pub fn run_codex(prompt: &str) -> Result<()> {
    run_agent("codex", &codex_args(prompt))
}

/// Probe that an agent CLI exists before doing any expensive work on its behalf — a codex ask
/// pays for a full recall (model load included) before its spawn would notice.
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

/// The funes binary the child session serves recall with: `FUNES_BIN`, else this very executable.
/// Ask's wiring lives for one process, so the exact running binary is the right default — the
/// PATH-name convention in the add modules exists for registrations that must survive updates.
fn funes_bin() -> Result<String> {
    if let Ok(bin) = std::env::var("FUNES_BIN") {
        return Ok(bin);
    }
    Ok(std::env::current_exe()?.display().to_string())
}

/// Spawn the agent with the prompt in argv, stdin closed, and the answer streaming to the
/// terminal. The error never quotes the argv — it embeds the full prompt.
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
