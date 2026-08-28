# funes — agent notes

Read this before changing the code or parsing funes output. The [README](README.md) explains what
funes is, for humans; this file holds the **interface contract** the read commands expose and the
**decisions that already hardened**.

## The read interface

The read commands print the **agent** format — the stable contract below — everywhere. `get`
alone also has a **human** rendering (terminal presentation, deliberately unstable; never parse
it), selected when both stdin and stdout are terminals; `--format human|agent` overrides. The
MCP server always returns agent-format strings.

### recall

`funes recall "<free text>" [flags]` — hybrid retrieval (vector + BM25, fused by reciprocal
rank) → cross-encoder rerank → recency reweight → neighbor expansion. Agent format, per hit:

```
[<ts>] <harness> <workdir>/<session8> <block_type>  score=<s.sss>
  → get <session_id> --from <seq> --to <seq> --memory <label>
<the full chunk text>
  ~ [<role> <block_type> seq<N>] <neighbor chunk, first 160 chars>
---
```

`no results` when nothing matched. The `→ get` line carries exactly the arguments `get` wants —
including the memory the hits were actually read from (an offline degrade names the local memory it
fell back to).

| Flag | Default | Meaning |
| --- | --- | --- |
| `-k` | 8 | hits returned |
| `--candidates` | 30 | fused pool reranked before the top-k cut |
| `--half-life` | 30 | recency decay in days (a hit this old keeps half its weight); 0 disables |
| `--neighbors` | 1 | adjacent chunks (by seq) attached per hit; 0 disables |
| `--type` | — | restrict to `text \| thinking \| tool_use \| tool_result` |
| `--harness` | — | restrict to `claude \| codex \| pi \| hermes` (the saved facet `claude_code` also parses) |
| `--memory` | local memory | the memory to read — `<org>/<repo>`, an `hf://…` URI, a local path, or `local` |

### scan

`funes scan <needle> <session_id> [--from <seq>] [--to <seq>] [-i] [--context <n>] [--memory <label>]`
— every block of one session carrying a literal, in reading order. Splits are stitched back together
before matching, so a needle straddling a chunk boundary is still found. `-i` matches regardless of
case; `--from`/`--to` scan a stretch, and then a zero clears only that stretch.

Agent format, a header then one line per carrying block:

```
scan "<needle>" in <session_id>[ turns <a>-<b>] — <n> hits
[<ts>] <block_type> seq<N>
  → get <session_id> --from <seq> --to <seq> --memory <label>
  … <the match, with surrounding text on one line> …
---
```

`no matches for "<needle>" in <session_id>` when nothing carries it — that zero is per session and
per spelling. A capped listing is cut at a turn boundary and names the coordinate to continue from;
when one turn holds more matches than the cap, it says so and names the turn.

### sessions

`funes sessions [--repo <owner/name>] [--since <date>] [--until <date>] [--limit <n>] [--offset <n>]
[--memory <label>]` — every session a memory holds, oldest first, with the prompt each opened with.
`--limit` keeps the most recent rows; `--offset` walks the listing back through time.

Agent format, a row per session plus its opening prompt, closed by the total:

```
[<date>] <harness> <repo|workdir> <n> turns <session_id>
  <the prompt it opened with, one line>
---
<shown> of <total> sessions — <n> older: continue with --offset <n>, or narrow with --repo/--since/--until
```

The session id is printed whole — it is what `get` takes. Turn counts are (seq, turn_uuid) pairs,
the unit `get` renders, so the two agree on a session's size. A complete listing closes with
`<total> sessions`; `no sessions in <label>` when the memory is empty, `no session in <label>
matches` when a filter keeps nothing.

### get

`funes get <session_id> [--from <seq>] [--to <seq>] [--memory <label>]` — a range of one session's
turns, splits reassembled into whole blocks. Pass the `--memory` a hint names so the drill-down reads
the same memory the hit came from.

Turns are addressed by `seq`, the session's own dense counter over its turns, so a range is turns n
through m. A recall hit's `→ get` line carries the session, the range around the hit, and the memory.
`--from` defaults to the session's start and `--to` to 20 turns on, so a session id alone is a valid
read.

The turn uuid is provenance, not an address: chunk ids are keyed on it and it is printed with every
turn, but nothing takes it as input.

Agent format, per turn, closed by the range read and the session's size:

```
[<ts>] <role> seq<N> turn=<turn_uuid>
<blocks, joined by blank lines>
---
turns <first>-<last> of <total>
```

A read renders 40,000 characters at most and names the coordinate to resume from:

```
turns 0-11 of 786
9 more turn(s) in range not shown — read them with --from 12
```

A turn renders whole or not at all, so a single turn larger than that is the one thing that can
exceed it. `no turns in that range of session <id> (it holds <n>)` when the coordinates land outside
the session, `no session <id> in <label>` when the id is unknown.

### ask

`funes ask claude|codex "<question>" [--memory <label>]` — one grounded answer from a coding
agent, nothing installed. Both agents get the same forced grounding: funes recalls in-process and
embeds the passages in the prompt, so the answer comes back in one turn with no tools — an A/B
against agent-driven recall showed the agentic loop pays only on a first-retrieval miss, at
several times the latency and cost. The session runs with any registered MCP servers silenced
(`claude -p <prompt> --strict-mcp-config --mcp-config {"mcpServers":{}}`; `codex exec
--skip-git-repo-check -c mcp_servers={} -- <prompt>`). A miss is not papered over: the answer says
the passages fall short and points at rephrasing or `funes add` (which wires the agent to funes
with recall tools of its own).

stdout is the agent's free-text answer — unlike the read commands, there is nothing stable to
parse. ask reads no stdin. Quote the question (or put `--` before it) when it contains flag-like
words. CLI-only; not an MCP tool.

funes errors before any agent spawns on: a memory that can't be read (missing, empty,
unauthorized, no index yet, or unreachable), a missing agent CLI, and zero recalled passages.
A non-zero agent exit fails funes (exit 1, the child's code quoted).

| Flag | Default | Meaning |
| --- | --- | --- |
| `--memory` | local memory | the memory to ground in — `<org>/<repo>`, an `hf://…` URI, a local path, or `local` |

### status

- `funes status [memory]` — memory label, chunk and session counts, and when it was last indexed (for a remote, the last push).

### MCP

`funes mcp [memory]` serves stdio; `funes add claude|codex|pi|hermes` registers it (and for
claude/codex/hermes also installs the automation hooks — see [docs/automation.md](docs/automation.md)). A
positional `memory` binds the server to a memory; `funes add <agent> <memory>` bakes it into the
registration. `funes remove <agent>` reverses that agent integration without deleting memories or
transcripts. Tools: `recall` (query, k, half_life, neighbors, candidates, block_type/harness
filters, memory), `get` (session_id, from, to, memory), `status` (memory) — each returns the
corresponding agent-format string verbatim. A tool call's `memory` overrides the server's; with
neither, it reads the local memory.

## Working on the repo

Building needs `protoc` (lance compiles protobuf at build time): system package, or
`./scripts/bootstrap-protoc.sh` then `export PROTOC="$PWD/.tools/protoc/bin/protoc"`. Before
calling work done: `cargo fmt && cargo clippy && cargo test` (the integration tests download the
embedder/reranker weights on first run).

`src/` is one layer per directory — traces, hub, memory, commands, ui, agents, inference — and where
a new function belongs follows from that; the layers and the placement test are in
[CONTRIBUTING.md](CONTRIBUTING.md#style), also as the crate doc in `src/lib.rs`. Ask the domain what
state a memory is in (`Memory::state()`); never read it out of an error shape.

Inference has two backends behind the `Embedder`/`Reranker` traits (`src/inference.rs`): the
default `blas` (src/inference/blas.rs, hand-written forward on Accelerate/faer) and the opt-in `onnx`
(fastembed/ort). CI lints both on every PR, so also run
`cargo clippy --all-targets --no-default-features --features onnx` before calling work done;
`cargo run --release --features onnx --example bench_backends` A/Bs them (latency + agreement).
