# Windows acceptance — 2026-09-12

The native Windows 11 CLI/Codex and authenticated Hub journeys have been exercised on
Windows 11 build 26200 with Rust 1.95.0 MSVC and Codex 0.153.4. Release qualification remains
open for clean Windows 10/11 installations and cross-OS memory exchange.

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
- `cargo test --locked --profile ci --target x86_64-pc-windows-msvc` passed, including 257 library
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

## Remaining release checks

- Clean Windows 10/11 installation and standard-user CMD/PowerShell journeys.
- Windows-created memory queried on Linux and Linux-created memory queried on Windows.
- Installation from a real versioned release asset and matching manifest.
- Current-commit Windows/Linux CI and release build results must be checked separately; older
  successful runs are not proof for the repaired source.

No Windows release has been published. The earlier CI evidence is in
[the Windows run](https://github.com/wellorbetter/funes/actions/runs/34239759269),
[Linux CI](https://github.com/wellorbetter/funes/actions/runs/34239759267), and
[release builds](https://github.com/wellorbetter/funes/actions/runs/34239759213).
