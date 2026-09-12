# Execution record

## Dependency gate — passed

- Upstream baseline: `huggingface/funes@1a01cc243b06a2d70f08f0da338b9bf29c275631`.
- Fork branch: `wellorbetter/funes:feature/windows-minimum-support`.
- Probe commit: `9d37f7d431fe487dcc0b241312f1df58535266c3`.
- Native run: https://github.com/wellorbetter/funes/actions/runs/34232202952
- Both MSVC BLAS and ONNX passed `cargo check --locked` and the real `index_recall` integration test.
- Selected backend: default BLAS, using the existing faer/multiversion seam. No upstream dependency
  patch, dependency version change, model change, or memory schema migration was required.

## Implementation

The follow-up adds profile/path handling, CODEX_HOME trace discovery, executable discovery, native
PowerShell hooks, Codex integration, a verification-first installer, and a Windows release build.
OpenSpec passed `openspec validate add-windows-minimum-support --strict`.

The shared platform layer owns host conventions. HF cache/token conventions stay in `hub.rs`,
and hook rendering/permissions stay in `agents/hooks.rs`, preserving the repository's layer rules.
PowerShell commands use UTF-16LE EncodedCommand instead of interpolating paths through CMD.

## Validation limitations

- Baseline Linux compilation and default-backend all-target Clippy passed before the follow-up.
- Follow-up Windows and Unix regression results are recorded as CI completes.
- Local Linux ONNX validation reached ort-sys, whose prebuilt archive download was refused by the
  local network. OpenSSL discovery was resolved with explicit system include/library paths.
- Local unit-test compilation twice produced zero-length object/archive failures in different
  Arrow/DataFusion dependencies despite adequate free disk space. The second attempt used a clean
  package cache and one build job. This is not reported as a passing test run.
- No Windows 10/11 desktop VM or authenticated HF test token was available in this workspace.
  Manual desktop installation, authenticated Hub round trips, and cross-OS memory transfer must be
  checked before release. No tag, binary release, or upstream PR is published by this work.

## Code review follow-up

- Fixed a configuration preservation bug: a group containing both Funes and user hooks lost
  all its commands during replacement/removal. Merging now removes individual owned hooks and
  preserves unrelated commands and group metadata. Regression coverage exercises plain and
  encoded PowerShell commands, removal, replacement, and repeated installation.
- Fixed malformed JSON container handling: valid JSON with a non-object `hooks` or non-array
  event could reach unchecked merging. Installation now preserves unsupported configurations
  with manual guidance; removal returns an error and retains referenced scripts. Tests cover
  invalid container shapes and byte-for-byte preservation on disk.
- Fixed install reads that treated every I/O error as a missing configuration. Only NotFound
  initializes a new configuration; other read errors propagate before script installation.
  A directory at hooks.json exercises this portably without relying on permission settings.
- Corrected the Windows integration test's mixed-separator expected path: construct each path
  component with `join` before comparing encoded commands.
- Local review validation: all-target default-backend Clippy, formatting, and diff checks pass.
  All nine tests in the actual shared hooks module pass via a small isolated harness, avoiding
  the local Arrow/DataFusion archive issue noted above. This is not a full-crate test result.
- Pre-review commit `45647bf`: Linux lint/unit and real-embedder integration jobs passed.
  Windows run https://github.com/wellorbetter/funes/actions/runs/34236706924 passed installer/
  automation tests, native compilation, real index/recall, Clippy, and 251 unit tests; it then
  failed at the mixed-separator assertion corrected above, before running windows_codex.
  The review changes require fresh Linux and Windows CI; desktop release checks remain open.
