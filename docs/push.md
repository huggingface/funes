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

The picker also keeps this host's already-published project sessions available for browsing. `↑`
marks a fully published, read-only session—an append-only upload cannot be retracted by changing its
decision later. `◐` marks a published session with new local chunks; those updates remain reviewable.
The preview opens on the deterministic session sketch; press `Tab` for the prompt history.
Typing filters across both views. `Shift-Tab` shows only sessions that still require a decision.

### Guided review against one criterion

Fix one editorial criterion before opening the picker by giving it a short label and a text file:

```bash
funes curate <memory> \
  --exclude-criterion internal=./exclude-internal.txt
```

The complete criterion is shown above every preview and cannot be edited while the picker is open.
It is saved locally for that memory and reused on later reviews; replace it with another
`--criterion` or `--exclude-criterion`, or remove it with `--clear-criterion`. Use `--criterion` for
a condition that supports inclusion when matched, and `--exclude-criterion` for a condition that
supports exclusion when matched.

Claude can assess the selected **session sketch** against that fixed criterion on demand:

```bash
funes curate <memory> \
  --exclude-criterion internal=./exclude-internal.txt \
  --assist claude
```

funes asks once before enabling the provider. Nothing is sent merely by opening or browsing the
picker; press `F2` on a session to send that sketch and the criterion. The assessment runs while you
continue browsing, is checked against funes's schema and evidence handles, and is cached locally.
Fresh cached results appear on later runs even without `--assist`. Change the model with
`--assist-model`; the default is `opus`, with a per-assessment budget ceiling of $1.25.

The row badges summarize the advisory result: `+` include candidate, `!` exclude candidate, `?`
full review needed, `◇` not assessed, and `×` runner or validation failure. The preview shows the
rationale, measured time and reported cost, then promotes every cited source turn as **CRITERION
EVIDENCE** inside the sketch. The result never changes a human decision. In particular, an exclusion
criterion evaluated from a selected sketch may flag a session for exclusion, but it cannot clear
unseen content for publication; insufficient evidence becomes `needs full review`.

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
