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

A hold-back is easy to miss when the hooks push in the background, so `funes status <memory>`
scans this host's pending rows the same way and reports what a push would hold back, until a
scrub lets them publish.

`funes push` and `funes scrub` refuse to run unscanned when TruffleHog is unavailable. See the
[upstream installation documentation](https://github.com/trufflesecurity/trufflehog#installation).
funes looks for it on `PATH`, or at the path set by `FUNES_TRUFFLEHOG`; see
[configuration.md](configuration.md#environment-reference).

### What `funes scrub` changes

`funes scrub` repairs the **local derived memory** in place, including sessions whose source
transcripts no longer exist. It takes the local writer lock, reconstructs and scans every stored
block, then makes one replacement commit:

- A secret whose value can be located safely is replaced with a `[REDACTED:<detector>]` marker. The
  block is re-chunked and its replacement chunks are re-embedded.
- If a finding cannot be reconstructed safely—for example, an encoded value with no reliable byte
  match—the entire block is dropped instead of risking a partial redaction.
- A redaction is kept only if the redacted block scans clean. Excising one match can expose another
  (removing one from a long base64 run re-aligns the window the next is found in), and such a block
  is dropped rather than stored.
- Clean rows retain their existing embeddings. The vector and full-text indexes are rebuilt after
  the replacement.

The source transcripts are never modified. Scrub reports how many secrets and blocks it redacted and
how many rows it had to drop. A completed scrub leaves a memory that scans clean, so the next
`funes push <memory>` has nothing left to hold back; the repaired local rows pass through the
independent egress gate.

Scrub does **not** alter an already-published remote. If a live credential reached the Hub, revoke or
rotate it first, then remediate the dataset separately; funes can prevent another upload but does not
automate remote deletion.

## Publishing a selection: `--sessions`

A push ships every local chunk the remote doesn't have. To publish a *selection* instead, name the
sessions:

```bash
funes push <memory> --sessions <session> --sessions <session>
```

Those sessions' chunks are exactly what ships. The list **is** the decision — funes keeps no record
of what you meant to publish, so a selection is made where it takes effect, and an unrecognized
session id fails the push rather than quietly publishing the rest. `funes sessions` lists the ids,
and a session is published whole: naming it publishes every chunk it holds.

The remote is append-only, so a selection is a pre-publication gate and not a remote undo. Nothing
retracts a session once it is up.

This is the surface an agent-driven curation writes into: an agent selects the qualifying sessions
against your criteria and reports them, and the person publishing passes that list.

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
