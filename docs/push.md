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

funes redacts credentials from each session *before* it's written. On push, a separate, **always-on
gate** withholds any chunk that still contains a secret rather than upload it — the clean rows still
publish, with a warning about what was held back. Only when that leaves *nothing* to publish does the
push exit non-zero (code `2`):

```console
$ funes push <user|org>/funes-memory
scanning 512 chunk(s) for secrets…
hf://datasets/<user|org>/funes-memory: nothing published — held back 3 row(s) with secrets (AWS×2, PrivateKey×1); run `funes scrub`, then push again
$ echo $?
2
```

`funes scrub` redacts secrets from your **local** memory in place — for rows indexed before redaction
existed, or flagged by an updated ruleset. It needs no source transcript. Run it, then push again.

Scrub cleans the local memory only; it does **not** scrub an already-published remote — the gate can
only stop *adding* secrets to one. If a secret was published before, remove it from the Hub the way
you would any dataset row.

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

For scripts, decide non-interactively:

```bash
funes curate <memory> --include <session> --exclude <session>
```

## Inspecting a memory: `status`

`funes status` takes an optional memory (an `<org>/<repo>`, an `hf://…` URI, a local path, or
`local`); with none it acts on your local memory.

```bash
funes status                 # memory label, chunk/session counts, last indexed (and an update check)
funes status <org>/<repo>    # …for a remote memory
```

`funes status` tells you whether recall is reading your own memory yet, and whether a newer funes
release is out.

## See also

- [recall.md](recall.md) — reading a shared memory with `--memory`.
- [automation.md](automation.md) — the session-boundary publishing the hooks run.
- [hub-caching.md](hub-caching.md) — how recall over a remote caches to local disk.
