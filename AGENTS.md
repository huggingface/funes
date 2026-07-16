# funes — agent notes

Read this before changing the code or parsing funes output. The [README](README.md) explains what
funes is, for humans; this file holds the **interface contract** the read commands expose and the
**decisions that already hardened**.

## The read interface

Every read command has two renderings: **agent** — the stable contract below — and **human** —
terminal presentation that is deliberately unstable; never parse it. Selection: human when both
stdin and stdout are terminals, agent otherwise; `--format human|agent` overrides. The MCP server
always returns agent-format strings.

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
fell back to; the built-in guide has no store to name and keeps a bare hint).

| Flag | Default | Meaning |
| --- | --- | --- |
| `-k` | 8 | hits returned |
| `--candidates` | 30 | fused pool reranked before the top-k cut |
| `--half-life` | 30 | recency decay in days (a hit this old keeps half its weight); 0 disables |
| `--neighbors` | 1 | adjacent chunks (by seq) attached per hit; 0 disables |
| `--type` | — | restrict to `text \| thinking \| tool_use \| tool_result` |
| `--harness` | — | restrict to `claude \| codex \| pi` (the stored facet `claude_code` also parses) |
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

### list / status

- `funes list [store] [--limit 50]` — sessions, newest activity first:
  `[<last_ts>] <workdir>/<session8>  chunks=<n>  <first user message, first 120 chars>`.
  CLI-only; not an MCP tool.
- `funes status [store]` — store label, table name, chunk count.

### MCP

`funes mcp [store]` serves stdio; `funes add claude|codex|pi|hermes|opencode` registers it (and for
claude/codex also installs the automation hooks — see [docs/automation.md](docs/automation.md)). A
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
