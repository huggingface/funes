# funes

> *"To think is to forget differences, generalize, make abstractions."*
> — Jorge Luis Borges, *Funes the Memorious* (trans. James E. Irby)

Ireneo Funes, after his fall, could forget nothing — every leaf of every tree, every
instant of every day, kept in perfect and infinite detail. It was a **curse**: buried in
particulars, he could no longer generalize, abstract, or think.

An LLM agent has the *opposite* affliction — the **goldfish problem**: the moment a session
ends or the context window fills, everything is gone, and it meets your project as a
stranger, again and again.

**`funes` lives between the two.** It gives any model durable, mid-term memory of your past
sessions — so it stops forgetting — but serves it through **selective, reranked recall**:
you (or the agent) ask, and get back the few relevant passages with exact provenance, not
Funes's drowning flood of detail. Memory that remembers *for* you, and forgets *on your
behalf*.

## What it is (and isn't)

- **Append-only event log, not a mutable knowledge base.** Every chunk is an immutable,
  timestamped record of *what was said when*. Nothing is overwritten; obsolescence is
  resolved at read time by recency + the reader, not by a reconciliation engine.
- **No LLM in the ingest path.** Deterministic parse → chunk → embed. It returns
  *passages*, not LLM-distilled "facts". Interpretation is deferred to read time (you, or an
  orchestrator). This is the property that keeps it stable, debuggable, and private.
- **Local + model-agnostic.** Local embeddings + reranker; the only state is a derived,
  rebuildable index. The transcripts (local + your HF bucket) are the source of truth. Any
  model can query it — switch models per task; nothing is trained into weights.

For the *why* behind these choices — and how funes differs from, and complements, the crowd of memory providers — see [docs/RATIONALE.md](docs/RATIONALE.md).

## Pipeline

```
~/.claude/projects/*.jsonl
   │  parse        deterministic — turns (text / thinking / tool_use / tool_result)
   │  chunk        one chunk per content block, tight provenance
   │  embed        pinned local model (BAAI/bge-small-en-v1.5)
   ▼  store        embedded vector store (vector + BM25)
recall(query) ──>  vector + BM25  →  RRF  →  cross-encoder rerank  →  recency  →  neighbors
```

## Sources

> **For now, the only supported source is Claude Code** — session transcripts under
> `~/.claude/projects/**/*.jsonl` (subagent sidechains included). Support for other agent
> frameworks is planned.

The Claude coupling is confined to the **parse** step: it reads Claude's transcript format
into a generic turn/block shape. Everything downstream — chunk → embed → store → recall — is
source-agnostic and operates on that shape. Adding another framework means writing one new
parser to the same shape, not touching the index or query path.

## Install

The [installer](scripts/install.sh) detects your platform, downloads the matching prebuilt
binary, and puts it on your PATH (`~/.local/bin` by default):

```bash
curl -fsSL https://raw.githubusercontent.com/huggingface/funes/main/scripts/install.sh | sh
```

Pass `-b <dir>` to change the install dir or `-v <tag>` to pin a release, after `sh -s --`.
Or grab a [binary](https://github.com/huggingface/funes/releases) by hand:

| Platform | Binary |
| --- | --- |
| Linux x86_64 | `funes-x86_64-linux` |
| Linux aarch64 | `funes-aarch64-linux` |
| macOS Apple Silicon | `funes-arm64-apple-darwin` |

```bash
curl -fsSL https://github.com/huggingface/funes/releases/latest/download/funes-x86_64-linux -o funes
chmod +x funes && ./funes recall "how do I get started with funes"
```

funes works the moment it lands: with no index yet, `recall` (and `get` / `list`) answer
from a small **built-in guide to funes itself**, so you can feel recall before indexing
anything. `funes status` tells you whether you're reading that built-in guide or your own index.

To build it yourself instead, see [Building from source](#building-from-source).

## Getting started

> `funes` is a single Rust binary (`lance` + `fastembed-rs`, CPU). `index` writes; `recall`
> / `list` / `get` / `status` read.

**1. Index your own history.**

```bash
funes index                                       # parse → chunk → embed ~/.claude/projects → ~/.funes
```

This replaces the built-in guide with recall over your real sessions. Re-run it to pick up
new work — it's incremental (only new turns are embedded), so it's cheap to run often.

**2. Recall.**

```bash
funes recall "how do we parse transcripts"        # hybrid → rerank → recency → neighbors
funes recall "the lance schema" --type tool_use --project <project>
funes list --project <project>                    # browse indexed sessions
funes get <session_id> <turn_uuid>                # expand a hit into its full surrounding turns
funes status
```

`recall` narrows with `--type` (`text|thinking|tool_use|tool_result`) and `--project`, and
each hit prints a `→ get <session_id> <turn_uuid>` line for drilling into the full
surrounding turns.

**3. Let your agent recall on its own.** Register funes as an MCP server, so Claude Code (or
Cursor, …) gets `recall` and `get` as tools:

```bash
claude mcp add funes -- /path/to/funes mcp        # `funes mcp` runs the stdio server
```

New sessions then get the tools **plus** instructions on when to use them, so the agent
reaches for prior decisions and rationale without you pasting context. (The repo also ships
an optional skill at [skills/funes/](skills/funes/) for richer recall-triggering and a
`/funes` command — optional, since the MCP server already carries when-to-use instructions.)

**4. Share it across machines or a team (optional).** Attach a dataset repo you own on the
Hugging Face Hub as your **active store**. From then on `index` publishes to it and `recall`
reads it — no per-command flags:

```bash
funes use acme/kb        # attach hf://datasets/acme/kb as the active store (persisted in funes.json)
funes index              # builds locally, then publishes to the active remote
funes recall "..."       # reads the active remote
funes use local          # detach — back to the local index
```

`funes use` is the one place you name a store; everything else just uses it. To **query a
different remote for a single call** — say, someone's published memories on a topic — without
changing your default, pass `--remote`:

```bash
funes recall "..." --remote other-org/subject-kb
```

`push` is a manual re-publish to the active remote (`index` already publishes on its own). You
never need the Hub to use funes locally — it's a tier you opt into.

## Building from source

Needs a Rust toolchain and **`protoc`** — `lance`'s build scripts compile protobuf at
build time (the finished binary does not need it). Install it one of two ways:

```bash
# System-wide:
sudo apt-get install -y protobuf-compiler   # Debian/Ubuntu
brew install protobuf                        # macOS

# …or repo-local, no sudo (downloads a pinned protoc into .tools/):
./scripts/bootstrap-protoc.sh
export PROTOC="$PWD/.tools/protoc/bin/protoc"
```

Then `cargo build --release` (binary at `target/release/funes`); `cargo test` runs the
suite. The integration test downloads the embedder/reranker weights on first run.

## Notes

- **Embedding model is pinned** and stamped into the index; querying with a different model
  is refused. To change models, rebuild from the transcripts (the index is a disposable
  derived artifact — the raw text is retained in every row).
- **Subagent transcripts** (`.../subagents/agent-*.jsonl`) are indexed too.
