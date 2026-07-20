# funes

**Durable memory for your AI coding agents.** `funes` indexes your past sessions across Claude
Code, Codex, and pi and lets any agent recall the past decisions, rationale, and findings. Your
memory is a dataset you can publish to the Hugging Face Hub — then any machine, teammate,
or agent can recall from it.

![Choosing an embedding model in Claude Code, then Codex recalling that decision in a separate session](docs/img/cross-agents.gif)

*Different agents, one memory: Claude picks an embedding model; a session-end hook indexes it on its own; Codex — a separate agent — uses funes to recall the decision.*

*Look closely at Codex's hits: some timestamps predate this recording. Those are earlier takes of this very demo — funes had already memorized the rehearsals. An append-only memory has no clean take, so we kept the Droste effect rather than pretend otherwise.*

## Features at a glance

- **Your agent recalls your past work.** The model spontaneously uses `funes` to recall prior decisions, rationale, and findings mid-task.
- **One memory across your agents.** Index Claude Code, Codex, and pi into a single store; recall
  spans all of them, and every hit shows which agent it came from.
- **Your memory is a Hugging Face dataset.** Publish it to the Hugging Face Hub; a teammate,
  another of your machines — or anyone, if you make it public — recalls from it with one flag.

## Get funes

The [installer](scripts/install.sh) detects your platform, downloads the matching prebuilt binary,
and puts it on your PATH (`~/.local/bin` by default):

```bash
curl -fsSL https://huggingface.co/buckets/huggingface/funes/resolve/install.sh | sh
```

Then add it to your agent:

```bash
funes add claude    # or codex, pi, hermes
```

Alternatively, grab a [binary](https://huggingface.co/buckets/huggingface/funes) by hand:

| Platform | Binary |
| --- | --- |
| Linux x86_64 | `funes-x86_64-linux` |
| Linux aarch64 | `funes-aarch64-linux` |
| macOS Apple Silicon | `funes-arm64-apple-darwin` |

```bash
curl -fsSL https://huggingface.co/buckets/huggingface/funes/resolve/funes-x86_64-linux -o funes
chmod +x funes && ./funes add claude
```

`funes` works the moment it lands: **`funes add`** onboards you — see
[Add funes to your agent](#add-funes-to-your-agent) below — and `funes status` tells you whether
recall is reading your own store yet.

Already installed? **`funes update`** replaces the binary in place with the latest build for your
platform (`--force` reinstalls the current one); `funes status` tells you when a newer release is
out.

To build it yourself instead, see [Building from source](#building-from-source).

## Add funes to your agent

```bash
funes add claude                           # local
funes add claude <user|org>/funes-memory   # …backed by a store you own (sync across machines/team)
```

One command sets everything up. Your agent gets `recall` and `get` as tools, plus instructions on
when to use them — and for **Claude, Codex, and Hermes**, `funes add` wires the memory itself: it
builds your first index — a fast, text-first pass (about a minute, after asking), with deeper
content backfilling as you work — installs a hook that keeps it current every turn, and — when you
name a remote store — publishes at each session boundary (and does the first push for you). Nothing
is left to run by hand.

`funes` can be added to: Claude, Codex, Pi, and Hermes — pi has no hook system, so one manual
command keeps its recall current ([How it works](#how-it-works)).

From here you just work. When something touches a past decision, its rationale, or an earlier
finding, the agent reaches for `recall` itself — no pasting context back in. This holds even for
small models: every model we tested — down to Gemma 4 E4B — invoked recall *spontaneously*, rather
than needing to be told to.

## Works across your agents (and models)

Your memory isn't tied to one tool. Because Claude Code, Codex, and pi all index into a single
store, you can **switch agents without losing anything** — start a task in Claude Code, pick it up
in Codex next week, and each one recalls the *entire* history, not just its own sessions (every hit
shows which agent it came from). Any other agent joins the same store via a `.parquet` trace export.

Models work the same way. funes runs no model of its own, so you reason with whatever your agent
does — through **pi**, any local model or one served through the Hugging Face router. Switch models
between sessions and the memory doesn't move.

## Your memory on the Hub

Your local store is a dataset, and it shares the way one does: publish it to a Hugging Face
**dataset** repo you own and it becomes an artifact on the Hub like any model or dataset — owned by
your account or org, gated by your token, readable by whoever you say. Not just the code of a
project, but the *process* behind it — the decisions, dead ends, and rationale — becomes something
an agent can recall:

![pi recalling a past decision from a shared Hugging Face dataset named in the prompt — a project this machine never worked on](docs/img/hub-store.gif)

*A project this machine never worked on: the prompt names `dacorvo/funes-Glint-Research-Fable-5` — ~21.6k chunks on the Hub — and pi recalls the past decision straight from it, one `store` argument on the recall call. Nothing attached, no local index.*

To share your own memory across machines or a team, bind the store when you add funes to an agent,
and it recalls from there and keeps it current on its own:

```bash
funes add claude <user|org>/funes-memory   # recall reads it; the hooks publish there
                                           # (builds your first index and does the first push for you)
```

The binding lives in the agent's own config — there's no hidden global default. Under the hood it's
two commands you can also run directly:

```bash
funes push <user|org>/funes-memory                   # publish your local store's new chunks there
funes recall "..." --store <user|org>/funes-memory   # read any remote store for one call (no binding needed)
```

That second form is how the demo above reads **someone else's** published memories on a topic —
`funes recall "..." --store other-org/subject-kb` — without touching your own setup. And to get an
**answer** rather than ranked passages, borrow an agent for one question — nothing installed:

```bash
funes ask claude "..." --store other-org/subject-kb   # or: funes ask codex
```

On its first publish, `funes push` also writes the repo's **dataset card** — what a funes store
is, how to recall from it, live stats — tagged `funes`, so every shared store is recognizable
(and [discoverable](https://huggingface.co/datasets?other=funes)) on the Hub; later pushes keep
the stats fresh. A card you've written yourself is never touched.

The first push to a store that shares no chunks with your local one (a first push, a new host, or the
wrong store) asks to confirm before uploading; off a terminal it refuses rather than guess (`--yes`
overrides). You never need the Hub to use `funes` locally — it's a tier you opt into. Recall over a remote caches whole files to local disk, so warm calls run at local speed ([how caching works](docs/hub-caching.md)).

## Keeping secrets out

`funes` redacts credentials from each session *before* it's stored. On publish, a separate,
always-on gate withholds any chunk that still contains a secret and exits non-zero, rather than
upload it — run `funes scrub` to clean older rows in place, then push again. It removes credentials
only; the rest is published as-is.

For example, when the gate holds back every dirty row, nothing ships and the push exits non-zero:

```console
$ funes push <user|org>/funes-memory
scanning 512 chunk(s) for secrets…
hf://datasets/<user|org>/funes-memory: nothing published — held back 3 row(s) with secrets (AWS×2, PrivateKey×1); run `funes scrub`, then push again
$ echo $?
2
```

## How it works

Underneath `funes add` runs one loop: **index** what you've done, **recall** it when it matters —
and index what you just did, so it's recallable next time.

**Indexing is one command:**

```bash
funes index      # a fast, text-first pass over ~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions, ~/.hermes/state.db
```

Run with no arguments and `funes` does a fast, text-first pass over every supported agent's sessions
it finds, into one store, offering to finish the rest. It's incremental — only new turns are embedded
— so re-running (and the per-turn hooks) fill in the deeper content a bounded step at a time — a store
runs ~2.3 KB/chunk and grows ~6 MB on a heavy day ([storage growth](docs/storage.md)). Point it at a
path to index one place in full, or scope to a single agent with `--harness codex`.

**The automation keeps the loop closed.** For Claude, Codex, and Hermes, the hooks `funes add`
installed index every turn as it completes, and — with a shared store bound — publish at session
boundaries; [docs/automation.md](docs/automation.md) covers exactly what is installed and how it
behaves. For pi (no hook system), re-run `funes index` yourself to keep recall fresh. Hermes
indexing is **beta**.

**Under the hood**, indexing and recall are one deterministic pipeline:

```
~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions, ~/.hermes/state.db   (or a .parquet trace)
   │  parse        deterministic — turns (text / thinking / tool_use / tool_result), tagged by agent
   │  chunk        one chunk per content block, tight provenance
   │  embed        pinned local model (BAAI/bge-small-en-v1.5)
   ▼  store        a local Lance dataset (vector + BM25)
recall(query) ──>  vector + BM25  →  RRF  →  cross-encoder rerank  →  recency  →  neighbors
```

Each source is a [`TraceSource`](src/source.rs) that reads its format into a generic turn/block
shape; everything downstream — chunk → embed → store → recall — is source-agnostic. Adding another
agent means implementing one trait, not touching the indexing or query path.

### Inspect it yourself

`funes` shapes its output for agents, not people.

Run recall in a terminal, though, and it notices — and switches to an interactive mode for inspecting a store by hand:

```bash
funes recall "why is funes append-only" --store huggingface/funes-memory
```

Browse the hits, filter as you type, and press enter on any of them to read its full surrounding
turns — a built-in browser, no extra tools to install.

When you want an **answer** to the same question rather than the passages, `funes ask` borrows a
coding agent for it — it recalls from the store and answers grounded in what it finds:

```bash
funes ask claude "why is funes append-only" --store huggingface/funes-memory
```

The full interface — output formats, flags, defaults — is specified in [AGENTS.md](AGENTS.md).

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

- **Embedding model is pinned** and stamped into the store; querying with a different embedding
  model is refused. To change it, rebuild from the transcripts (the store is a disposable derived
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
