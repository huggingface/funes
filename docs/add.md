# Adding funes to an agent

`funes add <agent> [memory]` wires funes into a coding agent in one command. Every agent gets the
`recall` and `get` tools plus instructions on when to use them; for **Claude, Codex, and Hermes** it
also wires the memory itself — building your first index, installing the hooks that keep it current,
and (with a memory bound) doing the first publish. Nothing is left to run by hand.

```bash
funes add claude                           # local
funes add claude <user|org>/funes-memory   # …backed by a memory you own (sync across machines/team)
```

## The agents

| Agent | `recall`/`get` | Per-turn indexing | Session-boundary publish |
| --- | --- | --- | --- |
| `claude` | ✅ | ✅ (plugin hooks) | ✅ (with a memory bound) |
| `codex` | ✅ | ✅ (hooks) | ✅ (`SessionStart` only — no session-end event) |
| `hermes` | ✅ | ✅ **beta** (shell hooks) | ✅ (with a memory bound) |
| `pi` | ✅ | — (no hook system) | — |

**pi** is trace-only and has no hooks, so `funes add pi` registers the read-side tools only; keep its
recall fresh by re-running `funes index` (it sweeps `~/.pi/agent/sessions`). What exactly gets
installed for each agent — and how the hooks behave — is in [automation.md](automation.md).

## Other MCP clients

`funes add` handles the supported agents above. Any client that can launch a stdio MCP server can
use the same read tools by running `funes mcp [memory]`. For example, in the common MCP JSON shape:

```json
{
  "mcpServers": {
    "funes": {
      "command": "funes",
      "args": ["mcp"]
    }
  }
}
```

To bind that server to a shared memory, put the memory after `mcp`:

```json
{
  "command": "funes",
  "args": ["mcp", "acme/funes-memory"]
}
```

Client configuration filenames and surrounding keys vary, but the spawned command is the same. The
server exposes:

| Tool | Purpose | Main arguments |
| --- | --- | --- |
| `recall` | Retrieve ranked passages. | `query`, optional `k`, `block_type`, `harness`, `memory` |
| `get` | Reassemble a hit and its surrounding turns. | `session_id`, `turn_uuid`, optional `window`, `memory` |
| `status` | Inspect memory and synchronization state. | optional `memory` |

Every tool returns the stable agent-format strings defined in [`AGENTS.md`](../AGENTS.md). A
tool-call `memory` overrides the memory bound when the server started; with neither, the server reads
the local memory. `mcp` is read-only: indexing and publishing remain separate commands or automation
installed by `funes add`.

## Binding a memory

The optional positional `[memory]` is the memory this agent recalls from — and, for the agents with
publishing, publishes to:

```bash
funes add claude <user|org>/funes-memory   # recall reads it; the hooks publish there
funes add claude local                     # explicit local (the default)
```

A memory is an `<org>/<repo>` shorthand, a full `hf://…` URI, or `local`. The binding lives in the
**agent's own config** — there is no hidden global default. If you name a memory that doesn't exist on
the Hub yet, `funes add` offers to create it (default no, to catch typos).

With **no memory named**, and an HF token present in a terminal, `funes add` offers to set up
`<user>/funes-memory` for you so your memory follows you across machines; decline and it stays local.
Without a token it stays local and tells you how to enable syncing later.

## What a run does

For Claude, Codex, and Hermes, `funes add` runs the one-time bootstrap the hooks can't do unattended:

1. **Builds your first index** if you don't have one — a fast, text-first pass over that agent's
   sessions (about a minute, after asking). Declining aborts the add; nothing is installed. Deeper
   content and older sessions backfill on later turns.
2. **Installs the hooks and registers the MCP server** (baking in the bound memory).
3. **Does the first push** to a freshly-bound memory — the publish the hook refuses to do off a
   terminal (the wrong-memory guard; see [automation.md](automation.md)).

Re-run `funes add <agent> <memory>` any time to change the memory or refresh the setup — it's
idempotent. On a new host, re-running it once clears the wrong-memory guard for that machine.

From here you just work: when something touches a past decision, its rationale, or an earlier
finding, the agent reaches for [`recall`](recall.md) itself.

## See also

- [recall.md](recall.md) — the `recall`/`get` tools your agent now has.
- [index.md](index.md) — building and updating the memory by hand.
- [push.md](push.md) — publishing a memory and sharing it.
- [automation.md](automation.md) — exactly what the hooks install and how they behave.
