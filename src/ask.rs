//! `funes ask <agent>`: one grounded answer from a coding agent, nothing installed.
//!
//! Both agents get the same forced grounding: recall runs in-process and the passages ride in
//! the prompt — one model turn, no tools. An A/B against agent-driven recall (claude mounting
//! funes as session MCP tools) showed the agentic loop pays only when the first retrieval
//! misses, at several times the latency and cost; instead, a miss makes the answer say so and
//! point at rephrasing — or at `funes add`, whose persistent wiring is where self-directed
//! recall lives.

use anyhow::{anyhow, bail, Result};
use std::process::{Command, Stdio};

use crate::hub::Memory;
use crate::recall::{check_readable, memory_hint, recall_hits};
use crate::render;

// Recall's CLI defaults; ask exposes no tuning of its own.
const K: usize = 8;
const CANDIDATES: usize = 30;
const HALF_LIFE: f64 = 30.0;
const NEIGHBORS: i64 = 1;

/// The prompt must precede the flags, which would otherwise swallow it; the empty strict MCP
/// config keeps any registered servers out of a session that must answer from the passages alone.
fn claude_args(prompt: &str) -> Vec<String> {
    [
        "-p",
        prompt,
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// codex exec cannot run MCP tools, so `mcp_servers={}` silences any registered ones; `--`
/// guards a dash-leading prompt; `--skip-git-repo-check` lets ask run outside a trusted repo.
fn codex_args(prompt: &str) -> Vec<String> {
    ["exec", "--skip-git-repo-check", "-c", "mcp_servers={}", "--", prompt]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Instruction first, question and passages after. Warns the model off the `→ get` hints in
/// the passages — they name tools this session doesn't have — and gives the miss its exit:
/// the answer must say when the passages fall short, not paper over them.
fn grounded_prompt(question: &str, passages: &str) -> String {
    format!(
        "Answer the question below from the recalled memory passages that follow. The passages \
         are your complete context — you have no recall tools in this session, and the `→ get` \
         lines inside them are hints for other tools, not commands you can run. Answer directly \
         and concisely, and name the sessions you drew from. If the passages don't answer the \
         question, say so plainly and suggest rephrasing it — or wiring this agent to funes with \
         `funes add`, which gives it recall tools to search the memory itself.\n\n\
         Question: {question}\n\n== RECALLED PASSAGES ==\n{passages}"
    )
}

/// `funes ask claude`: recall in-process, then hand claude a prompt with the passages baked in.
pub async fn claude(question: String, memory: Memory) -> Result<()> {
    // The binary probe comes first — grounding pays for a model load.
    preflight("claude")?;
    let prompt = grounding(memory, &question, &|_| ()).await?;
    run_agent("claude", &claude_args(&prompt))
}

/// `funes ask codex`: recall in-process, then hand codex a prompt with the passages baked in.
pub async fn codex(question: String, memory: Memory) -> Result<()> {
    // The binary probe comes first — grounding pays for a model load.
    preflight("codex")?;
    let prompt = grounding(memory, &question, &|_| ()).await?;
    run_agent("codex", &codex_args(&prompt))
}

/// The grounding for an ask. The memory is checked first: one that can't be read must fail
/// rather than silently answer from another corpus.
pub async fn grounding(memory: Memory, question: &str, progress: &(dyn Fn(&str) + Sync)) -> Result<String> {
    check_readable(&memory).await?;
    let (note, label, hits) = recall_hits(
        memory,
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
        bail!("the named memory went unreachable during recall — try again once you're back online");
    }
    if hits.is_empty() {
        bail!("nothing recalled for that question — no passages to ground an answer in");
    }
    let passages = render::recall_agent("", &memory_hint(label.as_deref()), &hits);
    Ok(grounded_prompt(question, &passages))
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
    use super::{claude_args, codex_args, grounded_prompt};

    #[test]
    fn agents_answer_from_a_bare_session() {
        assert_eq!(
            claude_args("p"),
            ["-p", "p", "--strict-mcp-config", "--mcp-config", r#"{"mcpServers":{}}"#]
        );
        assert_eq!(
            codex_args("p"),
            ["exec", "--skip-git-repo-check", "-c", "mcp_servers={}", "--", "p"]
        );
    }

    #[test]
    fn the_prompt_leads_grounds_and_names_the_exits() {
        let p = grounded_prompt("--help means what?", "== the passages ==");
        assert!(p.starts_with("Answer the question below"));
        assert!(p.contains("--help means what?"));
        assert!(p.contains("== the passages =="));
        assert!(p.contains("not commands you can run"));
        assert!(
            p.contains("rephrasing") && p.contains("`funes add`"),
            "the miss has its exits"
        );
    }
}
