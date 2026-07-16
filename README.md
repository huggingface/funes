# funes

**Durable memory for your AI coding agents.** `funes` indexes your past sessions across Claude
Code, Codex, and pi and lets any agent recall the past decisions, rationale, and findings.

![Choosing an embedding model in Claude Code, then Codex recalling that decision in a separate session](docs/img/cross-agents.gif)

*Different agents, one memory: Claude picks an embedding model; a session-end hook indexes it on its own; Codex — a separate agent — uses funes to recall the decision.*

*Look closely at Codex's hits: some timestamps predate this recording. Those are earlier takes of this very demo — funes had already memorized the rehearsals. An append-only memory has no clean take, so we kept the Droste effect rather than pretend otherwise.*

## Features at a glance

- **Your agent recalls your past work.** The model spontaneously uses `funes` to recall prior decisions, rationale, and findings mid-task.
- **One memory across your agents.** Index Claude Code, Codex, and pi into a single store; recall
  spans all of them, and every hit shows which agent it came from.
- **Share across machines or a team.** Publish your store to a Hugging Face dataset you own; a
  teammate or another host recalls it.

## Get funes

The [installer](scripts/install.sh) detects your platform, downloads the matching prebuilt binary,
and puts it on your PATH (`~/.local/bin` by default):

```bash
curl -fsSL https://huggingface.co/buckets/huggingface/funes/resolve/install.sh | sh
```

Then try it:

```bash
funes guide
```

Alternatively, grab a [binary](https://huggingface.co/buckets/huggingface/funes) by hand:

| Platform | Binary |
| --- | --- |
| Linux x86_64 | `funes-x86_64-linux` |
| Linux aarch64 | `funes-aarch64-linux` |
| macOS Apple Silicon | `funes-arm64-apple-darwin` |

```bash
curl -fsSL https://huggingface.co/buckets/huggingface/funes/resolve/funes-x86_64-linux -o funes
chmod +x funes && ./funes guide
```

`funes` works the moment it lands: **`funes guide`** walks you through it before you've indexed
anything, and `funes status` tells you whether recall is reading your own store yet.

Already installed? **`funes update`** replaces the binary in place with the latest build for your
platform (`--force` reinstalls the current one); `funes status` tells you when a newer release is
out.

To build it yourself instead, see [Building from source](#building-from-source).

## The memory loop

`funes` fits into one loop: **index** your past sessions, **add** `funes` to your agent, then just
**ask your agent** — and let it recall on its own.

**1. Index — one store for every agent on the machine.**

```bash
funes index      # sweeps ~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions into one store
```

Run with no arguments and `funes` indexes every supported agent's sessions it finds, into one store.
It's incremental — only new turns are embedded — so it's cheap to re-run as you work — a store runs ~2.3 KB/chunk and grows ~6 MB on a heavy day ([storage growth](docs/storage.md)). Point it at a
path to index one place, or scope to a single agent with `--harness codex`.

`funes` can index sessions from: Claude, Codex, Pi

**2. Add funes to your agent.**

```bash
funes add claude                 # local
funes add claude acme/kb         # …backed by a shared store you own (sync across machines/team)
```

Your agent gets `recall` and `get` as tools, plus instructions on when to use them. For **Claude
and Codex**, `funes add` also wires the automation: it builds your first index if you skipped step 1,
installs a hook that indexes every turn, and — when you name a shared store — publishes at each
session boundary (and does the first push for you). Nothing is left to run by hand; see
[docs/automation.md](docs/automation.md).

`funes` can be added to: Claude, Codex, Pi, Hermes, OpenCode

**3. Ask your agent — and let it recall.**

From here you just work. When something touches a past decision, its rationale, or an earlier
finding, the agent reaches for `recall` itself — no pasting context back in. This holds even for
small models: every model we tested — down to Gemma 4 E4B — invoked recall *spontaneously*, rather
than needing to be told to.

That closes the loop: the work you just did becomes recallable the next time you index — and for
Claude and Codex, `funes add` already installed the hooks that [keep it current](#automate-it) on
their own.

## Works across your agents (and models)

Your memory isn't tied to one tool. Because Claude Code, Codex, and pi all index into a single
store, you can **switch agents without losing anything** — start a task in Claude Code, pick it up
in Codex next week, and each one recalls the *entire* history, not just its own sessions (every hit
shows which agent it came from). Any other agent joins the same store via a `.parquet` trace export.

Models work the same way. funes runs no model of its own, so you reason with whatever your agent
does — through **pi**, any local model or one served through the Hugging Face router. Switch models
between sessions and the memory doesn't move.

## Share across machines or a team

Your local store is a dataset — publish it to a Hugging Face **dataset** repo you own to share it
across machines or a team. Bind the store when you add funes to an agent, and it recalls from there
and keeps it current on its own:

```bash
funes add claude acme/kb   # recall reads acme/kb; the hooks publish there
                           # (builds your first index and does the first push for you)
```

The binding lives in the agent's own config — there's no hidden global default. Under the hood it's
two commands you can also run directly:

```bash
funes push acme/kb                   # publish your local store's new chunks to acme/kb
funes recall "..." --store acme/kb   # read any remote store for one call (no binding needed)
```

That second form is also how you query **someone else's** published memories on a topic —
`funes recall "..." --store other-org/subject-kb` — without touching your own setup.

![pi recalling a past decision from a shared Hugging Face dataset named in the prompt — a project this machine never worked on](docs/img/hub-store.gif)

*A project this machine never worked on: the prompt names `dacorvo/funes-Glint-Research-Fable-5` — ~21.6k chunks on the Hub — and pi recalls the past decision straight from it, one `store` argument on the recall call. Nothing attached, no local index.*

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
$ funes push acme/kb
scanning 512 chunk(s) for secrets…
hf://datasets/acme/kb: nothing published — held back 3 row(s) with secrets (AWS×2, PrivateKey×1); run `funes scrub`, then push again
$ echo $?
2
```

## Automate it

`funes index` is incremental and cheap, but you still have to remember to run it — so for Claude and
Codex, `funes add` already wired it up: every turn indexes automatically, and with a shared store
bound, each session publishes. See [docs/automation.md](docs/automation.md) for what it installs and
how it behaves.

## How it works

```
~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions   (or a .parquet trace)
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
funes recall "why must sparse attention mask future keys before top-k selection" --store dacorvo/funes-Glint-Research-Fable-5
```

Browse the hits and expand any of them into its full surrounding turns.
Install [fzf](https://github.com/junegunn/fzf) for a richer interface.

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
