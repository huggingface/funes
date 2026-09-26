# Adding and removing funes

`funes add <agent> [memory]` wires funes into a coding agent in one command. Every agent gets the
funes read tools plus instructions on when to use them, and the memory itself — building your first
index, installing the automation that keeps it current, and (with a memory bound) doing the first
publish. Nothing is left to run by hand.

```bash
funes add claude                           # local
funes add claude <user|org>/funes-memory   # …backed by a memory you own (sync across machines/team)
```

`funes remove <agent>` reverses that integration:

```bash
funes remove claude                       # or codex, pi, hermes
```

It unregisters funes's tools, removes its automation hooks and owned integration files, and
preserves unrelated agent hooks and configuration. It does **not** delete your local memory,
original session transcripts, model/Hub caches, or any published memory. The
command is idempotent, so an already-absent integration is a successful no-op.

**Updating an install you already have: `funes add` again.** Re-running it is idempotent: it
keeps the memory bound — or rebinds to the one you name, `local` included — and re-runs the
integration's setup, which clears what an older funes put where the current one no longer looks. An integration the catalog installed is brought to its
newest release for this funes first, when there is one; one installed with `--from` stays at what
you named until you name it again — except that an install this funes cannot run, one another
funes made, is brought forward whatever its source. `funes remove` first is for a clean slate — it
takes the install away in full, your memory aside:

```bash
funes add codex                                                  # as bound, at the catalog's newest release
funes add codex <user|org>/funes-memory                          # rebound
funes remove codex && funes add codex <user|org>/funes-memory    # from a clean slate
```

funes tells you when this is due: an install its hooks no longer fit — one from before funes read
its converters' spool, or one another funes made — gets a `note:` line ahead of every read, in your
agent's tool results and on the CLI's stderr, naming the `funes add` to re-run. It goes away once
you have.

## The agents

| Agent | Read tools | Per-turn indexing | Session-boundary publish |
| --- | --- | --- | --- |
| `claude` | ✅ | ✅ (plugin hooks) | ✅ (with a memory bound) |
| `codex` | ✅ | ✅ (plugin hooks, after a `/hooks` review) | ✅ (with a memory bound) |
| `hermes` | ✅ | ✅ **beta** (plugin hooks) | ✅ (with a memory bound) |
| `pi` | ✅ | ✅ (extension events) | ✅ (with a memory bound) |

These four are maintained in [huggingface/funes-integrations](https://github.com/huggingface/funes-integrations)
and released on their own; `funes add <id>` installs the newest release for this funes. The name is
an integration id, not a fixed list: `funes add <id>` runs any integration installed under
`~/.funes/agents/`, and one the catalog does not list installs from wherever its publisher put it,
with `--from`. Integrations maintained by their authors are listed in that repository's
[COMMUNITY.md](https://github.com/huggingface/funes-integrations/blob/main/COMMUNITY.md); a listing
adds no alias — they install with `--from`, as their authors document.

Building an integration? [Writing an integration](../CONTRIBUTING.md#writing-an-integration)
walks through development, testing, publishing from your own repository, and listing it for others
to find. The [rationale](RATIONALE.md#why-integrations-live-outside-this-repository) explains why
agent integrations live outside the funes repository.

What exactly gets installed for each agent — and how the automation behaves — is in
[automation.md](automation.md).

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

Every tool returns the same stable strings the CLI prints. A
tool-call `memory` overrides the memory bound when the server started; with neither, the server reads
the local memory. `mcp` is read-only: indexing and publishing remain separate commands or automation
installed by `funes add`.

## Binding a memory

The optional positional `[memory]` is the memory this agent recalls from — and, for the agents with
publishing, publishes to:

```bash
funes add claude <user|org>/funes-memory   # recall reads it; the hooks publish there
funes add claude                           # re-run: keeps the memory bound; a first add stays local
funes add claude local                     # back to the local memory
```

A memory is an `<org>/<repo>` shorthand, a full `hf://…` URI, or `local`. The binding lives in the
**agent's own config**, and funes notes it beside the install (`~/.funes/agents/<id>.json`) so a
bare re-run keeps it — there is no hidden global default, and `local` unbinds. If you name a memory that doesn't exist on
the Hub yet, `funes add` offers to create it (default no, to catch typos). Dataset repositories that
funes creates are **private by default**; changing their visibility later is an explicit action on
the Hub. An existing repository keeps its existing visibility.

With **no memory named** and none bound yet, and an HF token present in a terminal, `funes add` offers to set up
`<user>/funes-memory` for you so your memory follows you across machines; decline and it stays local.
Without a token it stays local and tells you how to enable syncing later.

Remote publishing also requires [TruffleHog](https://github.com/trufflesecurity/trufflehog). That
is `push`'s requirement, not `add`'s: the first push `add` performs and every push the automation
runs fail closed unless funes can scan the content for credentials — agent traces commonly capture
exported environment variables. See its
[installation documentation](https://github.com/trufflesecurity/trufflehog#installation); funes
looks for it on `PATH`, or at the path set by `FUNES_TRUFFLEHOG`. A local-only setup, or an
integration that never publishes, does not need it.

## What a run does

`funes add` runs the one-time bootstrap the hooks can't do unattended:

1. **Asks** before your first index, about a minute of work. Declining aborts the add; nothing is
   installed.
2. **Installs the hooks and registers the MCP server** (baking in the bound memory). This is where
   the integration converts the agent's existing sessions into its spool.
3. **Builds your first index** from that spool if you don't have one — a fast, text-first pass.
   Deeper content and older sessions backfill on later turns.
4. **Does the first push** to a freshly-bound memory — the publish the hook refuses to do off a
   terminal (the wrong-memory guard; see [automation.md](automation.md)).

### The spool

funes reads no agent's own transcripts. Each integration converts a session into
`~/.funes/spool/<id>/<session id>.funes.jsonl` — at install for the history, then on every turn for
the session in progress and any other changed since the last turn — and funes indexes what it finds
there. The id names the spool; the
`harness` each turn carries is the integration's own to choose (the maintained four use their id,
by convention), and `recall --harness` filters on what the turns carry, whatever is installed.

The directory is funes's. A bundle only ever writes into it, and funes deletes a file once the whole
of it is in the memory, so what is left on disk is exactly the backlog still owed: the whole history
right after the seed, nothing once the per-turn drip has caught up. A file funes could not read
stays, so a broken converter is visible rather than silent.

That leaves the memory as the only copy of the converted turns. Rebuilding one from scratch is
`funes add <agent>` again: its seed re-converts the agent's own history.

What a producer owes the spool: write, never read back — funes may have drained what you wrote, so
keep your own record if you need one; one file per session, named from the session id, so a re-emit
overwrites its own file; write a temporary name in the same directory and rename it into place, with
the temporary name off `.jsonl` (see [funes-jsonl.md](funes-jsonl.md)). The hook then runs
`funes index --harness <id>`, one budgeted step over the spool.

Re-run `funes add <agent> <memory>` any time to change the memory or refresh the setup — it's
idempotent. On a new host, re-running it once clears the wrong-memory guard for that machine, as
soon as that host has an index of its own to push.
Run `funes remove <agent>` to reverse the agent wiring without deleting the memory it used.

From here you just work: when something touches a past decision, its rationale, or an earlier
finding, the agent reaches for [`recall`](recall.md) itself.

## The integration contract

`funes add <id>` runs an *integration*: a directory funes installs at `~/.funes/agents/<id>/` and
executes. The four above are integrations like any other, and a fifth needs no change to funes and
no approval from anyone. Using funes needs no integration at all: the CLI and MCP read verbs,
`funes index` on a turns file or directory, and `funes push` work with nothing installed, and any
producer that writes [the turns format](funes-jsonl.md) is indexed the same way. An integration is
the managed journey — setup, conversion, automation, removal — for one agent.

### The bundle

| File | Role |
| --- | --- |
| `manifest.json` | What the integration declares — the fields below. |
| `setup` | An executable. `setup add [MEMORY]` wires funes into the agent, bound to `MEMORY` when given (an `<org>/<repo>` shorthand or an `hf://…` URI; absent is the local memory); `setup remove` undoes it. A non-zero exit fails the command. |

| Field | |
| --- | --- |
| `contract_version` | The contract this section describes, `1`. funes refuses a mismatch before running anything, pointing at `funes update`. |
| `id` | The directory name, lowercase `[a-z0-9_-]`; it names the installation and its spool. |
| `label` | The agent's name as a human writes it. |
| `repo` | Where it is published from, `<publisher>/<name>`. The publisher and the `id` are what the package is: funes will not put one publisher's files where another's are installed — `funes remove <id>` first says you mean to replace it. |
| `version` | Its own release, `MAJOR.MINOR.PATCH`, moving independently of the contract. Optional. |

`setup` runs with three variables: `FUNES_BIN`, the funes command to record or invoke (the user's
pin, else `funes`); `FUNES_HOME`, the home whose memory and spool this install serves; and
`FUNES_AGENT_ID`, its own id, which names its spool. Everything else the bundle needs — converter,
hook scripts, plugin files — sits beside `setup`, and so does any state it keeps: a refresh rewrites
only the files that changed and prunes nothing, while `funes remove` deletes the directory whole.

The location is fixed rather than under `$FUNES_HOME` because an agent records the path it is
handed (pi installs the directory as an extension), so the files must outlive any one home. funes
refuses a `setup` that anyone but its owner could have written: the file and every directory from
`~/.funes/agents` down must belong to you and be neither group- nor world-writable.

### Where the files come from

`funes add <id>` installs the integration when none is installed. Installed from the catalog, it
takes the catalog's newest release for this funes on every run, fetching nothing when that is the
release installed; installed from `--from`, it runs as installed until named again; and a copy
speaking a contract this funes cannot run is refreshed regardless. The source, in order: what `--from` names
— a directory holding the integration, or an `hf://buckets/<owner>/<bucket>/<path>/<id>.tar.gz`
archive published with a `SHA256SUMS` beside it; the directory `$FUNES_INTEGRATIONS` names, when
set — authoritative, consulted on every run, and nothing else is; the integrations catalog —
`hf://buckets/huggingface/funes-integrations/catalog.json`, naming each maintained integration's
releases — from which funes takes the newest for its contract
and checks the archive against the digest the catalog names. A source that cannot be reached
leaves the installed copy to run. `funes remove` fetches nothing: it runs the installed copy, unless this
funes cannot run it. An id with no files anywhere is an error naming what is installed.

funes vouches for files it published — a release the catalog names, whose digest matched — and runs
them without asking. Anything else — what `--from` names, a
`$FUNES_INTEGRATIONS` directory, files it has no record of installing — is confirmed at the
terminal before `setup` runs, naming the package by publisher and version and where it came from;
off a terminal, funes refuses rather than assumes. An archive's checksum proves it arrived intact,
not who published it, so an archive `--from` names is confirmed like a directory.

What `funes add` installed is recorded beside the directory, in `~/.funes/agents/<id>.json`: the
package as its manifest declared it — publisher, id, version, contract — where the files came
from — a directory, an archive, or the catalog's release, with its checksum — and each of its
files with its digest. That record is what lets a confirmed install run again unasked: before running an
installed copy, funes checks the package's files against it, and asks again only when one changed,
or when the same source hands it different files. Files it has no record of installing — a copy put
in place by hand — are asked about every time. The record goes with the directory on `funes remove`.

So a fifth integration installs from wherever its publisher put it: `funes add <id> --from <dir>`
for a directory holding it, `funes add <id> --from hf://buckets/<owner>/<bucket>/<path>/<id>.tar.gz`
for a published archive — confirmed once, and remembered until its files change. Installing one by
name alone is for what the catalog lists: the maintained four.

### What the managed `add` assumes

The maintained four register `$FUNES_BIN mcp [MEMORY]` as the agent's MCP server, convert the
agent's history into the spool at install, and install automation that converts each finished
session and runs `funes index --harness <id>`, plus a session-boundary `funes push` when a memory
is bound. None of that is required of an integration — one may register the read tools alone — and
funes does not ask which kind it is running: the bootstrap around `setup` is the same for every
integration, and each step is a no-op when there is nothing for it. A first add asks before
indexing, the seed indexes what the spool holds, and the first push publishes what was indexed; an
integration that converts nothing gets a note at each step and nothing else.

### Converting by hand

funes reads no agent's own transcripts, so indexing an agent's history on a machine that never ran
the automation — an archive, a colleague's export — is two steps: the bundle's converter into a
directory of your own, then `funes index` on that directory. A directory you own keeps its files;
only the spool is drained. The converter is each bundle's own, not part of the contract, so its
usage is listed here for the maintained four (`<bundle>` is `~/.funes/agents/<id>` once `funes add
<id>` has run on any machine, or `<id>/` in a checkout of huggingface/funes-integrations):

| Agent | Converter |
| --- | --- |
| `claude` | `<bundle>/claude-plugin/funes/convert <transcript.jsonl> [out.funes.jsonl]` — needs jq; one argument prints to stdout |
| `codex` | `<bundle>/codex-plugin/plugins/funes/convert <rollout.jsonl> [out.funes.jsonl]` — needs jq; one argument prints to stdout |
| `pi` | `node <bundle>/convert.mjs <session.jsonl \| sessions-root> <out-dir>` — a root converts every session under it |
| `hermes` | `python3 <bundle>/plugin/convert.py <state.db> <out-dir> [session-id]` — every session in the store unless one is named |

```bash
for t in /archive/.claude/projects/*/*.jsonl; do
    ~/.funes/agents/claude/claude-plugin/funes/convert "$t" ~/imports/"$(basename "$t" .jsonl)".funes.jsonl
done
funes index ~/imports
```

A converter emits the same ids the hook does, so a session imported this way and one captured live
are one session in the memory, deduplicated on re-index.

## See also

- [recall.md](recall.md) — the `recall`/`get` tools your agent now has.
- [index.md](index.md) — building and updating the memory by hand.
- [push.md](push.md) — publishing a memory and sharing it.
- [automation.md](automation.md) — exactly what the hooks install and how they behave.
- [configuration.md](configuration.md) — installed paths, authentication, caches, and environment
  overrides.
