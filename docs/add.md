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

What each command asks, and what it leaves in place, is below in that order. How funes does it is
at the end, for when something looks off.

## Wiring an agent

| Agent | Read tools | Per-turn indexing | Session-boundary publish |
| --- | --- | --- | --- |
| `claude` | ✅ | ✅ (plugin hooks) | ✅ (with a memory bound) |
| `codex` | ✅ | ✅ (plugin hooks, after a `/hooks` review) | ✅ (with a memory bound) |
| `hermes` | ✅ | ✅ **beta** (plugin hooks) | ✅ (with a memory bound) |
| `pi` | ✅ | ✅ (extension events) | ✅ (with a memory bound) |

These four are maintained in [huggingface/funes-integrations](https://github.com/huggingface/funes-integrations)
and released on their own; `funes add <id>` installs the newest release for this funes. What each
one installs into its agent, and needs on the box, is in its README there; how the automation
behaves is in [automation.md](automation.md).

A first add asks before it indexes — declining installs nothing — and, at a terminal, whether your
sessions are English or multilingual, which picks the memory's
[embedding model](index.md#sessions-not-in-english). With an HF token in a terminal and no memory
named or bound yet, it also offers to set up `<user>/funes-memory` so your memory follows you
across machines; decline and it stays local. Without a token it stays local and says how to enable
syncing later. The steps, and what each does without a terminal, are under
[what a run does](#what-a-run-does).

## Binding a memory

A host has one memory: the one every agent on it recalls from and, for the agents with
publishing, publishes to. `funes bind` sets it, and `funes add <agent> <memory>` runs that bind
before installing the agent, as it runs the first index and the first push. The memory is an
`<org>/<repo>` shorthand, a full `hf://…` URI, or `local`.

```bash
funes add claude <user|org>/funes-memory   # binds this host, then installs claude on it
funes add codex                            # installs codex on the same memory
funes bind <user|org>/other-memory         # every installed agent follows
funes bind local                           # back to the local memory, every agent following
```

- The binding is one record for the host, `$FUNES_HOME/bound`, written by `bind` alone. Each
  agent's registration carries a copy, and `bind` re-runs every installed agent's setup so the
  copies follow; a bare `funes add <agent>` installs on it, and `funes update` keeps it.
- A read naming no memory — `funes recall` with no `--memory`, a tool call with no `memory` —
  reads the bound memory as it will be after this host's next push: its rows, and the turns this
  host has indexed but not pushed yet. A read naming a memory, the bound one included, reads it
  as it stands, and `local` reads this host's memory alone;
  [recall.md](recall.md#reading-a-different-memory) has the details.
- A host set up by a funes from before the binding noted a memory beside each agent it bound
  instead. The next `funes update` or bare `funes add` binds the host to it when those agents all
  noted the same one, every installed agent following, and `funes status` says so until then.
  Agents that noted different memories were never meant to: the update names them, and
  `funes bind <memory>` picks one, or `funes bind local` none.
- A memory that does not exist on the Hub yet: `funes add` offers to create it (default no, to
  catch typos). Repositories funes creates are **private by default**; changing that later is an
  explicit action on the Hub. An existing repository keeps its visibility.
- A memory holds one [embedding model](index.md#sessions-not-in-english), so `funes bind` and
  `funes add` refuse a memory embedded with another model than your local memory, before anything
  is bound. On a host with no local memory yet, they create it with the memory's model, and stop
  if they can't read that memory.
- Publishing needs [TruffleHog](https://github.com/trufflesecurity/trufflehog): the first push
  `add` performs and every push the automation runs fail closed unless funes can scan the content
  for credentials, which agent traces commonly capture. funes looks for it on `PATH`, or at the
  path `FUNES_TRUFFLEHOG` names. A local-only setup does not need it.
- The first push to a freshly-bound memory is `bind`'s, or `add`'s, to do, at a terminal: the
  hooks refuse it off one (the wrong-memory guard, in [automation.md](automation.md)). On a new
  host, binding it or re-running `funes add` once clears the guard for that machine, as soon as it
  has an index of its own to push. A memory this host has published to before is left as it is:
  what is pending goes up with the next push, the hooks' or yours. One emptied since is pushed to
  as a first publish again. One whose content was replaced since holds nothing of this host's, so
  the hooks refuse it off a terminal as the wrong memory, and `funes push <memory>` at a terminal
  settles it. A memory at a local path is not pushed to.

## Getting the newest integration

`funes update` replaces the binary, then takes each installed integration to what its source now
holds and re-runs its setup, on the host's memory, only when that changed its files. None of an add's
one-time steps run: no first index, no first push. An install named with `--from` stays as it is,
and so does a copy no source lists. The update says so.

Re-running `funes add <agent>` does the same for one agent, and more: it installs on the host's
memory, or binds the host to the one you name first, and re-runs the integration's setup, which clears what an older funes
put where the current one no longer looks. An integration the catalog installed, or one funes has
no record of installing, is brought to its newest release for this funes first. `funes remove`
first is for a clean slate: it takes the install away in full, your memory aside.

```bash
funes add codex                                                  # on the host's memory, at the catalog's newest release
funes add codex <user|org>/funes-memory                          # binds the host first
funes remove codex && funes add codex <user|org>/funes-memory    # from a clean slate
```

funes tells you when a re-run is due: an install its hooks no longer fit — one from before funes
read its converters' spool, or one another funes made — gets a `note:` line ahead of every read, in
your agent's tool results and on the CLI's stderr, naming the `funes add` to re-run. It goes away
once you have.

## Running your own copy of an integration

`--from <dir>` installs the integration from that directory instead of the catalog, and the install
stays at what you named until you name it again, through `funes update` too:

```bash
funes add claude --from ~/dev/funes-integrations/claude
```

funes vouches only for what it published, so it asks you to confirm at the terminal before running
those files — once, until they change. Off a terminal it refuses rather than assumes. To have a
checkout consulted on every run instead, point `$FUNES_INTEGRATIONS` at it
([configuration.md](configuration.md#environment-reference)).

## Installing an integration the catalog does not list

The name is an integration id, not a fixed list. One the catalog does not list installs from
wherever its publisher put it: a directory holding it, or an
`hf://buckets/<owner>/<bucket>/<path>/<id>.tar.gz` archive published with a `SHA256SUMS` beside it.

```bash
funes add <id> --from <dir>
funes add <id> --from hf://buckets/<owner>/<bucket>/<path>/<id>.tar.gz
```

Both are confirmed at the terminal, as above. Integrations maintained by their authors are listed in
funes-integrations' [COMMUNITY.md](https://github.com/huggingface/funes-integrations/blob/main/COMMUNITY.md);
a listing adds no alias, they install with `--from` as their authors document. Writing one is
covered in [CONTRIBUTING.md](../CONTRIBUTING.md#writing-an-integration), and the
[rationale](RATIONALE.md#why-integrations-live-outside-this-repository) explains why integrations
live outside the funes repository.

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
server exposes:

| Tool | Purpose | Main arguments |
| --- | --- | --- |
| `recall` | Retrieve ranked passages. | `query`, optional `k`, `block_type`, `harness`, `memory` |
| `get` | Reassemble a hit and its surrounding turns. | `session_id`, `turn_uuid`, optional `window`, `memory` |
| `status` | Inspect memory and synchronization state. | optional `memory` |

Every tool returns the same stable strings the CLI prints. A tool call naming a `memory` reads it
as it stands. One naming none reads the memory the server was started on, or, started on none,
the memory this host is bound to, else the local memory. `mcp` is read-only: indexing and
publishing remain separate commands or automation installed by `funes add`.

On a server started on the host's bound memory, which is what `funes add` registers, a call naming
none reads it as it will be after this host's next push. The hooks publish at session boundaries,
so without this, a turn another local session just indexed would stay out of reach until a session
opens or ends. Such turns come back in the same reply, a session holding them is listed and read
from the local memory, which holds all of it, and the `→ get` lines name no memory, so following
them reads the same way. What counts as not pushed is what this host's push receipt for the memory
lacks, which `funes status` reports as pending.

A call naming a memory, the bound one included, reads exactly what that memory holds, and so does
a call naming none on a server started by hand on a memory this host is not bound to: your local
sessions are never mixed into a memory this host only reads.

## How it works

### What a run does

`funes add` runs the one-time bootstrap the hooks can't do unattended:

1. **Asks** before your first index. Declining aborts the add; nothing is installed. At a terminal,
   with no memory yet, it also asks whether your sessions are English or multilingual, which picks
   the memory's [embedding model](index.md#sessions-not-in-english). The default is multilingual
   when your locale (`LC_ALL`, `LC_MESSAGES`, then `LANG`) is in another language than English
   (an unset, `C` or `POSIX` locale counts as English, `C.UTF-8` too), and off a terminal funes
   takes that default without asking. When the memory it binds already has a model, the local
   memory takes it instead, and nothing is asked.
   If funes can't read that memory (offline, or your token has no access), the add stops here.
2. **Binds the host** to the memory named, or offered and taken, as `funes bind` does: every
   other installed agent's setup is re-run on it. A bare add on a bound host binds nothing.
3. **Installs the hooks and registers the MCP server** on the host's memory. This is where
   the integration converts the agent's existing sessions into its spool.
4. **Builds your first index** from that spool if you don't have one. Indexed content is ready
   to recall; [unfinished work continues on later turns](index.md#progress-and-resuming).
5. **Does the first push** to a freshly-bound memory — the publish the hook refuses to do off a
   terminal (the wrong-memory guard; see [automation.md](automation.md)). With nothing indexed yet,
   it still gives a memory with no dataset your local memory's model. A memory this host has
   published to before is left to the hooks' next push, which the guard lets through. A memory at
   a local path is not pushed to.

### The spool

funes reads no agent's own transcripts. Each integration converts a session into
`~/.funes/spool/<id>/<session id>.funes.jsonl` — at install for the history, then on every turn for
the session in progress and any other changed since the last turn — and the hook runs
`funes index --harness <id>` over what it finds there. The directory is funes's: it deletes each
file after storing its content, and a file it cannot read stays, so converter failures remain
visible. That leaves the memory as the only copy of the converted turns; rebuilding one from
scratch is `funes add <agent>` again, whose seed re-converts the agent's own history.

### Where the integration's files come from

`funes add <id>` runs an integration: a directory installed at `~/.funes/agents/<id>/` and
executed. The location is fixed rather than under `$FUNES_HOME` because an agent records the path
it is handed (pi installs the directory as an extension), so the files must outlive any one home.
funes refuses a `setup` that anyone but its owner could have written: the file and every directory
from `~/.funes/agents` down must belong to you and be neither group- nor world-writable.

The source, in order: what `--from` names; the directory `$FUNES_INTEGRATIONS` names, when set —
authoritative, consulted on every run, and nothing else is; the integrations catalog,
`hf://buckets/huggingface/funes-integrations/catalog.json`, from which funes takes the newest
release for its contract and checks the archive against the digest the catalog names. Installed
from the catalog, or found with no record of what installed it, a copy takes the catalog's newest
release on every run, fetching nothing when that is the release installed; installed from `--from`,
it runs as installed until named again; and a copy speaking a contract this funes cannot run is
refreshed whatever its source. A source that cannot be reached leaves the installed copy to run,
unless `--from` named it.
`funes remove` fetches nothing: it runs the installed copy, unless this funes cannot run it. An id
with no files anywhere is an error naming what is installed. A refresh rewrites only the files that
differ and prunes nothing; `funes remove` deletes the directory whole.

### What funes vouches for

funes vouches for files it published — a release the catalog names, whose digest matched — and runs
them without asking. Anything else — what `--from` names, a `$FUNES_INTEGRATIONS` directory, files
it has no record of installing — is confirmed at the terminal before `setup` runs, naming the
package by publisher and version and where it came from; off a terminal, funes refuses rather than
assumes, and no flag answers for you. A refused refresh leaves the installed copy as it was. An
archive's checksum proves it arrived intact, not who published it, so an archive `--from` names is
confirmed like a directory.

What `funes add` installed is recorded beside the directory, in `~/.funes/agents/<id>.json`: the
package as its manifest declared it — publisher, id, version, contract — where the files came from
— a directory, an archive, or the catalog's release, with its checksum — and each of its files with
its digest. That record is what lets a confirmed install run again unasked: before running an
installed copy, funes checks the package's files against it, and asks again only when one changed,
or when the same source hands it different files. Files it has no record of installing — a copy put
in place by hand, or by a funes from before records — are replaced by the catalog's release on the
next add or update, and asked about every time when the catalog lists no such id. The record goes
with the directory on `funes remove`. One publisher's files are never put where another's are
installed: `funes remove <id>` first says you mean to replace it.

### The integration contract

The four above are integrations like any other, and a fifth needs no change to funes and no
approval from anyone: a `manifest.json`, an executable `setup`, and whatever the integration needs
beside them. Using funes needs no integration at all — the read verbs, `funes index` on a turns
file or directory, and `funes push` work with nothing installed, and any producer that writes
[the turns format](funes-jsonl.md) is indexed the same way. An integration is the managed journey
for one agent: setup, conversion, automation, removal. What funes requires of one is
[the contract](../CONTRIBUTING.md#the-integration-contract).

## See also

- [recall.md](recall.md) — the `recall`/`get` tools your agent now has.
- [index.md](index.md) — building and updating the memory by hand.
- [push.md](push.md) — publishing a memory and sharing it.
- [automation.md](automation.md) — what `funes add` sets up and how the automation behaves.
