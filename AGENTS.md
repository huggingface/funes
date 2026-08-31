# funes — agent notes

Read this before changing the code or reporting an issue. The [README](README.md) explains what
funes is, for humans; this file holds the conventions and the decisions that already hardened.

## Surfaces to keep in step

A verb is described in four places. A change to one is a change to all four:

| Surface | Where |
| --- | --- |
| CLI help | the clap doc comments in `src/main.rs` |
| The agent contract | `#[tool(description = …)]` and the `schemars` field descriptions in `src/commands/mcp.rs` — what an agent reads before it calls |
| User docs | [docs/recall.md](docs/recall.md) (recall, get), [docs/sessions.md](docs/sessions.md) (sessions, sketch, scan), [docs/ask.md](docs/ask.md), [docs/push.md](docs/push.md) |
| Output shape | the `*_agent` renderers in `src/ui/render.rs`, pinned byte-for-byte by `tests/index_recall.rs` |

Keep each surface to its own job: `--help` and the MCP schemas own flags and defaults, the docs own
usage, the renderers and their tests own the byte shape. A fact repeated across two of them is a fact
that will drift.

## Decisions that already hardened

- **One agent format, everywhere.** The read commands print the same layout to a terminal and to a
  pipe, and the MCP tools return those strings verbatim. There is no human rendering and no
  `--format` switch to add one back.
- **An integration never restates the tool surface.** The pi extension registers what `tools/list`
  returns, so `src/commands/mcp.rs` stays the one place a verb is described to an agent.
- **`--memory` precedence.** A tool call's own `memory` beats the server's (`funes mcp <memory>`),
  which beats the local memory. Same order for the CLI flag.
- **The stored harness facet for Claude is `claude_code`.** `--harness` takes the CLI spellings
  (`claude`, and `claude_code` too) and normalizes before filtering; comparing a raw CLI name against
  the column silently matches nothing.
- **`ask` grounds in one turn.** funes recalls in-process, embeds the passages in the prompt, and
  runs the agent with no tools and its MCP servers silenced. An A/B against agent-driven recall
  showed the agentic loop pays only on a first-retrieval miss, at several times the latency and cost.
- **A memory's state comes from the domain.** Ask `Memory::state()`; never infer it from an error
  shape.

## Working on the repo

Building needs `protoc` (lance compiles protobuf at build time): system package, or
`./scripts/bootstrap-protoc.sh` then `export PROTOC="$PWD/.tools/protoc/bin/protoc"`. Before
calling work done: `cargo fmt && cargo clippy && cargo test` (the integration tests download the
embedder/reranker weights on first run).

`src/` is one layer per directory — traces, hub, memory, commands, ui, agents, inference — and where
a new function belongs follows from that; the layers and the placement test are in
[CONTRIBUTING.md](CONTRIBUTING.md#style), also as the crate doc in `src/lib.rs`.

Inference has two backends behind the `Embedder`/`Reranker` traits (`src/inference.rs`): the
default `blas` (src/inference/blas.rs, hand-written forward on Accelerate/faer) and the opt-in `onnx`
(fastembed/ort). CI lints both on every PR, so also run
`cargo clippy --all-targets --no-default-features --features onnx` before calling work done;
`cargo run --release --features onnx --example bench_backends` A/Bs them (latency + agreement).
