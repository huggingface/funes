# handover-vs-recall

Measures the incremental cost of getting a task's prior *investigation* into a fresh session through
different channels, as **cost per successful task**:

| arm | channel |
|-----|---------|
| **A** branch-only  | nothing — re-derives the result from scratch |
| **B** handoff      | a written handoff (produced live, or pre-written design docs) |
| **C** recall       | `funes` recall over the origin session's memory |
| **D** same-session | resume the origin session's full context (a fork) |

All arms start from the same base commit, get the same task, and are graded by the same test; the only
difference is the channel. Recall "wins" when it reaches the fix for less than the alternatives.

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

```bash
./run.sh <experiment> <A|B|C|D> [rep]      # e.g. ./run.sh hf-cache-recall C 1
```

It fetches `artifacts/<exp>/` to a local cache (`../funes-<exp>-bundle/`; delete it to refresh),
scaffolds a worktree at `BASE_COMMIT`, runs one autonomous `--print` turn, grades it, prints usage,
and syncs the receipts to `results/<exp>/<date>/<rep>/` — but only if the turn completed, decided by a
successful result event in the stream (a dead run is never published). Arm B/D also carry a
doc-production charge, added at tabulation. `CLAUDE_TIMEOUT` (default 3600s, where `timeout` exists)
backstops a turn whose agent leaves a background task deadlocked, so a batch can't stall forever.

## Authoring a new experiment (the bundle contract)

An experiment is a folder `artifacts/<exp>/` on the dataset. To add one:

**1. Find (or construct) a candidate — the selection rules.** Every arm starts from the same base commit,
so **git + the base code is free context**; the arms only transfer the *uncommitted investigation* on top.
The experiment is only meaningful when that investigation is worth transferring, decided in two stages:

- **A vs B — the gate: does transferred knowledge beat re-deriving at all?** State it as a condition on A:
  **A is expected to fail.** If A instead *passes*, the experiment clears only when **A cost more than B**
  (B = consuming the handoff *plus* producing it) — otherwise the fresh agent re-derived the result for less
  than it took to transfer, and the knowledge wasn't worth transferring. This holds only for **trap
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

**Not every turning point produces code.** A *decision* the project measured and settled (which approach to
invest in, which to reject) is graded as a **recommendation**: `task.md` is a neutral verdict form and
`grader.rs` parses the arm's answers (`RECOMMENDATION.md`) against a hidden key — deterministic, no LLM-judge
(`ARM_B_MODE=prewritten-docs`, the handoff is `docs/`). Two rules keep it honest: **(i)** the form must be
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
| `docs/*.md`   | design notes — **only** if `ARM_B_MODE=prewritten-docs` | arm B |
| `fork.jsonl`  | pre-handoff fork (`build_fork.py <session> <cut> fork.jsonl`); its `sessionId` = `FORK_ID` | arms B(live)/D |
| `memory/chunks.lance/` | `funes index` of the session sliced to the boundary | arm C |

`config.sh` sets — **consumed by `run.sh`:** `BASE_COMMIT`, `FORK_ID`, `GRADER_TEST` (the `cargo test
--test` name), `HANDS_TEST` (`yes` hands the arm the grader / `no` hides it), `ARM_B_MODE`
(`live-producer` | `prewritten-docs`), `HOST_REQUIRES` (space-sep binaries to require), `MODEL`.
**Provenance (not consumed):** `ORIGIN_SESSION`, `ORIGIN_CUT`, and any per-arm accounting notes.

**3. Upload** and run:
```bash
hf upload dacorvo/funes-handoff-test artifacts/<exp>/ artifacts/<exp>/ --repo-type dataset
./run.sh <exp> A 1
```
Secret-scan any transcript (`fork.jsonl`) before upload — `trufflehog filesystem`.

## Current experiments

**Positives (recall wins):**
- **`hf-cache-recall`** — flagship: make warm remote (`hf://`) recall read through the hf-hub cache.
  Recall lands the fix at ~half the cost of re-deriving. Trap = an *undocumented* lance quirk (warm
  recall reads 0 bytes off symlinked manifests) — a discovery that's also a crisp correctness bug.
- **`recall-features`** — recommendation shape: judge six candidate recall-quality enhancements + the
  pipeline ceiling. Trap = the F1–F4 NO-GO finding (retrieval-side features don't move recall; the
  cross-encoder is the ceiling), measured against a labeled anchor and un-lookup-able from the code.
  A-vs-B gate passed **A 2/7 fail, B 7/7 pass**. Base `97a2ccf` = parent of the commit that logged the
  verdict in RATIONALE.

**Negative controls (recall can't win — the boundary):** each was re-derived cheaply because the answer
was *lookup-able*, so A wins and the gate fails. They bound the flagship's claim.
- **`codex-parser`** — Codex rollout format is in fetchable public sample transcripts.
- **`prefilter`** — the fix (`lance prefilter=false` post-filters) is in lance's docstring + a textbook pattern.
- **`store-lock`** — the grader was handed to the arm (`HANDS_TEST=yes`), trivializing re-derivation.
