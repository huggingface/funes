# funes

**Durable memory for your AI coding agents.** `funes` indexes your past sessions across Claude
Code, Codex, pi, and Hermes and lets any agent recall the past decisions, rationale, and findings.
Your memory is a dataset you can publish to the Hugging Face Hub — then any machine, teammate, or
agent can recall from it.

![Asking a published memory why funes is append-only; funes recalls the relevant sessions and a coding agent answers, grounded, naming its sources](docs/img/ask.gif)

*Put a question to a memory and borrow a coding agent to answer it: funes recalls the relevant sessions, hands them over, and you get one grounded answer that names the sessions it drew from — nothing installed. Here it reads the public `huggingface/funes-memory` dataset named right in the command.*

## Features at a glance

- **Your agent recalls your past work.** The model spontaneously uses `funes` to recall prior decisions, rationale, and findings mid-task.
- **One memory across your agents.** Index Claude Code, Codex, pi, and Hermes into a single memory;
  recall spans all of them, and every hit shows which agent it came from.
- **Your memory is a Hugging Face dataset.** Publish it to the Hugging Face Hub; a teammate,
  another of your machines — or anyone, if you make it public — recalls from it with one flag.

![A coding agent, mid-task, reaches for funes on its own and recalls from the public huggingface/funes-memory dataset, then answers grounded, naming the session it drew from](docs/img/recall.gif)

*And it happens on its own: with funes added, your agent reaches for `recall` mid-conversation — no command to run. Here it recalls to answer a question about funes's own design, grounded, naming the session it drew from.*

## Get funes

The [installer](scripts/install.sh) detects your platform, downloads the matching prebuilt binary,
verifies its tagged release checksum and version, and puts it on your PATH (`~/.local/bin` by
default):

```bash
curl -fsSL https://huggingface.co/buckets/huggingface/funes/resolve/install.sh | sh
```

Then add it to your agent:

```bash
funes add claude    # or codex, pi, hermes
```

One command onboards you: your agent gets `recall` and `get` as tools, and — for Claude, Codex, and
Hermes — funes builds your first index, installs a hook that keeps it current every turn, and (with a
memory bound) publishes at each session boundary. From here you just work. See
[docs/add.md](docs/add.md) for the agents, memory binding, and what a run does; `funes status` tells
you whether recall is reading your own memory yet.

Tagged binaries and their `SHA256SUMS` manifest are also available in the
[release bucket](https://huggingface.co/buckets/huggingface/funes):

| Platform | Binary |
| --- | --- |
| Linux x86_64 | `funes-x86_64-linux` |
| Linux aarch64 | `funes-aarch64-linux` |
| macOS Apple Silicon | `funes-arm64-apple-darwin` |

The checksum detects corrupt, truncated, or mismatched release downloads. Because the binary and
checksum share the same bucket, it does not authenticate the bucket itself.

Already installed? **`funes update`** replaces the binary in place with the latest build for your
platform (`--force` reinstalls the current one); `funes status` tells you when a newer release is
out. To build it yourself, see [Building from source](#building-from-source).

## Works across your agents (and models)

Your memory isn't tied to one tool. Because Claude Code, Codex, pi, and Hermes all index into a single
memory, you can **switch agents without losing anything** — start a task in Claude Code, pick it up
in Codex next week, and each one recalls the *entire* history, not just its own sessions (every hit
shows which agent it came from). Another agent can join through a compatible `.parquet` trace
export; [the import contract](docs/index.md#parquet-trace-format) defines the required schema.

Models work the same way. funes runs pinned local embedding and reranking models, but no generative
model of its own: you reason with whatever your agent uses — through **pi**, any local model or one
served through the Hugging Face router. Switch models between sessions and the memory doesn't move.

## Your memory on the Hub

Your local memory is a dataset, and it shares the way one does: publish it to a Hugging Face
**dataset** repo you own and it becomes an artifact on the Hub like any model or dataset — owned by
your account or org, gated by your token, readable by whoever you say. Not just the code of a
project, but the *process* behind it — the decisions, dead ends, and rationale — becomes something
an agent can recall.

Dataset repositories created by funes are **private by default**. Existing repositories retain their
current visibility, and making a funes-created memory public is a deliberate change on the Hub.

Bind a memory when you add funes to an agent and it recalls from there and keeps it current on its
own; or run the two commands directly:

```bash
funes push <user|org>/funes-memory                    # publish your local memory's new chunks
funes recall "..." --memory <user|org>/funes-memory   # read any remote memory for one call
```

That second form is how you read **someone else's** published memory on a topic, without touching
your own setup. Publishing is guarded: funes redacts credentials at index time, and a separate,
always-on gate refuses to push any chunk that still contains a secret. And because a published memory
is just a dataset, you can try recall **right now**, before indexing anything of your own:

```bash
funes recall "why is funes append-only" --memory huggingface/funes-memory
```

And to get an **answer** rather than ranked passages, borrow an agent for one question — nothing
installed:

```bash
funes ask claude "why is funes append-only" --memory huggingface/funes-memory   # or: funes ask codex
```

See [docs/push.md](docs/push.md) for publishing, the secrets gate, project memories, and inspecting a
memory; [docs/ask.md](docs/ask.md) for grounded answers; [docs/hub-caching.md](docs/hub-caching.md) for
how remote recall caches to local speed.

## Commands

`funes add` wires all of this into your agent; each command is also usable on its own. Browse the
workflow-oriented [documentation index](docs/README.md) for the complete guides.

| Command | Docs |
| --- | --- |
| `funes add <agent> [memory]` / `funes remove <agent>` / `funes mcp [memory]` | [docs/add.md](docs/add.md) — supported agents, integration lifecycle, generic MCP clients, hooks, and memory binding |
| `funes index [path]` | [docs/index.md](docs/index.md) — build/update the memory; sources, incremental, flags |
| `funes recall "…"` / `funes get …` | [docs/recall.md](docs/recall.md) — recall passages and drill into them |
| `funes ask <agent> "…"` | [docs/ask.md](docs/ask.md) — borrow an agent for a grounded answer |
| `funes push <memory>` (+ `curate`, `scrub`, `status`) | [docs/push.md](docs/push.md) — publish and share a memory |
| `funes update` | [installation and updating](#get-funes) — replace the installed binary with a verified release |

The stable agent-facing output contract for the read commands is specified in [AGENTS.md](AGENTS.md).
The per-turn indexing and session-boundary publishing the hooks run are detailed in
[docs/automation.md](docs/automation.md).

## How it works

`funes add` runs one loop: **index** what you've done, **recall** it when it matters — and index what
you just did, so it's recallable next time. Both halves are one deterministic pipeline: each source
is parsed into a generic turn/block shape, chunked, embedded with a pinned local model, and written to
a local Lance dataset; recall fuses vector + BM25 search, reranks, and reweights by recency. Because
everything downstream of parsing is source-agnostic, adding an agent means implementing one
[`TraceSource`](src/source.rs) trait — not touching the indexing or query path.

`funes` shapes its output for agents, not people — so to put a question to a memory yourself, borrow
an agent: `funes ask` recalls from the memory and answers grounded in what it finds, installing
nothing. `funes recall` prints the raw ranked passages behind an answer, and `funes get` reassembles
any cited turn in full.

- [docs/index.md](docs/index.md) — the indexing pipeline, tiers, and incremental behavior.
- [docs/recall.md](docs/recall.md) — retrieval, drill-down with `get`, and reading a shared memory.
- [docs/ask.md](docs/ask.md) — borrow an agent for a grounded answer, nothing installed.
- [docs/automation.md](docs/automation.md) — the per-turn hooks that keep it all fresh.

## Building from source

Needs a Rust toolchain and **`protoc`** — `lance`'s build scripts compile protobuf at build time
(the finished binary does not need it). Install it one of two ways:

```bash
# System-wide:
sudo apt-get install -y protobuf-compiler   # Debian/Ubuntu
brew install protobuf                        # macOS

# …or repo-local, no sudo (downloads a pinned protoc into .tools/):
./scripts/bootstrap-protoc.sh
export PROTOC="$PWD/.tools/protoc/bin/protoc"
```

Then `cargo build --release` (binary at `target/release/funes`); `cargo test` runs the suite. The
integration test downloads the embedder/reranker weights on first run.

Inference (embedding + reranking) runs on a built-in backend — Accelerate on macOS, pure Rust on
Linux — so the default build has no ML runtime dependency and runs on any glibc ≥ 2.35 (Ubuntu
22.04). An ONNX Runtime backend is available as an opt-in variant:

```bash
cargo build --release --no-default-features --features onnx   # ONNX backend instead
cargo run --release --features onnx --example bench_backends  # A/B both backends
```

## Notes

- **Embedding model is pinned** and stamped into the memory; querying with a different embedding
  model is refused. To change it, rebuild from the transcripts (the memory is a disposable derived
  artifact — the raw text is retained in every row). This is separate from the model you *reason*
  with, which is free to change.
- **Subagent transcripts** (`.../subagents/agent-*.jsonl`) are indexed too.

## Why funes

> *"To think is to forget differences, generalize, make abstractions."*
> — Jorge Luis Borges, *Funes the Memorious*

Why `funes` is built the way it is — and how it compares to other memory tools — is documented in
[docs/RATIONALE.md](docs/RATIONALE.md).

## License

`funes` is licensed under the [Apache License 2.0](LICENSE).
