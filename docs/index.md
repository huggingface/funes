# Building the memory

`funes index` builds or updates your local memory from session transcripts. [`funes add`](add.md)
runs it for you on every turn; run it by hand to seed a memory, to fold in a new source, or to keep
recall fresh for an agent with no hooks (pi).

```bash
funes index      # a fast, text-first pass over every known harness dir, into one local memory
```

## What it indexes

With **no argument**, in a terminal, `funes index` sweeps every supported agent's session dir it
finds — `~/.claude/projects`, `~/.codex/sessions`, `~/.pi/agent/sessions`, `~/.hermes/state.db` — into
one memory, then offers to finish any deeper work left. Scope it to a single agent with `--harness`:

```bash
funes index --harness codex        # only ~/.codex/sessions
```

Point it at a **path** to index one place in full — a transcript tree or a single `.parquet` trace
export — or at a **Hub trace repo** to index its auto-converted parquet:

```bash
funes index ./some/session/tree            # a local transcript tree or .parquet
funes index <org>/<repo>                   # a Hub trace dataset (or a full hf://… URI)
```

An existing local path always wins over reading the same string as a repo ref. An **automated
(non-terminal) run must name a target** — a path or `--harness <name>`; funes refuses to sweep every
harness root unattended (a Claude session-end shouldn't pull in Codex or pi sessions).

## Incremental by construction

A chunk's id derives from `(session, turn, block, split)`, so a completed turn produces **exactly the
same chunks** no matter when it's indexed — and re-running embeds nothing already written. That is
what makes it cheap to re-run as you work, and what lets the per-turn hook do the same job as one
sweep at the end.

A no-path refresh is **budgeted and text-first**: it does a fast text pass and offers to backfill the
deeper content, so a large backlog fills in a bounded step at a time rather than one long stall. An
explicit path or Hub repo is indexed in full.

## Tiers and ordering

Blocks are indexed in three tiers, cheapest-and-highest-value first:

| Tier | Blocks | Why first |
| --- | --- | --- |
| L1 `text` | user and assistant prose, thinking | the decisions and rationale — where recall pays off |
| L2 `tool_use` | tool calls | context for what was done |
| L3 `tool_result` | tool output | bulky, lowest value per byte |

A budgeted (no-path) run drains these **tier-major**: it indexes *every* owed session at `text`
first — newest session first, subagents last — then every session at `tool_use`, then at
`tool_result`, checking a ~60s wall-clock budget at each whole-session boundary and stopping at the
first one past it. So the whole memory becomes recallable at the decision/rationale level within
about a minute, and the bulky tool output backfills on later runs (the per-turn hook, or a rerun) a
bounded step at a time. `--no-thinking` drops thinking blocks from the `text` tier; an explicit path
or Hub repo skips the budget and indexes all tiers in one pass.

## Flags

| Flag | Meaning |
| --- | --- |
| `--harness <name>` | Override auto-detection for a path, or (with no path) target one harness's dir: `claude \| codex \| pi \| hermes`. |
| `--limit <N>` | Index only the most recent N sessions per source. Omit to index all. A Hub repo ignores it and indexes every shard. |
| `--no-thinking` | Exclude thinking blocks. |
| `--yes` | Don't ask: a budgeted (no-path) run finishes all remaining work; an explicit path skips the first-index size confirmation. |

## The pipeline

Indexing and recall are one deterministic pipeline:

```
~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions, ~/.hermes/state.db   (or a .parquet trace)
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
