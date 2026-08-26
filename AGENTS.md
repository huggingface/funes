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

`funes sessions [--repo <owner/name>] [--since <date>] [--until <date>] [--limit <n>] [--offset <n>]
[--memory <label>]` — the memory's sessions, oldest first. Recall is ranked retrieval and reaches only what a
query reaches; this is the enumerator, and the only thing that answers how much a memory holds,
which sessions a criterion is about, or how much of it a pass covered.

Agent format, two lines per session:

```
[<date>] <harness> <repo-or-workdir> <n> turns <session_id>
  <the prompt the session opened with, one line, cut at 120 chars>
---
<n> sessions
```

The opening prompt is the cheapest triage there is: it says what a session was for without reading
any of it. Injected scaffolding is skipped, so the line is the first thing a human actually typed; a
session whose user turns are all scaffolding carries no second line. `<repo-or-workdir>` is the
session's source repo when its checkout resolved at index time, else the working directory.

Turns are counted distinct, not as rows: chunking is an indexing artifact. The session id is printed
whole, so it feeds `get`, `scan`, and any other id-taking command directly.

**Narrow rather than list everything.** `--repo` keeps the sessions whose checkout resolved to that
repo — the population question rule 1 of a selection criterion actually asks. `--since`/`--until`
bracket the start date inclusively, in `YYYY-MM-DD`.

A listing renders **50 rows by default**, keeping the most recent, and is capped at **200 rows and
~40,000 characters** whatever `--limit` asks for — a larger reply is one nobody receives. What it held
back is both stated and reachable:

```
---
50 of 726 sessions — 676 older: continue with --offset 50, or narrow with --repo/--since/--until
```

`--limit 0` is an **error**, not every match — it once meant that, before a listing had a ceiling.
`--offset` skips that many of the most recent matches before taking `--limit`, so a walk covers the
whole population without repeating or skipping a row: rows are ordered on (timestamp, session id), so
a given offset always names the same session. Dates cannot do this — a day holds many sessions, so a
`--until` resume would either repeat that day or skip part of it. The last page says `the oldest match
reached` rather than offering an offset that returns nothing, and an offset past the matches says so.

`no sessions in <label>` when the memory is empty; `no session in <label> matches` when the filters
keep nothing.

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

### sketch

`funes sketch <session_id> [--units <n>] [--max-chars <n>] [--from <seq>] [--to <seq>] [--memory
<label>]` — what one session **contains**, asked without a query: the passages most distinctive
within it, chosen from the stored vectors.

**This is the only read that does not need to know what it is looking for.** `recall` needs a query,
`scan` needs a literal, `get` needs coordinates, `sessions` gives the opening prompt and nothing
after. Reach for `sketch` for every open-ended judgement about a session — is this worth publishing,
does it reveal anything internal, what was accomplished here. The material that disqualifies a
session is usually the material that does not belong in it, and a distinctiveness selector surfaces
exactly that where a summary drops it.

**Do not read the whole session instead.** A session runs to thousands of turns; reading one end to
end costs more than every other call together and leaves you paging a transcript. Every place a
sketch shows carries a `→ get` line for the surrounding turns verbatim.

```
sketch <session_id> — <n> of <units> places · <chars> of <max-chars> chars · <k> eligible units
  (units clamped to <n>)
[<ts>] <role> <block_type> seq<N> · shortened
  → get <session_id> --from <seq> --to <seq> --memory <label>
<the passage>
---
a sketch samples: it shows what it selected, so it cannot show that anything is absent — scan a literal for that
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--units` | 8 | distinct places to show; clamped to 40, and to what `--max-chars` fits at 240 chars each |
| `--max-chars` | 8,000 | total characters rendered; clamped to 40,000 |
| `--from` / `--to` | whole session | digest a stretch instead — a fixed number of places over 13,000 turns is thin, the same number over each 2,000 is proportional |
| `--memory` | local memory | the memory holding the session |

No passage may take more than its share of the budget, so one turn carrying a file cannot become the
whole digest; a shortened passage says so. Every clamp is reported rather than applied quietly. It
**samples**, so a sketch can never establish absence — `scan` a literal for that.

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
filters, memory), `get` (session_id, from, to, memory), `sessions` (repo, since, until, limit,
offset, memory), `scan` (needle, session_id, from, to, ignore_case, context, memory), `sketch`
(session_id, units, max_chars, from, to, memory), `status` (memory) — each returns
the corresponding agent-format string verbatim. A tool call's `memory` overrides the server's; with
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
