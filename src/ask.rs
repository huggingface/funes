//! `funes ask <agent>`: one grounded answer from a coding agent, nothing installed.
//!
//! Both agents get the same forced grounding: recall runs in-process and the passages ride in
//! the prompt — one model turn, no tools. An A/B against agent-driven recall (claude mounting
//! funes as session MCP tools) showed the agentic loop pays only when the first retrieval
//! misses, at several times the latency and cost; instead, a miss makes the answer say so and
//! point at rephrasing — or at `funes add`, whose persistent wiring is where self-directed
//! recall lives. Both agents stream JSONL events: the events animate the wait
//! ([`crate::banner`]) and only the final answer reaches stdout.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, IsTerminal, Read};
use std::process::{Command, ExitStatus, Stdio};

use crate::banner::{accent, band_width, Banner};
use crate::hub::Memory;
use crate::recall::{check_readable, memory_hint, recall_hits};
use crate::render;

// Recall's CLI defaults; ask exposes no tuning of its own.
const K: usize = 8;
const CANDIDATES: usize = 30;
const HALF_LIFE: f64 = 30.0;
const NEIGHBORS: i64 = 1;

/// The prompt must precede the flags, which would otherwise swallow it; the empty strict MCP
/// config keeps any registered servers out of a session that must answer from the passages
/// alone. stream-json (and the --verbose it requires) turns stdout into the events the wait
/// animation feeds on.
fn claude_args(prompt: &str) -> Vec<String> {
    [
        "-p",
        prompt,
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
        "--output-format",
        "stream-json",
        "--verbose",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// codex exec cannot run MCP tools, so `mcp_servers={}` silences any registered ones; `--`
/// guards a dash-leading prompt; `--skip-git-repo-check` lets ask run outside a trusted repo;
/// `--json` swaps the console chatter for the events the wait animation feeds on.
fn codex_args(prompt: &str) -> Vec<String> {
    [
        "exec",
        "--json",
        "--skip-git-repo-check",
        "-c",
        "mcp_servers={}",
        "--",
        prompt,
    ]
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
         lines inside them are hints for other tools, not commands you can run. Keep the answer \
         short: a few sentences of plain prose, no preamble and no headings, then a final line \
         naming the sessions you drew from. If the passages don't answer the question, say so \
         plainly and suggest rephrasing it — or wiring this agent to funes with `funes add`, \
         which gives it recall tools to search the memory itself.\n\n\
         Question: {question}\n\n== RECALLED PASSAGES ==\n{passages}"
    )
}

/// `funes ask claude`: recall in-process, then hand claude a prompt with the passages baked in.
pub async fn claude(question: String, memory: Memory) -> Result<()> {
    // The binary probe comes first — grounding pays for a model load.
    preflight("claude")?;
    let banner = Banner::start("recalling…");
    let progress = |label: &str| {
        if let Some(b) = &banner {
            b.set(label);
        }
    };
    let prompt = grounding(memory, &question, &progress).await?;
    progress("waking claude…");
    let run = run_streaming("claude", &claude_args(&prompt), claude_event, &progress)?;
    drop(banner);
    conclude("claude", run)
}

/// `funes ask codex`: recall in-process, then hand codex a prompt with the passages baked in.
pub async fn codex(question: String, memory: Memory) -> Result<()> {
    // The binary probe comes first — grounding pays for a model load.
    preflight("codex")?;
    let banner = Banner::start("recalling…");
    let progress = |label: &str| {
        if let Some(b) = &banner {
            b.set(label);
        }
    };
    let prompt = grounding(memory, &question, &progress).await?;
    progress("waking codex…");
    let run = run_streaming("codex", &codex_args(&prompt), codex_event, &progress)?;
    drop(banner);
    conclude("codex", run)
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

/// What one streamed JSONL line contributes to the wait display.
#[derive(Debug, PartialEq)]
enum Event {
    Status(String),
    Answer(String),
    Noise,
}

/// claude stream-json: assistant content blocks show the work; the result event is the answer.
fn claude_event(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Event::Noise;
    };
    match v["type"].as_str() {
        Some("assistant") => {
            let mut status = Event::Noise;
            for block in v["message"]["content"].as_array().into_iter().flatten() {
                status = match block["type"].as_str() {
                    Some("thinking") => Event::Status("thinking…".to_string()),
                    Some("text") => Event::Status("writing the answer…".to_string()),
                    Some("tool_use") => Event::Status(format!("{}…", block["name"].as_str().unwrap_or("tool"))),
                    _ => status,
                };
            }
            status
        }
        Some("result") if v["is_error"] == false => match v["result"].as_str() {
            Some(text) => Event::Answer(text.to_string()),
            None => Event::Noise,
        },
        _ => Event::Noise,
    }
}

/// codex exec --json: completed items show the work; the last agent message is the answer.
fn codex_event(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Event::Noise;
    };
    let item = &v["item"];
    match (v["type"].as_str(), item["type"].as_str()) {
        (Some("turn.started"), _) => Event::Status("thinking…".to_string()),
        (Some("item.completed"), Some("reasoning")) => Event::Status("thinking…".to_string()),
        (Some("item.completed"), Some("command_execution")) => Event::Status(format!(
            "running: {}",
            clip(item["command"].as_str().unwrap_or("a command"), 42)
        )),
        (Some("item.completed"), Some("agent_message")) => match item["text"].as_str() {
            Some(text) => Event::Answer(text.to_string()),
            None => Event::Noise,
        },
        _ => Event::Noise,
    }
}

/// Truncate to `max` display chars, marking the cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// One finished agent run: the answer if one arrived, plus everything the process wrote — kept
/// so a bad ending can be replayed once the animation is gone.
struct Run {
    answer: Option<String>,
    output: String,
    status: ExitStatus,
}

/// Stream an agent's JSONL events: statuses feed `progress`, the answer rides back in the
/// [`Run`]. stdin is closed — an open pipe would feed the child's prompt.
fn run_streaming(
    agent: &str,
    args: &[String],
    event: fn(&str) -> Event,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Run> {
    let mut child = match Command::new(agent)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing_agent(agent)),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("running `{agent}`"))),
    };
    let stderr = child.stderr.take();
    let drain = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut err) = stderr {
            let _ = err.read_to_string(&mut text);
        }
        text
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no stdout pipe on `{agent}`"))?;
    let (mut answer, mut output) = (None, String::new());
    // A read error mid-stream must still reap the child and join the drain below, or we'd leave
    // the child unwaited and leak the stderr thread — so capture it and break, don't `?` out here.
    let mut read_err = None;
    for line in BufReader::new(stdout).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                read_err = Some(e);
                break;
            }
        };
        match event(&line) {
            Event::Status(s) => progress(&s),
            Event::Answer(a) => answer = Some(a),
            Event::Noise => {}
        }
        output.push_str(&line);
        output.push('\n');
    }
    let status = child.wait();
    output.push_str(&drain.join().unwrap_or_default());
    if let Some(e) = read_err {
        return Err(anyhow::Error::new(e).context(format!("reading `{agent}` output")));
    }
    Ok(Run {
        answer,
        output,
        status: status?,
    })
}

/// Past the animation: print the answer, or replay the captured output so the failure stays
/// diagnosable. The bail itself never embeds the output, which can quote the full prompt.
fn conclude(agent: &str, run: Run) -> Result<()> {
    if run.status.success() {
        if let Some(answer) = &run.answer {
            let answer = answer.trim_end();
            // Set the answer off from the command line above it — but only for a human at the
            // terminal; a redirected or piped answer must stay plain text.
            if std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
                println!("{}", accent(&"─".repeat(band_width())));
                // Dim the trailing provenance line, but never a lone single-line answer.
                let mut lines = answer.lines().peekable();
                let mut first = true;
                while let Some(line) = lines.next() {
                    if lines.peek().is_none() && !first {
                        println!("{}", render::dim(line, true));
                    } else {
                        println!("{line}");
                    }
                    first = false;
                }
            } else {
                println!("{answer}");
            }
            return Ok(());
        }
    }
    eprint!("{}", run.output);
    if run.status.success() {
        bail!("`{agent}` ended without an answer; its output above should say why")
    }
    bail!(
        "`{agent}` failed (exit {:?}); its output above should say why",
        run.status.code()
    )
}

fn missing_agent(agent: &str) -> anyhow::Error {
    let other = if agent == "claude" { "codex" } else { "claude" };
    anyhow!("`{agent}` isn't on PATH — install it, or try `funes ask {other} …`")
}

#[cfg(test)]
mod tests {
    use super::{claude_args, claude_event, codex_args, codex_event, grounded_prompt, Event};

    #[test]
    fn agents_get_a_bare_session_and_streamed_events() {
        assert_eq!(
            claude_args("p"),
            [
                "-p",
                "p",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--output-format",
                "stream-json",
                "--verbose",
            ]
        );
        assert_eq!(
            codex_args("p"),
            [
                "exec",
                "--json",
                "--skip-git-repo-check",
                "-c",
                "mcp_servers={}",
                "--",
                "p"
            ]
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

    #[test]
    fn claude_events_surface_the_work_and_the_answer() {
        let think = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#;
        assert_eq!(claude_event(think), Event::Status("thinking…".into()));
        let denied = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#;
        assert_eq!(claude_event(denied), Event::Status("Bash…".into()));
        let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        assert_eq!(claude_event(text), Event::Status("writing the answer…".into()));
        let done = r#"{"type":"result","subtype":"success","is_error":false,"result":"hi"}"#;
        assert_eq!(claude_event(done), Event::Answer("hi".into()));
        let failed = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"sorry"}"#;
        assert_eq!(claude_event(failed), Event::Noise, "an error result is no answer");
        assert_eq!(claude_event("not json"), Event::Noise);
    }

    #[test]
    fn codex_events_surface_the_work_and_the_answer() {
        let msg = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hi"}}"#;
        assert_eq!(codex_event(msg), Event::Answer("hi".into()));
        let think = r#"{"type":"item.completed","item":{"type":"reasoning"}}"#;
        assert_eq!(codex_event(think), Event::Status("thinking…".into()));
        let run = r#"{"type":"item.completed","item":{"type":"command_execution","command":"ls -la"}}"#;
        assert_eq!(codex_event(run), Event::Status("running: ls -la".into()));
        let done = r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#;
        assert_eq!(codex_event(done), Event::Noise);
    }
}
