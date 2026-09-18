# The funes JSONL format

`.funes.jsonl` is funes's own turn model, serialized: one JSON object per line, one turn per object,
one or many turns per file, no header. It is how a conversation reaches funes when funes has no parser
for its source — a coding agent funes does not read natively, an issue tracker, a chat export. A
*producer* (a plugin, a script, a Space) writes the file; funes indexes it through the same pipeline
as its native transcripts, so recall, `get`, `sessions` and `sketch` work on it unchanged.

```bash
funes index thread-2026-06.funes.jsonl   # one file
funes index ./exports/                    # a directory of them, recursively
funes index --check ./exports/            # validate everything, write nothing
```

A file is one unit: it is read whole and written in one append. A directory is one unit per file. A
file is *signature-less* — it is re-read on every `funes index` that names it and never recorded in
`state.json`; chunk-id dedup makes the re-read a no-op. Keep files bounded and ship updates as new
files rather than regrowing one. `--harness` is refused on both shapes: the facet is in the data.

## The turn

| field | type | required | meaning |
|---|---|---|---|
| `format` | integer | no | format version; `1` when absent. A version funes does not know is rejected. |
| `session_id` | string | yes | the conversation this turn belongs to. **One session is one thread** — a transcript, an issue, a PR. An id input; may not contain `:`. |
| `turn_uuid` | string | yes | the turn's identity: unique within its session, **stable across re-emits**. An id input; may not contain `:`. |
| `seq` | integer | yes | the turn's position in its session: a **dense counter from 0**, in the order the producer emits turns, never reassigned. Positional neighbours (`seq ± window`) and `get` read by it. |
| `ts` | string | yes | when the turn happened, RFC 3339 **in UTC, `Z` suffix** — `2026-09-18T09:41:07Z`. Recency ranking, per turn. |
| `role` | string | yes | who speaks. One dimension per producer, no default bucket. Agents: `user` / `assistant` / `tool` / `system`. A tracker might use `member` / `contributor` / `bot`. |
| `harness` | string | yes | the producer's id: lowercase letters, digits, `-` (preferred) or `_` — `opencode`, `cline`, `github`. A stored facet; `recall --harness` filters on it. |
| `cwd` | string | no | the working directory the conversation ran in, *as a path on the indexing machine*. funes derives the `workdir` and `repo` facets from it (`git -C <cwd> remote -v`). Absent → no repo facet. |
| `parent_uuid` | string | no | the turn this one replies to. Stored; not used for ranking. |
| `blocks` | array | yes | the turn's content, **complete and in order**. May be empty; an empty turn contributes no rows. |

## The block

| field | type | required | meaning |
|---|---|---|---|
| `block_type` | string | yes | `text` \| `thinking` \| `tool_use` \| `tool_result`. Anything else is rejected. |
| `text` | string | yes | the content. For `tool_use`, the call — arguments as text; for `tool_result`, the output. Markdown is fine; it is indexed as written. |
| `tool_name` | string | `tool_use`: yes; `tool_result`: no | the tool. funes never infers it: a result without a name renders without one. |
| `tool_use_id` | string | no | pairs a `tool_result` with its `tool_use`. Stored as given; funes derives nothing from it. |

Any field not listed above is rejected, on either object — funes expects its producers to be exactly
aligned on this contract, and would rather refuse a file than silently drop what it does not
understand. The funes-owned fields (`source_path`, `workdir`, `repo`, chunk ids) must not appear.

**Tool output is a `tool_result` block, whatever the turn's role.** Tiers key on `block_type`, never
on `role`: a `role: "tool"` turn whose output sits in a `text` block is indexed as text — first, not
deferred.

**A turn's rows are a function of the turn alone.** funes renders and splits each block from that
turn's own fields — never from another turn, another file, or an earlier run — so re-emitting an
identical turn always reproduces identical rows. Dedup is only safe because of that.

## Identity — what you owe the store

Every stored row is a *chunk* of one block. Its id is derived from four inputs and nothing else:

```
id = the first 16 hex characters of sha1("<session_id>:<turn_uuid>:<block_idx>:<split_idx>")
```

`block_idx` is the block's position in `blocks`; `split_idx` numbers the pieces funes cuts a long
block into. The four are joined with `:`, which is why the two string inputs may not contain one —
`a:b` + `c` would otherwise collide with `a` + `b:c`. **The text is not an input.** Everything below
follows from that.

1. **`session_id` and `turn_uuid` are yours, and must be stable.** Re-emitting a session — a rerun, a
   delta that overlaps an earlier file — must reproduce the same ids; it then costs nothing, because
   dedup drops what the store already holds. Never derive `turn_uuid` from a running counter or an
   array index that can shift.
2. **The block list must be complete.** `block_idx` counts every block — thinking, tool results, all
   of it — whether or not it ends up indexed (`--no-thinking` drops thinking blocks at chunking and
   never renumbers). Adding, dropping or reordering the blocks of a turn the store already holds
   renumbers its chunks: the old rows stay, the new ones are added, and the store has doubled that
   turn.
3. **An edit is a new turn.** Re-emitting an edited turn under the same `turn_uuid` produces the same
   ids, so the edit is deduped away and never indexed. funes is append-only: give the edit its own
   identity — `turn_uuid = "<id>@<updated_at>"` with the colons dropped from the stamp
   (`…@20260603T081240Z`), `ts = updated_at` — and the next `seq` in the session (rule 4). The earlier
   version stays; recency ranks the newer one above it. Deletions are never reflected.
4. **`seq` is a dense counter in emission order.** 0, 1, 2… with no gaps, assigned once and never
   reassigned: a delta file continues where the previous one stopped, and a turn discovered late takes
   the next number, not the one its `ts` would suggest. Position is order of appearance; `ts` carries
   time. `get` reads ranges of `seq` and neighbours are `seq ± window`, so a sparse or shifting counter
   breaks both.
5. **One session is one thread.** `get`, neighbours, `sketch` and `sessions` all assume it. A whole
   issue tracker is thousands of sessions, not one.

## Validation

A file is accepted or rejected **whole**; nothing from a rejected file is written. Rejected: a line that
is not a JSON object, an unknown field, a missing required field, a wrong type, an unknown
`block_type`, a `ts` that is not RFC 3339 UTC (`Z`), a `format` funes does not know, a `:` in
`session_id` or `turn_uuid`, a `harness` outside `[a-z0-9_-]`, a `tool_use` without a `tool_name`.

Indexing a single file, a rejection fails the run. Indexing a directory, each file stands alone: a
rejected file is reported with its first bad line, the run continues, the summary counts it under
`rejected`, and the exit status is non-zero. A directory holding any other `.jsonl` file is rejected —
the source would be ambiguous; files that are not `.jsonl` are ignored.

`funes index --check <file-or-dir>` runs the same validation and computes ids without writing:
turns, chunks, duplicate ids, and the first bad line of every rejected file. Run it before you
publish a producer.

## What funes does with your turns

Behaviour, not contract — it may evolve; the identity rule above will not.

- **Elides** inline base64 `data:` payloads to `[elided]`.
- **Redacts** secrets before chunking, best-effort; whatever slips through is caught by the fail-closed
  gate on `push`, so a leaked token never reaches a published memory.
- **Renders** each block to the text that is embedded and full-text indexed: `text` and `thinking`
  as-is; `tool_use` as `[tool_use <tool_name>] <text>`; `tool_result` as `[tool_result <tool_name>]
  <text>` (`[tool_result] <text>` without a name).
- **Splits** long rendered text into overlapping pieces; `split_idx` numbers them and `get` stitches
  them back.
- **Indexes by tier**: `text` and `thinking` first, then `tool_use`, then `tool_result`. A budgeted run
  may leave later tiers pending; `funes status` says so.
- **Stamps** `source_path` with the file's path and derives `workdir` and `repo` from `cwd`.
- **Lists** a session in `sessions` by its opening text — the first `user` text block, else the first
  text block; `sketch` uses `user` / `assistant` turns where they exist.

## Versioning

`format` is the version, per turn, `1` when absent. Changes are **additive only**: no field is ever
renamed or removed, and a new optional field arrives with a new `format` value that funes learns to
accept. A producer emitting a `format` funes does not know is rejected rather than misread, and an
unknown field is rejected rather than dropped — so a file is either understood exactly or not at all.

## Examples

An agent conversation, OpenAI-style, with the tool result in its own turn (a producer may just as well
place it inside the assistant's turn — tiers follow `block_type`):

```json
{"format":1,"session_id":"b3f2e0c4","turn_uuid":"t-0001","seq":0,"ts":"2026-09-18T09:41:07Z","role":"user","harness":"opencode","cwd":"/home/me/dev/x","blocks":[{"block_type":"text","text":"why does the build fail on arm64?"}]}
{"format":1,"session_id":"b3f2e0c4","turn_uuid":"t-0002","parent_uuid":"t-0001","seq":1,"ts":"2026-09-18T09:41:12Z","role":"assistant","harness":"opencode","cwd":"/home/me/dev/x","blocks":[{"block_type":"thinking","text":"check the target triple first"},{"block_type":"text","text":"Let me look at the CI config."},{"block_type":"tool_use","text":"{\"command\":\"cat .github/workflows/ci.yml\"}","tool_name":"bash","tool_use_id":"call_7"}]}
{"format":1,"session_id":"b3f2e0c4","turn_uuid":"t-0003","parent_uuid":"t-0002","seq":2,"ts":"2026-09-18T09:41:13Z","role":"tool","harness":"opencode","cwd":"/home/me/dev/x","blocks":[{"block_type":"tool_result","text":"runs-on: ubuntu-latest\n…","tool_name":"bash","tool_use_id":"call_7"}]}
```

A GitHub issue thread: the thread is the session, the node id is the turn identity, the author's
relationship to the project is the role, the login lives in the text where it can be searched, and an
edited comment is a new turn with the next `seq`. No colon anywhere in the ids. The `cwd` points at a
local clone so the `repo` facet resolves.

```json
{"format":1,"session_id":"gh/huggingface/transformers#31234","turn_uuid":"I_kwDOCUB6oc6M1xQz","seq":0,"ts":"2026-06-02T14:03:55Z","role":"contributor","harness":"github","cwd":"/data/checkouts/transformers","blocks":[{"block_type":"text","text":"**@someone** opened: Static cache breaks with per-layer head shapes\n\nWhen `num_key_value_heads` differs per layer …"}]}
{"format":1,"session_id":"gh/huggingface/transformers#31234","turn_uuid":"IC_kwDOCUB6oc6P0qLm@20260603T081240Z","seq":1,"ts":"2026-06-03T08:12:40Z","role":"member","harness":"github","cwd":"/data/checkouts/transformers","blocks":[{"block_type":"text","text":"**@maintainer** wrote: reproduced on main, the static cache assumes one head shape …"}]}
```

A PR's diff fits the tool vocabulary honestly — a `tool_use` block `gh pr diff 31234` followed by a
`tool_result` holding it — and lands in the last tier, indexed after the discussion.
