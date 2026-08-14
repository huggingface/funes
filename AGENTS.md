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

### get

`funes get <session_id> [--from <seq>] [--to <seq>] [--memory <label>]` — a range of one session's
turns, splits reassembled into whole blocks. Pass the `--memory` a hint names so the drill-down reads
the same memory the hit came from.

**Turns are addressed by `seq`, and only by seq.** It is the session's own dense counter over its
turns, so a range is literally "turns n through m". A recall or scan hit's `→ get` line already
carries the session, the range around the hit, and the memory — drilling into a hit is running what
it printed, and widening it is editing two numbers. `--from` defaults to the session's start and
`--to` to 20 turns on, so **a session id alone is a valid read**: that is how you read a session you
picked out of `sessions` rather than found by searching.

The turn uuid is provenance, not an address. It identifies a turn across re-indexing (chunk ids are
keyed on it) and is printed with every turn, but nothing takes it as input.

Agent format, per turn, closed by the coordinates read and the session's size:

```
[<ts>] <role> seq<N> turn=<turn_uuid>
<blocks, joined by blank lines>
---
turns <first>-<last> of <total>
```

A single turn can carry a whole file, so rendering stops at ~40,000 characters and says what it left
out and where to resume:

```
turns 0-11 of 786
9 more turn(s) in range not shown — read them with --from 12
```

One turn always renders, however large — a read that answers nothing cannot be narrowed further.
`no turns in that range of session <id> (it holds <n>)` when the coordinates land outside it.

### sessions

`funes sessions [--memory <label>]` — every session in the memory, oldest first. Recall is ranked
retrieval and reaches only what a query reaches; this is the enumerator, and the only thing that
answers how much a memory holds or how much of it a pass covered. Agent format:

```
[<first_ts>] <harness> <workdir>/<session_id> <n> turns
---
<n> sessions
```

Turns are counted distinct, not as rows: chunking is an indexing artifact. The session id is
printed whole, so it feeds `get`, `scan`, and any other id-taking command
directly. `no sessions in <label>` when the memory is empty.

### scan

`funes scan <needle> <session_id> [--from <seq>] [--to <seq>] [--ignore-case] [--context <chars>]
[--memory <label>]` — a literal string found in every block of **one session**. Exhaustive and unranked, where `recall` is
ranked and partial: this is what settles a question of absence, which recall cannot. Both the needle
and the session are required; there is no projection mode.

**One session, by design.** Every question worth asking of a literal is a claim about a session —
does *this* one name an internal project, quote a credential, mention a customer. A memory-wide hit
was never a clearance for any particular session, so the verb cannot make that shape of claim.
Enumerate with `sessions`, then scan the ones you mean to screen.

**Literal, never a pattern.** The step this exists for reads zero hits as clean, so a regex that
silently matched nothing would be a false clearance. `--ignore-case` covers case variation. A
session id that isn't in the memory is an **error**, for the same reason: a typo must not read as
absence.

Splits are stitched back into their block before matching, so a needle straddling a chunk boundary
is still found and a split block reports once rather than once per chunk. Agent format:

```
scan "<needle>" in <session_id> — <m> hits
[<ts>] <block_type> seq<N>
  → get <session_id> --from <seq> --to <seq> --memory <label>
  … <context chars each side of the match, whitespace-collapsed> …
---
```

Hits come in reading order. `no matches for "<needle>" in <session_id>` when zero — needle and
session are both echoed, so a zero is attributable to a specific query against a specific session.

**A cap you can walk past.** Listing stops at 200 hits and names the coordinate to continue at:
`<k> more hits not shown — continue with --from <seq>`. A common word in a long session runs to
thousands of hits (4,402 for `"the"` in a 10,000-turn session), so an elision you cannot reach past
would make the count a dead end. The resume coordinate is the first *dropped* hit's seq, not one past
the last shown: a turn can hold several matching blocks, and `last + 1` would skip the rest of it —
so continuing may repeat a hit but never skips one. The header count is always what was found, not
what was listed.

`--from`/`--to` also scan a stretch of a session on their own, in the same `seq` coordinate `get`
uses. **A window scopes the clearance**: a zero over `--from 500 --to 999` reads `no matches for
"<needle>" in <session_id> turns 500-999`, and clears only those turns. A window that lands outside
the session reports `no turns in that range`, never absence.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--ignore-case` / `-i` | off | fold case when matching |
| `--context` | 100 | chars of surrounding text shown each side of a match |
| `--memory` | local memory | the memory holding the session |

Absence of a match clears only the literal actually passed, in the session actually named.

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
filters, memory), `get` (session_id, from, to, memory), `sessions` (memory), `scan` (needle,
session_id, ignore_case, context, memory), `status` (memory) — each returns the corresponding
agent-format string verbatim. A tool call's `memory` overrides the server's; with neither, it reads
the local memory.

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
