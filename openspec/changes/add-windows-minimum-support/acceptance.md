# Windows acceptance — 2026-09-12

The native Windows 11 CLI/Codex and authenticated Hub journeys have been exercised on
Windows 11 build 26200 with Rust 1.95.0 MSVC and Codex 0.153.4. Release qualification remains
open for clean Windows 10/11 installations and installation from a versioned release asset.

## Regression fixes

- First publish now converts staged filesystem paths into slash-separated Hub keys. Before the
  fix, Windows uploaded backslash-containing filenames, recall could not find the manifest, and
  a second push repeated first publication.
- Detached PowerShell workers redirect stdin. Previously, the native child inherited a terminal
  and could wait indefinitely at the index budget prompt while holding the memory lock.
- Hook cleanup recognizes complete generated invocations and exact script basenames. References
  to the same filename, compound commands, and Bash command substitutions remain untouched.

Each fix has a regression test. Read-only independent review found an additional Bash substitution
case; it was fixed and reviewed again before final validation.

## Local verification

- `cargo fmt --check` and `git diff --check` passed.
- `cargo test --locked --profile ci --target x86_64-pc-windows-msvc` passed, including 258 library
  tests and the main/integration targets. Unix-only/prerequisite-gated cases and tests gated on the upstream HF fixture token did not
  perform remote work; the separate authenticated checks below supply that evidence.
- All-target Clippy with `-D warnings` passed for both default and `--no-default-features
  --features onnx` builds under the same locked CI profile and target.
- `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test-windows.ps1` passed.
  Its native fixture rejects nonredirected stdin, exercising both detached workers.
- The optimized default-backend executable builds and starts without a custom stack argument.
- Real Codex MCP status, a completed turn, background indexing, and retrieval of that turn passed.
  MCP initialize/list/recall and the remaining local read commands also passed.
- The actual npm Codex `.CMD` shim was exercised under space/Unicode paths. Add/repeat/remove and
  user-hook preservation passed. The ownership regression also failed on the old executable and
  passed on the repaired executable.
- A synthetic drive-rooted Codex rollout preserves PowerShell arguments, Unicode paths, CRLF
  output, and tool-call correlation. All eight Codex parser tests passed on Windows.
- The native Codex test also checks profile/state/cache resolution with `HOME` unset and explicit
  `FUNES_HOME`/`HF_HOME` overrides, using isolated directories and invented token files.

## Authenticated Hub checks

Only invented transcripts were used in a private, user-authorized dataset. In a fresh prefix:

1. First push published 6 chunks; remote recall returned the expected marker.
2. Repeated push reported all 6 chunks already present.
3. One appended transcript record produced exactly one new remote chunk; scan found its marker.
4. Status reported 7 remote chunks; forced reindex completed and recall still found the new marker.
5. Missing-scanner push failed. A separate, unused generated private-key sample was held back
   with exit code 2; the corresponding remote prefix contained no files.
6. The Hub listing contained the expected manifest and no backslash-containing keys in the new prefix.

The original malformed prefix was retained as diagnostic evidence. No real conversation history
was uploaded or bound to automatic publishing.

## Cross-OS memory exchange

[The one-off exchange run](https://github.com/wellorbetter/funes/actions/runs/34675674228)
used the Windows/Linux x86-64 artifacts from final source `94fbec1`. Each OS created a two-chunk
synthetic Codex memory; the other OS then opened it and retrieved the expected marker and session.
The producer's file list and SHA-256 hashes matched after transfer and remained unchanged after
status/recall. Both directions passed without reindexing or migration. The temporary test branch
is separate from this PR; no user history or Hub credentials were uploaded.

## Remaining release checks

- Clean Windows 10/11 installation and standard-user CMD/PowerShell journeys.
- Installation from a real versioned release asset and matching manifest.

## CI evidence

Runtime repair `ad776ee8c026037b972611d52597c591c55de0de` passed
[Windows native](https://github.com/wellorbetter/funes/actions/runs/34672802662),
[Linux unit/integration and installer tests](https://github.com/wellorbetter/funes/actions/runs/34672802666),
and [all four release builds](https://github.com/wellorbetter/funes/actions/runs/34672802674).
Both Windows jobs started without a cache. The Windows release binary passed its version check
and was uploaded as a CI artifact. No Windows release has been published.

Subsequent changes add only the regression fixtures described above and documentation. The PR
checks report their final-head results separately from this runtime-repair evidence.
Source `94fbec1` also passed [Windows native](https://github.com/wellorbetter/funes/actions/runs/34675275335),
[Linux CI](https://github.com/wellorbetter/funes/actions/runs/34675275419), and
[release builds](https://github.com/wellorbetter/funes/actions/runs/34675275355).
The Windows native job passed with its cache restored as well as without a cache in the earlier run.
