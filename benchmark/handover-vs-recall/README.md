# handover-vs-recall

Some tasks run in an agent harness require long investigations that bloat the session context, to the
point where the cost of each new turn is dominated by carrying that context rather than by the work the
turn does. Switching to a fresh session avoids that cost — but the findings of the investigation then
have to be carried into the new session somehow, and the ways of doing that differ in cost.

This benchmark measures the cost of those strategies, as **cost per successful task**, over tasks that
genuinely require the prior investigation to complete:

| arm | channel |
|-----|---------|
| **A** branch-only  | switch, carry nothing — the fresh session re-derives the result from scratch |
| **B** handoff      | switch, carry a written handoff of the investigation (produced live, or pre-written docs) |
| **C** recall       | switch, `funes` recall over the origin session's memory on demand |
| **D** same-session | don't switch — resume the origin session's full context (a fork) |

All arms start from the same base commit, get the same task, and are graded by the same hidden test; the
only difference is the channel. The task is in scope only when re-deriving is expensive: arm **A** either
fails, or succeeds only by redoing the whole investigation — at a cost higher than carrying it over. If A
re-derives the result cheaply, the investigation wasn't worth transferring and the task is out of scope.
**D** doesn't switch at all: it keeps the whole context, the cost switching is meant to avoid. The
comparison of interest is **B vs C** — summarizing the investigation versus recalling it.

## Layout: engine in git, experiments on the dataset

This directory is the **engine** — it does not grow when you add experiments:

- `run.sh` — the runner.
- `build_fork.py` — cut a pre-handoff fork from a session transcript.
- `stream_view.py` — live pretty-printer.
- `hub_io.py` — `hf`-less HF-dataset I/O in pure stdlib (exists/download/upload); run.sh's fallback when the CLI is absent.
- `pause-hooks.sh` / `unpause-hooks.sh` — pause/restore installed funes SessionEnd hooks (claude + codex) so arm sessions aren't auto-indexed mid-run.

Each **experiment is a bundle on the dataset** `dacorvo/funes-handoff-test` (a versioned HF *dataset*,
not a bucket) at `artifacts/<exp>/`, and its results at `results/<exp>/`. `run.sh` fetches the bundle
by name and runs it — so adding or editing an experiment is a dataset change, never a git commit.

## Run

Prereqs: `funes` and `claude` on `PATH`; a funes clone (`FUNES_REPO`, defaults to this repo's toplevel);
and an HF token (`HF_TOKEN` or `~/.cache/huggingface/token`) for the private dataset. The `hf` CLI is
**optional** — without it the runner falls back to `hub_io.py` (Python stdlib) for the bundle fetch, the
overwrite check, and the result sync.

Isolation, so arm sessions never land in `~/.claude/projects` and get indexed mid-run: disable the funes
plugin (`claude plugin disable funes@huggingface`) **or** set an isolated `BENCH_CLAUDE_DIR` (a throwaway
config — not authenticated, so also `export ANTHROPIC_API_KEY` or copy your `~/.claude/.credentials.json`
into it). If your indexer is a **SessionEnd hook** rather than the plugin, run `./pause-hooks.sh` first (it
covers `funes-sync.sh`/`archive-session.sh` under `~/.claude/hooks`) and `./unpause-hooks.sh` when done.

Isolate **auto-memory** too, and note it is a *separate* channel from the plugin and the hooks: Claude resolves
a session's auto-memory from the repo's **main worktree**, not from the arm worktree's cwd, so arms cut from
your working clone read that project's memory — where the experiment's own answer usually lives. Point
`FUNES_REPO` at a clone with no memory of its own, or set `BENCH_CLAUDE_DIR`. `run.sh` refuses to start when
the main worktree has a populated memory dir, and refuses to sync a receipt whose `init` event reports one.
When auditing older receipts, check `memory_paths.auto` in the arm's `init` event before trusting its score.

```bash
./run.sh <experiment> <A|B|C|D> [rep]      # e.g. ./run.sh rerank-triage C 1
```

It fetches `artifacts/<exp>/` to a local cache (`../funes-<exp>-bundle/`; delete it to refresh),
scaffolds a worktree at `BASE_COMMIT`, runs one autonomous `--print` turn, grades it, prints usage,
and syncs the receipts to `results/<exp>/<date>/<rep>/` — but only if the turn completed (a successful
result event in the stream) and the arm read no project auto-memory. A dead or contaminated run is never
published.

**Charging convention: each arm pays only to prepare its own channel.** The doc-production charge — the
cost of producing the handoff in `docs/` that B consumes — is added to arm B and no one
else. A prepares nothing; C's `funes index` and D's `build_fork.py` cost no API spend; D's channel is the
origin transcript itself. B's doc-production charge is usually the term that decides B.

Tabulate **dollars** (`total_cost_usd`), not output tokens: D replays its whole context every turn, so it
is expensive in input while cheap in output, and an output-token axis alone ranks it misleadingly well.
`CLAUDE_TIMEOUT` (default 3600s, where `timeout` exists) backstops a turn whose agent leaves a background
task deadlocked, so a batch can't stall forever.

## Authoring a new experiment (the bundle contract)

An experiment is a folder `artifacts/<exp>/` on the dataset. To add one:

**1. Find (or construct) a candidate — the selection rules.** Every arm starts from the same base commit,
so **git + the base code is free context**; the arms only transfer the *uncommitted investigation* on top.
The experiment is only meaningful when that investigation is worth transferring, decided in two stages:

- **A vs B — the gate: does transferred knowledge beat re-deriving at all?** State it as a condition on A:
  **A either fails, or passes only by re-deriving at a cost higher than B** (B = consuming the handoff
  *plus* producing it). If A re-derives the result cheaply, the knowledge wasn't worth transferring and
  the candidate is out of scope. This holds only for **trap
  knowledge**: a discovery that is **(a) un-lookup-able** — not recoverable from the base code, the task's
  sample inputs, the *dependencies' own docs/source*, or the model's textbook priors — and **(b) on the
  critical path**, so the naive approach fails the grader. Mechanically-rederivable work and documented
  gotchas fail (a): a competent fresh agent reconstructs them and A wins cheaply. *Investigation expense is
  NOT the signal* — an expensive investigation of a rederivable thing is still moot. The mechanical "naive
  fix fails at the base" check is necessary but **not sufficient**; the decisive gate is a **cheap arm-A
  pilot** — run A first, and discard the candidate unless A fails (or pays more than B) before building B/C/D.
- **B vs C — the funes claim** (only meaningful once the gate passes): recall vs an explicit handoff; recall
  wins by skipping doc-production. (**D** is the reference ceiling — full context replayed every turn,
  expected to lose to all; not a contender.)

A natural anchor is a commit whose **added test fails at its parent** (parent = base, test = grader, diff =
reference fix) **and fails for the naive fix too**, cut at the **known-but-unrecorded** boundary (the turns
hold the conclusion; the code doesn't yet), graded by a **portable, non-skipping, hidden** test.

**If no natural session fits** — the common case; clean trap-knowledge sessions are rare — *construct* one:
1. Identify the **turning point** — the design choice / discovery that is the trap knowledge. Mine it by
   **recall over the project's own memory** (`dacorvo/funes-memory`), not just `git log`: git shows *what*
   changed; recall surfaces the un-lookup-able *why* — the measured verdict, the rejected alternative, the
   dead end.
2. Pick a **clean base** *before* that knowledge was **recorded** — not just before the code shipped, but
   before the *why* was written down anywhere in-repo (RATIONALE, a doc, a docstring, even a commit message).
   funes logs its own rationale, so the answer is usually at HEAD by construction; roll the base back to the
   **parent of the commit that recorded it**. **Verify the naive approach fails at this base** — the cheap
   check a moot experiment skips.
3. **Synthesize the origin** instead of mining one: guide a fresh session from the clean base toward the
   *right direction* to rediscover the knowledge, and **stop before it implements or commits** — that
   transcript (investigation done, code uncommitted) is the boundary that feeds B/C/D via `build_fork.py`.
   Set the **direction, never the conclusion**, or B/C/D get an unfairly rich handoff and A is starved.
4. **Cut where the session is AT REST** — its last assistant turn hands back to the user ("that's the
   result, your call"), never mid-action. A fork whose tip *announces* a next step ("Let me record the
   outcome:") makes the live producer **perform that step instead of writing**: with `--tools ""` the call
   silently fails, so it fabricates tool calls and returns nothing usable. The at-rest tip is what
   separates a fork the producer can summarise from one it cannot; `funes recall` (arm C) is indifferent
   to where you cut — only B and D are sensitive to it.

**Not every turning point produces code.** A *decision* the project measured and settled (which approach to
invest in, which to reject) is graded as a **recommendation**: `task.md` is a neutral verdict form and
`grader.rs` parses the arm's answers against a hidden key — deterministic, no LLM-judge. Two rules keep it honest: **(i)** the form must be
**non-revealing** and must **not hand A the means to cheaply verify** — otherwise you do B's work for free and
discount the experiment's cost; **(ii)** mix in a **control item** whose answer is the obvious one, so the key
isn't uniform and can't be guessed. A stays in the dark by design — it is the realistic developer who never
logged the result.

**2. Build the bundle** `artifacts/<exp>/`:

| file | purpose | consumed by |
|------|---------|-------------|
| `config.sh`   | the vars below | `run.sh` |
| `task.md`     | the prose task handed to every arm | all arms |
| `grader.rs`   | completion test; compiles against the base's public API, passes iff done | grading |
| `docs/*.md`   | the frozen handoff arm B reads (produced once out of band — see below) | arm B |
| `fork.jsonl`  | pre-handoff fork (`build_fork.py <session> <cut> fork.jsonl`); its `sessionId` = `FORK_ID`. Also resumed out of band to produce the `docs/` handoff. | arm D |
| `memory/chunks.lance/` | `funes index` of the session sliced to the boundary | arm C |

`config.sh` sets — **consumed by `run.sh`:** `BASE_COMMIT`, `FORK_ID`, `HOST_REQUIRES` (space-sep binaries
to require), `MODEL`, and optional `CLAUDE_TIMEOUT`. The
grader is **always hidden**: `run.sh` drops `grader.rs` as `tests/bench_grader.rs` only at grade time and
never hands it to the arm — handing the arm its test collapses the task to "implement to this test,"
where no channel beats branch-only.
**Provenance (not consumed):** `ORIGIN_SESSION`, `ORIGIN_CUT`, and any per-arm accounting notes.

**Produce the handoff once, out of band.** Arm B reads a frozen `docs/` handoff — `run.sh` never produces
it. Author it by resuming the origin fork (`claude --resume <FORK_ID> --fork-session --tools ""`) with a
**form-blind brief**: name the investigation *topically*, never the task or the answer form, or B
transcribes instead of comprehending. Resuming is nondeterministic and cut-sensitive (see the at-rest
rule), so run it until it yields a real handoff, then **freeze that output as `docs/`** and record its
measured cost as the doc-production charge. The doc is still the origin session's own words, blind to the
task and form, so B's reps then vary only in the consumer. Reject a handoff that is short or contains
fabricated tool calls — never grade a consumer against one.

**Accounting:** tabulate tokens from `modelUsage`, never `usage.*` — the latter reports only part of a
turn and can undercount by orders of magnitude. `total_cost_usd` is derived from `modelUsage` and is sound.

**3. Upload** and run:
```bash
hf upload dacorvo/funes-handoff-test artifacts/<exp>/ artifacts/<exp>/ --repo-type dataset
./run.sh <exp> A 1
```
Secret-scan any transcript (`fork.jsonl`) before upload — `trufflehog filesystem`.

## Current experiments

Two form-based positives — a decision the project measured and settled, graded as a recommendation
against a hidden key. The numbers are the 2026-08-11 verdict forms; B's cost is consume-only, so add the
one-time doc-production charge for the B-vs-C comparison.

- **`rerank-triage`** — triage six routes to closing the ~2× rerank gap vs torch on Apple silicon, plus
  the cause. Trap = the measured inversion: candle+`accelerate` reaches AMX on a GEMM microbench yet is
  ~5× slower on the full forward (scalar libm `expf`/`erff` dominate), while a hand-written Accelerate
  forward is bit-faithful (`2.1e-5`) and 1.53× faster — every arm reasons its way to the opposite
  conclusion. Base `0eaa3f1` = parent of `d3e6618`, the first in-repo recording; origin `6282863b`@1169.

  | arm | n | pass | cost |
  |---|---|---|---|
  | A branch-only | 3 | 0/3 | $0.94 |
  | B handoff | 3 | 3/3 | $0.26 + doc |
  | **C recall** | 3 | **3/3** | **$0.64** |
  | D same-session | 3 | 3/3 | $6.15 |

  A never re-derives the inversion; B, C, D all recover it. Cost per success **C $0.64 < B ($0.26 + doc) < D $6.15**.

- **`recall-features`** — judge candidate recall-quality enhancements against the pipeline ceiling. Trap =
  the F1–F4 NO-GO finding (retrieval-side features don't move recall; the cross-encoder is the ceiling),
  measured against a labeled anchor and un-lookup-able from the code. Base `97a2ccf` = parent of the commit
  that logged the verdict in RATIONALE.

  | arm | n | pass | cost |
  |---|---|---|---|
  | A branch-only | 3 | 0/3 | $0.80 |
  | B handoff | 3 | 3/3 | $0.49 + doc |
  | **C recall** | 3 | **3/3** | **$0.84** |
  | D same-session | 3 | 3/3 | $3.25 |

  A never re-derives the verdict; B, C, D all recover it. Cost per success **C $0.84 < B ($0.49 + doc) < D $3.25** — same shape as `rerank-triage`.
