# Publishing and sharing

Your local store is a [Lance](https://lancedb.github.io/lance/) dataset, and it shares the way a
dataset does: publish it to a Hugging Face **dataset repo you own** and any machine, teammate — or
anyone, if you make it public — recalls from it with one flag. The Hub is a tier you opt into; you
never need it to use funes locally.

```bash
funes push <user|org>/funes-memory                   # publish your local store's new chunks there
funes recall "..." --store <user|org>/funes-memory   # read it back from anywhere (see recall.md)
```

## `funes push`

`funes push <store>` uploads the chunks your local store has that the remote doesn't. The store is an
`<org>/<repo>` shorthand or a full `hf://…` URI.

On its **first publish**, push also writes the repo's dataset card — what a funes store is, how to
recall from it, live stats — tagged [`funes`](https://huggingface.co/datasets?other=funes) so every
shared store is recognizable and discoverable on the Hub. Later pushes keep the stats fresh. A card
you wrote yourself is never touched.

| Flag | Meaning |
| --- | --- |
| `-y`, `--yes` | Skip the wrong-store confirmation (below). |
| `--force-reindex` | Refresh the remote index after pushing even if the backlog is below the auto-reindex threshold; with nothing new to push, reindex only. |

**The wrong-store guard.** A first push to a store your local store shares no chunks with — a first
push, a new host, or genuinely the wrong store — asks before uploading. Off a terminal it refuses
rather than guess; `--yes` overrides. ([`funes add`](add.md) clears this for you by doing the first
push interactively.)

If your token can't write the target, push says so — recall can still read a store you can't publish
to.

The hooks [`funes add`](add.md) installs run this at session boundaries automatically; see
[automation.md](automation.md).

## Keeping secrets out: the gate and `funes scrub`

funes redacts credentials from each session *before* it's stored. On push, a separate, **always-on
gate** withholds any chunk that still contains a secret and exits non-zero (code `2`), rather than
upload it:

```console
$ funes push <user|org>/funes-memory
scanning 512 chunk(s) for secrets…
hf://datasets/<user|org>/funes-memory: nothing published — held back 3 row(s) with secrets (AWS×2, PrivateKey×1); run `funes scrub`, then push again
$ echo $?
2
```

`funes scrub` redacts secrets from your **local** store in place — for rows indexed before redaction
existed, or flagged by an updated ruleset. It needs no source transcript. Run it, then push again.

Scrub cleans the local store only; it does **not** scrub an already-published remote — the gate can
only stop *adding* secrets to one. If a secret was published before, remove it from the Hub the way
you would any dataset row.

## Project memories: `funes curate`

A project memory is a store that ships only the sessions you've reviewed and marked `include` — think
of it as a `CLAUDE.md`, but the entire searchable history of *why the project is the way it is*
instead of a page someone maintains.

```bash
funes curate <store> huggingface/funes    # name the store the project memory of a repo, then review
funes curate <store>                       # review again later
```

The project must be a **repo identity** (`owner/name`) — funes attributes sessions to it by their
checkout's git remotes. In a terminal, `curate` opens an interactive review of the candidate
sessions: `→` includes a session, `←` excludes it, and the preview shows each session's prompts.
Your review alone decides what the next `funes push` ships there; leaving the review offers that push.

For scripts, decide non-interactively:

```bash
funes curate <store> --include <session> --exclude <session>
```

## Inspecting a store: `status`

`funes status` takes an optional store (an `<org>/<repo>`, an `hf://…` URI, a local path, or
`local`); with none it acts on your local store.

```bash
funes status                 # store label, table name, chunk count (and an update check)
funes status <org>/<repo>    # …for a remote store
```

`funes status` tells you whether recall is reading your own store yet, and whether a newer funes
release is out.

## See also

- [recall.md](recall.md) — reading a shared store with `--store`.
- [automation.md](automation.md) — the session-boundary publishing the hooks run.
- [hub-caching.md](hub-caching.md) — how recall over a remote caches to local disk.
