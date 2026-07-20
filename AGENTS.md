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
  → get <session_id> <turn_uuid> --store <label>
<the chunk, first 400 chars>
  ~ [<role> <block_type> seq<N>] <neighbor chunk, first 160 chars>
---
```

`no results` when nothing matched. The `→ get` line carries exactly the arguments `get` wants —
including the store the hits were actually read from (an offline degrade names the local store it
fell back to).

| Flag | Default | Meaning |
| --- | --- | --- |
| `-k` | 8 | hits returned |
| `--candidates` | 30 | fused pool reranked before the top-k cut |
| `--half-life` | 30 | recency decay in days (a hit this old keeps half its weight); 0 disables |
| `--neighbors` | 1 | adjacent chunks (by seq) attached per hit; 0 disables |
| `--type` | — | restrict to `text \| thinking \| tool_use \| tool_result` |
| `--harness` | — | restrict to `claude \| codex \| pi \| hermes` (the stored facet `claude_code` also parses) |
| `--store` | local store | the store to read — `<org>/<repo>`, an `hf://…` URI, a local path, or `local` |

### get

`funes get <session_id> <turn_uuid> [--window 3] [--store <label>]` — the named turn plus turns
within the seq window, splits reassembled into whole blocks. Pass the `--store` a recall hint
names so the drill-down reads the same store the hit came from. `--highlight <text>` marks the
text in the human rendering (matched whitespace-insensitively; no effect on the agent format).
Agent format, per turn:

```
[<ts>] <role> seq<N> turn=<turn_uuid>
<blocks, joined by blank lines>
---
```

`turn <uuid> not found in session <id>` when absent.

### ask

`funes ask claude|codex "<question>" [--store <label>]` — one grounded answer from a coding
agent, nothing installed. claude runs with funes mounted as a session-only MCP server
(`claude -p <prompt> --strict-mcp-config --mcp-config <funes mcp [store]> --allowedTools
mcp__funes__recall,mcp__funes__get`) and recalls on its own; codex exec can't run MCP tools (its
tool-approval elicitation is auto-cancelled headless), so funes recalls in-process and embeds the
passages in the prompt (`codex exec --skip-git-repo-check -c mcp_servers={} -- <prompt>`).

stdout is the agent's free-text answer — unlike the read commands, there is nothing stable to
parse. ask reads no stdin. Quote the question (or put `--` before it) when it contains flag-like
words. CLI-only; not an MCP tool.

funes errors before any agent spawns on: a store that can't be read (missing, empty,
unauthorized, no index yet, or unreachable), a missing agent CLI, and (codex) zero
recalled passages. A non-zero agent exit fails funes (exit 1, the child's code quoted).

| Flag | Default | Meaning |
| --- | --- | --- |
| `--store` | local store | the store to ground in — `<org>/<repo>`, an `hf://…` URI, a local path, or `local` |

### list / status

- `funes list [store] [--limit 50]` — sessions, newest activity first:
  `[<last_ts>] <workdir>/<session8>  chunks=<n>  <first user message, first 120 chars>`.
  CLI-only; not an MCP tool.
- `funes status [store]` — store label, table name, chunk count.

### MCP

`funes mcp [store]` serves stdio; `funes add claude|codex|pi|hermes` registers it (and for
claude/codex/hermes also installs the automation hooks — see [docs/automation.md](docs/automation.md)). A
positional `store` binds the server to a store; `funes add <agent> <store>` bakes it into the
registration. Tools: `recall` (query, k, block_type/harness filters, store), `get`
(session_id, turn_uuid, window, store), `status` (store) — each returns the corresponding
agent-format string verbatim. A tool call's `store` overrides the server's; with neither, it reads
the local store.

## Working on the repo

Building needs `protoc` (lance compiles protobuf at build time): system package, or
`./scripts/bootstrap-protoc.sh` then `export PROTOC="$PWD/.tools/protoc/bin/protoc"`. Before
calling work done: `cargo fmt && cargo clippy && cargo test` (the integration tests download the
embedder/reranker weights on first run).

Inference has two backends behind the `Embedder`/`Reranker` traits (`src/inference.rs`): the
default `blas` (src/blas.rs, hand-written forward on Accelerate/faer) and the opt-in `onnx`
(fastembed/ort). CI lints both on every PR, so also run
`cargo clippy --all-targets --no-default-features --features onnx` before calling work done;
`cargo run --release --features onnx --example bench_backends` A/Bs them (latency + agreement).
