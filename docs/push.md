# Publishing and sharing

Your local memory is a [Lance](https://lancedb.github.io/lance/) dataset, and it shares the way a
dataset does: publish it to a Hugging Face **dataset repo you own** and any machine, teammate — or
anyone, if you make it public — recalls from it with one flag. The Hub is a tier you opt into; you
never need it to use funes locally.

```bash
funes push <user|org>/funes-memory                   # publish your local memory's new chunks there
funes recall "..." --memory <user|org>/funes-memory   # read it back from anywhere (see recall.md)
```

## `funes push`

`funes push <memory>` uploads the chunks your local memory has that the remote doesn't. The memory is an
`<org>/<repo>` shorthand or a full `hf://…` URI.

On its **first publish**, push also writes the repo's dataset card — what a funes memory is, how to
recall from it, live stats — tagged [`funes`](https://huggingface.co/datasets?other=funes) so every
shared memory is recognizable and discoverable on the Hub. Later pushes keep the stats fresh. A card
you wrote yourself is never touched.

| Flag | Meaning |
| --- | --- |
| `-y`, `--yes` | Skip the wrong-memory confirmation (below). |
| `--force-reindex` | Refresh the remote index after pushing even if the backlog is below the auto-reindex threshold; with nothing new to push, reindex only. |

**The wrong-memory guard.** A first push to a memory your local memory shares no chunks with — a first
push, a new host, or genuinely the wrong memory — asks before uploading. Off a terminal it refuses
rather than guess; `--yes` overrides. ([`funes add`](add.md) clears this for you by doing the first
push interactively.)

If your token can't write the target, push says so — recall can still read a memory you can't publish
to.

The hooks [`funes add`](add.md) installs run this at session boundaries automatically; see
[automation.md](automation.md).

## Keeping secrets out: the gate and `funes scrub`

When TruffleHog is available, indexing redacts detected credentials before storing a session. That
first pass is best-effort: local indexing still works without the scanner because the local memory
has not crossed a publication boundary. It prints a warning that index-time redaction is disabled.

Push is the hard boundary. A separate, **always-on, fail-closed gate** requires TruffleHog and scans
the rows about to leave the machine. It reconstructs complete content blocks before scanning, so a
secret split across chunks cannot evade detection. If any chunk of a block contains a secret, every
chunk of that block is held back; unrelated clean rows still publish with a warning. Only when that
leaves *nothing* to publish does push exit non-zero (code `2`):

```console
$ funes push <user|org>/funes-memory
scanning 512 chunk(s) for secrets…
hf://datasets/<user|org>/funes-memory: nothing published — held back 3 row(s) with secrets (AWS×2, PrivateKey×1); run `funes scrub`, then push again
$ echo $?
2
```

`funes push` and `funes scrub` refuse to run unscanned when TruffleHog is unavailable. Install it on
`PATH` or set `FUNES_TRUFFLEHOG=/path/to/trufflehog`; see
[configuration.md](configuration.md#environment-reference).

### What `funes scrub` changes

`funes scrub` repairs the **local derived memory** in place, including sessions whose source
transcripts no longer exist. It takes the local writer lock, reconstructs and scans every stored
block, then makes one replacement commit:

- A secret whose value can be located safely is replaced with a `[REDACTED:<detector>]` marker. The
  block is re-chunked and its replacement chunks are re-embedded.
- If a finding cannot be reconstructed safely—for example, an encoded value with no reliable byte
  match—the entire block is dropped instead of risking a partial redaction.
- Clean rows retain their existing embeddings. The vector and full-text indexes are rebuilt after
  the replacement.

The source transcripts are never modified. Scrub reports how many secrets and blocks it redacted and
how many rows it had to drop. Run `funes push <memory>` afterward; the repaired local rows then pass
through the independent egress gate.

Scrub does **not** alter an already-published remote. If a live credential reached the Hub, revoke or
rotate it first, then remediate the dataset separately; funes can prevent another upload but does not
automate remote deletion.

## Project memories: `funes curate`

A project memory is a memory that ships only the sessions you've reviewed and marked `include` — think
of it as a `CLAUDE.md`, but the entire searchable history of *why the project is the way it is*
instead of a page someone maintains.

```bash
funes curate <memory> huggingface/funes    # name the memory the project memory of a repo, then review
funes curate <memory>                       # review again later
```

The project must be a **repo identity** (`owner/name`) — funes attributes sessions to it by their
checkout's git remotes. In a terminal, `curate` opens an interactive review of the candidate
sessions: `→` includes a session, `←` excludes it, and the preview shows each session's prompts.
Your review alone decides what the next `funes push` ships there; leaving the review offers that push.

The preview is evidence for the decision, not the publication unit: **including publishes every
chunk in that session**. A session left pending stays local, as does an excluded session. Decisions
are stored per host because each host can publish only the sessions in its own local memory.

An include records how many chunks the session had when it was reviewed. If that session later
grows, its new state becomes pending again and no additional chunks ship until it is re-reviewed.
Changing an include to exclude prevents future chunks from shipping, but cannot retract chunks
already present in the append-only remote. Curation is a pre-publication gate, not a remote undo.

For scripts, decide non-interactively:

```bash
funes curate <memory> --include <session> --exclude <session>
```

The decisions are kept under `$FUNES_HOME/curation/` in a line-oriented, human-readable file; see
[configuration.md](configuration.md#the-funes-home). `funes status <memory>` reports this host's
pending review count for a project memory.

## Inspecting a memory: `status`

`funes status` takes an optional memory (an `<org>/<repo>`, an `hf://…` URI, a local path, or
`local`); with none it acts on your local memory.

```bash
funes status                 # memory label, chunk/session counts, last indexed (and an update check)
funes status <org>/<repo>    # …and what this host has or has not pushed there
```

`funes status` tells you whether recall is reading your own memory yet, and whether a newer funes
release is out. When work exists, local-index sections report how many source sessions the latest
indexing sweep left pending and the command to run; a completed sweep stays quiet. The status read
uses the sweep's small coverage snapshot rather than recursively scanning transcript trees. For a
personal remote memory, one `local push` line says either that this host is up to date or how many
local sessions are pending. This comes from a per-remote receipt kept on this host, so sessions
contributed by other hosts do not distort the result and status never scans the remote to compute
it. Run `funes push <memory>` once to initialize the receipt for an existing memory.

## See also

- [recall.md](recall.md) — reading a shared memory with `--memory`.
- [automation.md](automation.md) — the session-boundary publishing the hooks run.
- [hub-caching.md](hub-caching.md) — how recall over a remote caches to local disk.
