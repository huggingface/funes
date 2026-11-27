# Adding and removing funes

`funes add <agent> [memory]` wires funes into a coding agent in one command: the read tools and the
instructions on when to use them, the automation that keeps the memory current, your first index,
and, with a memory bound, the first publish. From there you just work: when something touches a
past decision, its rationale, or an earlier finding, the agent reaches for [`recall`](recall.md)
itself.

| I want to | Run |
| --- | --- |
| wire funes into an agent | `funes add claude` — or `codex`, `pi`, `hermes` |
| the same, with a memory that follows me across machines, or a team | `funes add claude <user\|org>/funes-memory` |
| bind this host to another memory, or back to the local one | `funes bind <memory>`, `funes bind local` |
| bring an integration to its newest release | `funes update` |
| run my own checkout of an integration | `funes add claude --from ~/dev/funes-integrations/claude` |
| install an integration the catalog does not list | `funes add <id> --from <dir \| hf://…/<id>.tar.gz>` |
| take funes out of an agent | `funes remove claude` |
| index transcripts from an archive | its converter, then `funes index <dir>` |
| use funes from another MCP client | `funes mcp [memory]` |

What each command asks, and what it leaves in place, is below in that order.

## Wiring an agent

| Agent | Read tools | Per-turn indexing | Session-boundary publish |
| --- | --- | --- | --- |
| `claude` | ✅ | ✅ (plugin hooks) | ✅ (with a memory bound) |
| `codex` | ✅ | ✅ (plugin hooks, after a `/hooks` review) | ✅ (with a memory bound) |
| `hermes` | ✅ | ✅ (plugin hooks) | ✅ (with a memory bound) |
| `pi` | ✅ | ✅ (extension events) | ✅ (with a memory bound) |

These four are maintained in [huggingface/funes-integrations](https://github.com/huggingface/funes-integrations)
and released on their own; `funes add <id>` installs the newest release for this funes. What each
one installs into its agent, and needs on the box, is in its README there. How they keep the memory
current is in [the repository's README](https://github.com/huggingface/funes-integrations#how-the-bundles-automate).

A first add asks before it builds your first index, and declining installs nothing. At a terminal,
it also asks whether your sessions are English or multilingual, which picks the memory's
[embedding model](index.md#sessions-not-in-english). With an HF token, at a terminal, and no memory
named or bound yet, it offers to set up `<user>/funes-memory` so your memory follows you across
machines. Decline and it stays local.

Then it binds the host to the memory you named or accepted, runs the integration's setup, which
registers the read tools, installs the automation and converts the agent's existing sessions,
builds your first index from them, and, with a memory bound, does the first push.

## Binding a memory

A host has one memory: the one every agent on it recalls from and, for the agents with
publishing, publishes to. `funes bind` sets it, every installed agent following, and does the first
push to a memory this host never pushed to, which the hooks refuse ([push.md](push.md)).
`funes add <agent> <memory>` binds the same way before installing the agent. The memory is an
`<org>/<repo>` shorthand, a full `hf://…` URI, or `local`.

```bash
funes add claude <user|org>/funes-memory   # binds this host, then installs claude on it
funes add codex                            # installs codex on the same memory
funes bind <user|org>/other-memory         # every installed agent follows
funes bind local                           # back to the local memory, every agent following
```

- A memory that does not exist on the Hub yet: `funes add` offers to create it (default no, to
  catch typos). Repositories funes creates are **private**. An existing repository keeps its
  visibility.
- A memory holds one [embedding model](index.md#sessions-not-in-english), so `funes bind` and
  `funes add` refuse one embedded with another model than your local memory. On a host with no
  local memory yet, they create it with the memory's model.
- Publishing needs [TruffleHog](https://github.com/trufflesecurity/trufflehog), on `PATH` or where
  `FUNES_TRUFFLEHOG` points. Without it every push fails, rather than publish content no scan
  checked for credentials. A local-only setup does not need it.

## Getting the newest integration

`funes update` replaces the binary, then brings each installed integration to its newest release
and, when its files changed, re-runs its setup on the host's memory. None of an add's one-time
steps run.

Re-running `funes add <agent>` does the same for one agent, and always re-runs its setup.
`funes remove` first gives a clean slate.

```bash
funes add codex                                                  # on the host's memory, at the catalog's newest release
funes add codex <user|org>/funes-memory                          # binds the host first
funes remove codex && funes add codex <user|org>/funes-memory    # from a clean slate
```

When an install no longer fits this funes, every read starts with a `note:` line naming the
`funes add` to re-run.

## Installing from elsewhere

`--from` installs an integration from a directory, or from an
`hf://buckets/<owner>/<bucket>/<path>/<id>.tar.gz` archive published with a `SHA256SUMS` beside it,
instead of from the catalog. That is how you run your own checkout, and how you install an
integration the catalog does not list: the name is an integration id, not a fixed list.

```bash
funes add claude --from ~/dev/funes-integrations/claude
funes add <id> --from hf://buckets/<owner>/<bucket>/<path>/<id>.tar.gz
```

The install stays at what you named, through `funes update` too, until you name it again. To have a
checkout consulted on every run instead, point `$FUNES_INTEGRATIONS` at it
([configuration.md](configuration.md#environment-reference)).

funes runs the releases it publishes without asking. Anything else, it confirms at the terminal
before running, once until its files change, and refuses off a terminal. It also refuses files
another user could have changed, and will not put one publisher's integration where another's is
installed: `funes remove <id>` first.

Integrations maintained by their authors are listed in funes-integrations'
[COMMUNITY.md](https://github.com/huggingface/funes-integrations/blob/main/COMMUNITY.md). A listing
adds no alias: they install with `--from`, as their authors document.

## Taking funes out of an agent

```bash
funes remove claude                       # or codex, pi, hermes
```

It unregisters funes's tools, removes its automation hooks and the integration's files, and
preserves unrelated agent hooks and configuration. It does **not** delete your local memory, the
original session transcripts, the model and Hub caches, or any published memory. An already-absent
integration is a successful no-op.

## Converting by hand

funes reads no agent's own transcripts, so indexing an agent's history from a machine that never ran
the automation — an archive, a colleague's export — is two steps: the bundle's converter into a
directory of your own, then `funes index` on that directory. A directory you own keeps its files;
only the spool is drained. The converter is each bundle's own, and how to run it is in
[funes-integrations](https://github.com/huggingface/funes-integrations#converting-by-hand):

```bash
<converter> /archive/session.jsonl ~/imports/session.funes.jsonl   # as the bundle's README says
funes index ~/imports
```

A converter emits the same ids the hook does, so a session imported this way and one captured live
are one session in the memory, deduplicated on re-index.

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

To start that server on a memory other than the host's, put it after `mcp`:

```json
{
  "command": "funes",
  "args": ["mcp", "acme/funes-memory"]
}
```

Client configuration filenames and surrounding keys vary, but the spawned command is the same. The
server exposes the read tools: `recall` and `get` ([recall.md](recall.md)), `sessions`, `sketch` and
`scan` ([sessions.md](sessions.md)), and `status`. Each returns the strings the CLI prints.

A tool call naming a `memory` reads it as it stands. One naming none reads the memory the server
was started on, or, started on none, the memory this host is bound to, else the local memory.
[recall.md](recall.md#reading-a-different-memory) says what such a read includes. `mcp` is
read-only: indexing and publishing remain separate commands or automation installed by `funes add`.

## The integration contract

The four above are integrations like any other, and a fifth needs no change to funes and no
approval from anyone: a `manifest.json`, an executable `setup`, and whatever the integration needs
beside them. Using funes needs no integration at all — the read verbs, `funes index` on a turns
file or directory, and `funes push` work with nothing installed, and any producer that writes
[the turns format](funes-jsonl.md) is indexed the same way. An integration is the managed journey
for one agent: setup, conversion, automation, removal. What funes requires of one is
[the contract](../CONTRIBUTING.md#the-integration-contract), and the
[rationale](RATIONALE.md#why-integrations-live-outside-this-repository) explains why integrations
live outside the funes repository.

## See also

- [recall.md](recall.md) — the `recall`/`get` tools your agent now has.
- [index.md](index.md) — building and updating the memory by hand.
- [push.md](push.md) — publishing a memory and sharing it.
