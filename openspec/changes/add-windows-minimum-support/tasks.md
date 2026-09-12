## 0. Dependency and Backend Gate

- [x] 0.1 Add a temporary `windows-latest` build job for `x86_64-pc-windows-msvc` using the current
  dependency lockfile and pinned Rust/protoc versions
- [x] 0.2 Record every native compile blocker from Lance, faer, multiversion, SQLite, hf-hub,
  ratatui/crossterm, rmcp, and transitive native crates
- [x] 0.3 Prototype the default BLAS build by reusing the faer platform seam on Windows
- [x] 0.4 If the BLAS build requires an upstream patch, test the existing ONNX-only feature set and
  cross-backend memory compatibility instead
- [x] 0.5 Update `design.md` with the selected Windows release backend and evidence; stop the change
  if neither candidate passes a real native index/recall smoke test

## 1. Platform Abstraction

- [x] 1.1 Add `src/platform.rs` for profile/home/cache discovery, path classification, known-tail
  matching, hook command rendering, and executable-permission handling
- [x] 1.2 Preserve `FUNES_HOME`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `FUNES_BIN`, `HF_HOME`, and token
  override precedence
- [ ] 1.3 Replace direct `HOME` lookups in memory, Hub token/cache, inference cache, trace discovery,
  and agent integration paths
- [x] 1.4 Replace string separator matching in `Harness::from_known_dir` with component matching
- [x] 1.5 Make local-path classification recognize Windows drive and UNC paths before Hub shorthand
- [x] 1.6 Add tests for unset `HOME`, overrides, drive paths, UNC paths, spaces, Unicode, and Unix
  non-regression

## 2. Native Windows Compilation

- [x] 2.1 Implement the selected Windows inference backend without changing model IDs, dimension, or
  memory schema/model stamp
- [x] 2.2 Move/gate target-specific dependencies in `Cargo.toml` for MSVC compilation
- [x] 2.3 Gate `PermissionsExt`, chmod behavior, `ExecutableFileBusy`, and Unix-only updater tests
- [x] 2.4 Add Windows-safe fake executable/test helpers (`.cmd` or fixture executables as appropriate)
- [x] 2.5 Run `cargo clippy --all-targets --profile ci -- -D warnings` and unit tests on Windows

## 3. Core CLI and Memory Validation

- [x] 3.1 Verify `--version`, `status`, explicit-path `index`, `recall`, `get`, `sessions`, `sketch`,
  `scan`, `scrub`, `push`, `ask codex`, and `mcp` on native Windows
- [x] 3.2 Add a Windows fixture with a drive-rooted Codex transcript and PowerShell tool blocks
- [x] 3.3 Add an end-to-end explicit-path index -> recall test under a path containing spaces and
  non-ASCII characters
- [ ] 3.4 Verify a Windows-created memory opens on Linux and a Linux-created memory opens on Windows
- [x] 3.5 Verify MCP stdio keeps diagnostics off stdout
- [x] 3.6 Verify Hub read/push and trufflehog discovery on Windows; document/install the required
  trufflehog executable if the push path is enabled in the minimum tier

## 4. Windows PowerShell Automation

- [x] 4.1 Add `scripts/automation/funes-index.ps1` matching the current index script's stdin,
  detachment, binary-resolution, logging, and exit behavior
- [x] 4.2 Add `scripts/automation/funes-push.ps1` matching index-before-push, bounded retry, secret-gate,
  logging, and detachment behavior
- [x] 4.3 Make `hooks::write_scripts` select `.sh` on Unix and `.ps1` on Windows while preserving Unix
  file content and executable bits
- [x] 4.4 Add a Windows hook-command renderer using process-scoped PowerShell execution-policy bypass
- [x] 4.5 Add injection/quoting tests for spaces, Unicode, quotes, ampersands, and parentheses
- [x] 4.6 Add timing tests proving the foreground hook returns before the 15-second timeout while the
  worker completes independently

## 5. Windows Codex Integration

- [x] 5.1 Make Codex home fallback use the platform profile after `codex doctor --json` and
  `CODEX_HOME`
- [x] 5.2 Install Windows PowerShell hooks and keep the existing Funes-only JSON merge semantics
- [x] 5.3 Verify `codex mcp add/remove` uses argv-based process spawning and supports `funes.exe` paths
- [x] 5.4 Port add/remove integration tests to `windows-latest` with a fake `codex.exe`
- [x] 5.5 Test local and bound-memory installs, repeat-run idempotency, malformed config safety, and
  independent cleanup failures
- [x] 5.6 Add pre-write Windows capability guards for Claude, pi, and Hermes until separate changes
  declare those integrations supported

## 6. Windows Installer and Upgrade Guidance

- [x] 6.1 Add `scripts/install.ps1` with `-InstallDir`, `-Version`, and `-NoPathUpdate`
- [x] 6.2 Implement HTTPS metadata/asset download, strict manifest parsing, SHA-256 verification, and
  staged `--version` verification
- [x] 6.3 Implement failure-safe binary replacement with bounded retries and no change on verification
  failure
- [x] 6.4 Add idempotent user-PATH update for the default install and new-terminal guidance
- [x] 6.5 Add hermetic installer tests for success, pinned version, checksum mismatch, version mismatch,
  unsupported architecture, existing install, and paths with spaces
- [x] 6.6 Make `funes update` and update notices print platform-correct PowerShell reinstall guidance on
  Windows without attempting live-executable replacement

## 7. CI and Release

- [ ] 7.1 Promote the Windows job from the dependency spike to a required PR/main regression job
- [ ] 7.2 Cache Windows model files without sharing incompatible cache keys with Linux/macOS
- [x] 7.3 Add a release build for `x86_64-pc-windows-msvc` and verify the embedded version
- [x] 7.4 Package the asset as `funes-x86_64-windows.exe`
- [x] 7.5 Include the Windows asset in exact asset-count validation and `SHA256SUMS`
- [x] 7.6 Require Windows build success before GitHub/HF-bucket publication and upload the asset to both

## 8. Documentation

- [ ] 8.1 Add `Windows x86_64 — Core CLI + Codex` to the README support table
- [x] 8.2 Document the PowerShell install command and CMD/PowerShell runtime usage
- [x] 8.3 Document Windows profile/cache paths, overrides, trufflehog requirement, and log location
- [x] 8.4 Document excluded agent integrations, PowerShell execution-policy behavior, re-install update
  flow, and WSL distinction
- [x] 8.5 Update contributor instructions with the Windows toolchain, protoc, and targeted test commands
- [ ] 8.6 Link issue #136 and open follow-up issues for Windows self-update, additional agents, ARM64,
  and desktop GUI

## 9. Final Verification

- [x] 9.1 Run the full Linux unit/integration and installer suite unchanged
- [ ] 9.2 Run the macOS release build and existing automation integration tests unchanged
- [ ] 9.3 Run the complete required Windows job twice to verify cache-independent repeatability
- [ ] 9.4 Perform a clean Windows 11 smoke test: install -> CMD version -> index -> recall -> add Codex ->
  complete a Codex turn -> verify background index -> remove Codex
- [ ] 9.5 Perform a Windows 10 smoke test or document the exact CI/VM evidence used to claim support
- [ ] 9.6 Confirm every requirement scenario has an automated test or an explicitly named manual check
- [x] 9.7 Run `openspec validate add-windows-minimum-support --strict` after OpenSpec initialization
