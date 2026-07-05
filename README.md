# funes

**Durable, local memory for your AI coding agent.** funes indexes your past sessions across Claude
Code, Codex, and pi and lets the agent recall its own decisions, rationale, and findings mid-task —
the exact passages, with provenance, using whatever model it's running. Everything stays on your
machine; you can query the memories yourself from the CLI to check or debug a result.

## Features at a glance

- **Your agent recalls its own past work.** The model spontaneously uses funes to recall prior decisions, rationale, and findings mid-task.
- **One memory across your agents.** Index Claude Code, Codex, and pi into a single store; recall
  spans all of them, and every hit shows which agent it came from.
- **Runs on your machine.** Indexing and recall are local; nothing is sent anywhere until you `push`.
- **Share across machines or a team.** Publish your index to a Hugging Face dataset you own; a
  teammate or another host recalls it.
- **Secrets stay out.** Credentials are redacted at index time, and a fail-closed gate blocks them
  from any push.

## Get funes

The [installer](scripts/install.sh) detects your platform, downloads the matching prebuilt binary,
and puts it on your PATH (`~/.local/bin` by default):

```bash
curl -fsSL https://huggingface.co/buckets/huggingface/funes/resolve/install.sh | sh
```

Alternatively, grab a [binary](https://huggingface.co/buckets/huggingface/funes) by hand:

| Platform | Binary |
| --- | --- |
| Linux x86_64 | `funes-x86_64-linux` |
| Linux aarch64 | `funes-aarch64-linux` |
| macOS Apple Silicon | `funes-arm64-apple-darwin` |

```bash
curl -fsSL https://huggingface.co/buckets/huggingface/funes/resolve/funes-x86_64-linux -o funes
chmod +x funes && ./funes recall "how do I get started with funes"
```

funes works the moment it lands: with no index yet, `recall` (and `get` / `list`) answer from a
small **built-in guide to funes itself**, so you can feel recall before indexing anything. `funes
status` tells you whether you're reading that built-in guide or your own index.

Already installed? **`funes update`** replaces the binary in place with the latest build for your
platform (`--force` reinstalls the current one); `funes status` tells you when a newer release is
out. Along with `push`, that update check is the only routine call funes makes off your machine.

To build it yourself instead, see [Building from source](#building-from-source).

## The memory loop

funes fits into one loop: **index** your past sessions, **add** funes to your agent, then just
**ask your agent** — and let it recall on its own.

**1. Index — one store for every agent on the machine.**

```bash
funes index      # sweeps ~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions into one store
```

Run with no arguments and funes indexes every supported agent's sessions it finds, into one store.
It's incremental — only new turns are embedded — so it's cheap to re-run as you work. Point it at a
path to index one place, or scope to a single agent with `--harness codex`.

funes can index sessions from: Claude, Codex, Pi

**2. Add funes to your agent.**

```bash
funes add claude
```

Your agent gets `recall` and `get` as tools, plus instructions on when to use them.

funes can be added to: Claude, Codex, Pi, Hermes, OpenCode

**3. Ask your agent — and let it recall.**

From here you just work. When something touches a past decision, its rationale, or an earlier
finding, the agent reaches for `recall` itself — no pasting context back in. This holds even for
small models: every model we tested — down to Gemma 4 E4B — invoked recall *spontaneously*, rather
than needing to be told to.

That closes the loop: the work you just did becomes recallable the next time you index — [automate it](#automate-it)
with a session end hook and it stays current on its own.

## Works across your agents (and models)

Your memory isn't tied to one tool. Because Claude Code, Codex, and pi all index into a single
store, you can **switch agents without losing anything** — start a task in Claude Code, pick it up
in Codex next week, and each one recalls the *entire* history, not just its own sessions (every hit
shows which agent it came from). Any other agent joins the same store via a `.parquet` trace export.

Models work the same way. funes runs no model of its own, so you reason with whatever your agent
does — through **pi**, any local model or one served through the Hugging Face router. Switch models
between sessions and the memory doesn't move.

## Inspect it yourself

Because everything's local, you can query the store yourself from the CLI — handy to check what's
indexed or debug a result:

```bash
funes recall "why did we switch off lancedb"
funes recall "the lance schema" --type tool_use --harness codex --project funes
funes list --project funes                        # browse indexed sessions
funes get <session_id> <turn_uuid>                # expand a hit into its full surrounding turns
```

Each hit prints `[time] agent project/session type score`, a `→ get <session_id> <turn_uuid>` line,
a preview, and a few neighboring chunks. Narrow with `--type` (`text|thinking|tool_use|tool_result`),
`--project`, and `--harness` (`claude_code|codex|pi`); tune with `--k`, `--half-life` (recency
decay), and `--neighbors`.

## Share across machines or a team

Attach a Hugging Face dataset repo you own as your **active store**. `recall` then reads it, and
`funes push` publishes your local index to it — no per-command flags:

```bash
funes use acme/kb          # attach hf://datasets/acme/kb as the active store (persisted in funes.json)
funes index                # build/update the local index
funes push                 # publish the local index's new chunks to the active remote
funes recall "..."         # reads the active remote
funes use local            # detach — back to the local index
```

To query a **different remote for a single call** — say, someone's published memories on a topic —
without changing your default, pass `--remote`:

```bash
funes recall "..." --remote other-org/subject-kb
```

The first push to a store your local index shares no chunks with (a first push, a new host, or the
wrong store) asks to confirm before uploading; off a terminal it refuses rather than guess (`--yes`
overrides). You never need the Hub to use funes locally — it's a tier you opt into.

## Keeping secrets out

funes redacts credentials from each session *before* it's stored. On publish, a separate,
always-on gate withholds any chunk that still contains a secret and exits non-zero, rather than
upload it — run `funes scrub` to clean older rows in place, then push again. This is what makes a
shared store safe to push to.

## Automate it

`funes index` is incremental and cheap, but you still have to remember to run it. To index — and
push — at the end of every agent session instead of by hand, wire up a hook: see
[docs/automation.md](docs/automation.md).

## How it works

```
~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions   (or a .parquet trace)
   │  parse        deterministic — turns (text / thinking / tool_use / tool_result), tagged by agent
   │  chunk        one chunk per content block, tight provenance
   │  embed        pinned local model (BAAI/bge-small-en-v1.5)
   ▼  store        embedded vector store (vector + BM25)
recall(query) ──>  vector + BM25  →  RRF  →  cross-encoder rerank  →  recency  →  neighbors
```

Each source is a [`TraceSource`](src/source.rs) that reads its format into a generic turn/block
shape; everything downstream — chunk → embed → store → recall — is source-agnostic. Adding another
agent means implementing one trait, not touching the index or query path.

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

## Notes

- **Embedding model is pinned** and stamped into the index; querying with a different embedding
  model is refused. To change it, rebuild from the transcripts (the index is a disposable derived
  artifact — the raw text is retained in every row). This is separate from the model you *reason*
  with, which is free to change.
- **Subagent transcripts** (`.../subagents/agent-*.jsonl`) are indexed too.

## Why funes

> *"To think is to forget differences, generalize, make abstractions."*
> — Jorge Luis Borges, *Funes the Memorious*

Why funes is built the way it is — and how it compares to other memory tools — is documented in
[docs/RATIONALE.md](docs/RATIONALE.md).

## License

funes is licensed under the [Apache License 2.0](LICENSE).
