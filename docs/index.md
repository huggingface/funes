# Building the memory

`funes index` builds or updates your local memory from what the integrations convert and from the
turns files you point it at. [`funes add`](add.md) runs it for you on every turn; run it by hand to
catch a spool up, to finish the embeddings a budgeted run left, or to fold in a turns file, a
directory of them, a `.parquet` export, or a Hub trace repo. To index an agent's history from a
machine that never ran the automation, convert it first —
[add.md](add.md#converting-by-hand) says how.

```bash
funes index      # a budgeted pass over every spool, into one memory
```

## What it indexes

With **no argument**, in a terminal, `funes index` sweeps every spool under `~/.funes/spool/` — each
integration converts its agent's sessions into its own, `~/.funes/spool/<id>`, which is what funes
reads — into one memory, then offers to finish any work left. Scope it to one integration's
spool with `--harness <id>`:

```bash
funes index --harness codex        # only Codex's sessions
```

Point it at a **path** to index one place in full — a `.funes.jsonl` turns file (or a directory of
them), or a single `.parquet` trace export — or at a **Hub trace repo** to index its auto-converted
parquet:

```bash
funes index ./turns                        # a directory of turns files, or a .parquet
funes index thread.funes.jsonl             # turns from a source funes has no parser for
funes index <org>/<repo>                   # a Hub trace dataset (or a full hf://… URI)
```

An existing local path always wins over reading the same string as a repo ref. An **automated
(non-terminal) run must name a target** — a path or `--harness <id>`; funes refuses to sweep every
spool unattended (a Claude session-end shouldn't pull in Codex or pi sessions).

funes reads no agent's own transcripts: each integration converts its sessions into its spool, and a
path holding transcripts that are not turns files is refused with a pointer at the format
([funes-jsonl.md](funes-jsonl.md)) and at `funes add <agent>`. A session no integration has
converted is not indexed — install it, and its history is converted at install.
funes owns the spool and drains it as it goes: a file is deleted once the whole of it is in the
memory (see [add.md](add.md)), which is why a memory is rebuilt by re-running `funes add <agent>`.

Inline `data:` URI payloads are elided to `data:image/png;base64,[elided]` before a block is
scanned or stored: a pasted screenshot is megabytes of base64 with nothing recallable in it.

### Parquet trace format

Parquet import targets the Hugging Face agent-traces layout: **one row per session**, with these two
required columns:

| Column | Arrow type | Meaning |
| --- | --- | --- |
| `session_id` | `Utf8` or `LargeUtf8` | Stable identity for the session. |
| `messages` | `List<Utf8>` or `List<LargeUtf8>` | JSON-encoded, OpenAI-style chat messages in chronological order. |

Each non-null `messages` element is a JSON object. funes reads these fields:

| Field | Mapping |
| --- | --- |
| `role` | Turn role. |
| `content` | A `text` block when it is a non-empty string. |
| `reasoning_content` | A `thinking` block when it is a non-empty string. |
| `tool_calls[].function.name` | Tool name on a `tool_use` block. |
| `tool_calls[].function.arguments` | Tool input, kept as a string or serialized from its JSON value. |

The importer does not currently derive `tool_result` blocks from Parquet messages. Other message
fields are ignored. Invalid JSON elements are skipped; a row is skipped when none of its messages
produces an indexable block.

Optional string columns add provenance:

| Column | Meaning |
| --- | --- |
| `sent_at` | Timestamp applied to the imported turns. |
| `harness` | Agent/harness facet; it may differ per row. |
| `file_path` | Original source path shown in provenance. |
| `metadata` | JSON object whose `cwd` becomes the workdir facet. |

When optional provenance is absent, funes falls back to the Parquet filename where possible. Turn
UUIDs are synthesized as `<session_id>-<sequence>` and sequence counts only retained messages. This
is the compatibility contract behind “another agent can join through a Parquet trace export”; an
arbitrary Parquet table is not accepted merely because it has a `.parquet` suffix.

### funes JSONL (turns files)

A source funes has no parser for — another coding agent, an issue tracker, a chat export — reaches
it as `.funes.jsonl`: funes's own turn model, one JSON object per line, written by a *producer*
outside funes. [The format](funes-jsonl.md) is the contract — field table, identity rule,
validation. What `funes index` does with one:

- **One file is one unit**, always re-read (chunk-id dedup makes that a no-op) and never recorded in
  `state.json`. One invalid line rejects the whole file and fails the run; nothing from it is written.
  **A directory** is one unit per file: a rejected file writes nothing, is reported with its first bad
  line, the run goes on, and the summary counts it under `rejected` with a non-zero exit. A directory
  holding any other `.jsonl` is refused as ambiguous.
- **The facets come from the data.** `harness` is each turn's own, so `--harness` is refused.
  `workdir` and `repo` derive from the turn's `cwd`, resolved on the indexing machine — the one
  derivation every native parser goes through too; a turn without `cwd` has neither facet.
- **`funes index --check <file-or-dir>`** runs the same read and validation, computes the chunk ids,
  and reports turns, chunks, rejected files and the ids a file produces twice (a turn re-emitted under
  its `turn_uuid` would be deduped away, never indexed) — writing nothing. Run it before you publish a
  producer.

## Incremental by construction

A chunk's id derives from `(session, turn, block, split)`, so a completed turn produces **exactly the
same chunks** no matter when it's indexed — and re-running embeds nothing already embedded. That is
what makes it cheap to re-run as you work, and what lets the per-turn hook do the same job as one
sweep at the end.

## Progress and resuming

Without a path, `funes index` works through the backlog in bounded steps. Indexed content can be
recalled while embeddings are still pending. Later turns or another `funes index` continue the work;
`funes status` shows what remains.

In a terminal, funes may offer to finish the remaining work. You can decline and keep the progress
already made. Explicit imports have no time limit.

## Flags

| Flag | Meaning |
| --- | --- |
| `--harness <id>` | Index one integration's spool, `~/.funes/spool/<id>`. Refused with a PATH, whose turns name their own harness. |
| `--check` | Validate PATH without indexing it: turns, chunks, rejected files, duplicate ids; writes nothing, exits non-zero if a unit was rejected. |
| `--limit <N>` | Index only the most recent N sessions per source. Omit to index all. A Hub repo ignores it and indexes every shard. |
| `--no-thinking` | Exclude thinking blocks. |
| `--yes` | Don't ask: a budgeted (no-path) run finishes all remaining work; an explicit path skips the first-index size confirmation. |

## The pipeline

Indexing and recall are one deterministic pipeline:

```
~/.funes/spool/<id>      (each integration converts its agent's sessions into its own)
   (or a .parquet trace, or a .funes.jsonl turns file)
   │  parse        deterministic — turns (text / thinking / tool_use / tool_result), tagged by agent
   │  chunk        one chunk per content block, tight provenance
   │  embed        pinned local model (BAAI/bge-small-en-v1.5)
   ▼  store        a local Lance dataset (vector + BM25)
```

The embedding model is **pinned and stamped into the memory**; querying with a different one is
refused. To change it, rebuild from the transcripts — the memory is a disposable derived artifact, and
the raw text is retained in every row.

Each source is a `TraceSource` that reads its format into a generic turn/block shape; everything
downstream is source-agnostic. A memory runs ~2.3 KB/chunk and grows ~6 MB on a heavy day — see
[storage.md](storage.md).

## See also

- [recall.md](recall.md) — querying the memory you just built.
- [automation.md](automation.md) — the per-turn indexing the hooks run.
- [storage.md](storage.md) — how a memory grows on disk.
