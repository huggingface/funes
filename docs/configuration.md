# Configuration and local files

funes has no global configuration file. A read's memory is the one it names, else the one this
host is bound to (`funes bind`), else the local memory; the hooks publish to the bound memory. The
files below hold the local derived memory, incremental state, the binding, and integration wiring.

## The funes home

`FUNES_HOME` changes funes's state directory; the default is `~/.funes`.

```bash
FUNES_HOME=/tmp/funes-demo funes index ./traces
FUNES_HOME=/tmp/funes-demo funes recall "what changed"
```

Use the same value on every command that should see that isolated memory. This is useful for demos,
benchmarks, and tests because it leaves the normal local memory untouched.

| Path below `$FUNES_HOME` | Purpose |
| --- | --- |
| `memory/chunks.lance/` | The local Lance memory: passages, provenance, embeddings, and search indexes. |
| `state.json` | Per-source incremental indexing state. |
| `index-coverage.json` | The last sweep's small coverage snapshot used by `funes status`. |
| `pushed/` | Per-remote receipts used to report this host's pending push coverage. |
| `bound` | The memory this host is bound to, as typed to `funes bind`; absent when the host is local. |

The memory and indexing state are derived from the original agent transcripts and can be rebuilt,
and push receipts can be recreated by running `funes push <memory>`.

`FUNES_HOME` does **not** relocate agent configuration, installed integrations, model caches, or
the install id (`~/.funes/install-id`). Those paths must remain stable after an agent records them.

## Agent integration files

`funes add` writes or registers these user-wide files; `funes remove <agent>` removes the matching
registration and funes-owned files/entries:

| Agent | Files or configuration |
| --- | --- |
| Claude Code | Hooks-only plugin under `~/.funes/agents/claude/`; registered through Claude's plugin commands. |
| Codex | Plugin carrying the skill and the hooks under `~/.funes/agents/codex/`; registered through Codex's plugin commands. |
| Hermes | Plugin carrying the automation hooks at `~/.hermes/plugins/funes/`; enabled through hermes' plugin commands. |
| pi | Extension under `~/.funes/agents/pi/`, with its copy of the bound memory. |

Beside each integration's directory, `~/.funes/agents/<id>.json` records what `funes add`
installed there and where the files came from. A funes from before the host binding noted a memory
beside each agent, as `<id>.memory`; the next `funes update` or bare `funes add` moves that into
the binding.

What each integration puts there, and which of its agent's events it hooks, is in the
integration's README — the maintained ones are in
[funes-integrations](https://github.com/huggingface/funes-integrations#readme). Hook logs sit
beside the installed scripts as `funes-sync.log`.

## Authentication

Private-memory reads and all Hub writes need a Hugging Face token. funes uses the first non-empty
token in this order:

1. `HF_TOKEN`
2. `HUGGING_FACE_HUB_TOKEN`
3. `HUGGINGFACE_TOKEN`
4. `~/.cache/huggingface/token`, written by `hf auth login`

A token used only for recall needs read access; `push` needs write
access to the target dataset repository. Public-memory recall needs no token.

## Model and remote caches

The default inference backend downloads its pinned embedder and reranker into the standard
Hugging Face cache (`$HF_HOME/hub`, or `~/.cache/huggingface/hub`). The optional ONNX build uses
fastembed's `.fastembed_cache` under the process working directory unless configured by that
library.

Remote `hf://` recall also uses the standard hf-hub file cache. `HF_HUB_CACHE` can relocate that
cache; `HF_HOME` relocates the broader Hugging Face home. See [hub-caching.md](hub-caching.md) for the
file-grained cache design and cold-versus-warm behavior.

## Environment reference

| Variable | Effect |
| --- | --- |
| `FUNES_HOME` | Local memory and funes state directory; default `~/.funes`. |
| `FUNES_BIN` | Binary path recorded in supported MCP registrations and used by the pi bridge. Hook workers instead find `funes` on `PATH` or in common install directories. |
| `FUNES_MEMORY` | Per-run memory override understood by the pi extension; otherwise its copy of the host's binding is used. |
| `FUNES_INTEGRATIONS` | Directory `funes add` installs an agent integration from, instead of the published one. funes can't vouch for those files, so it asks you to confirm before running them — once, until they change. |
| `FUNES_TRUFFLEHOG` | Explicit TruffleHog binary for secret scanning. Index-time redaction is best-effort; push and scrub scanning fail closed. |
| `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN`, `HUGGINGFACE_TOKEN` | Hugging Face authentication, in the precedence shown above. |
| `HF_HOME` | Hugging Face home, including the default backend's model cache. |
| `HF_HUB_CACHE` | Hugging Face Hub file-cache location, including cached remote-memory files. |
| `HF_HUB_USER_AGENT_ORIGIN` | Origin hf-hub appends to the User-Agent of every Hub request. funes adds `funes/<version>` and `install/<id>` to it, keeping an origin already set, and the agent integrations add `agent/<harness>`. |
| `DO_NOT_TRACK`, `FUNES_NO_TELEMETRY` | Either set keeps the install id out of the User-Agent. |
| `NO_COLOR` | Disable ANSI color in human-facing terminal output. |
| `COLUMNS` | Human-rendering width, clamped to 40–120 columns. |

The host's binding is `$FUNES_HOME/bound`, written by `funes bind` alone, which `funes add <agent>
<memory>` runs first. Each agent's registration carries a copy, kept in step by `bind` re-running
the setups. `funes bind local` unbinds; `funes remove <agent>` removes that agent integration
without touching the binding or the memory.
