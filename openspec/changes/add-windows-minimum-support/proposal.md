## Why

Funes currently publishes binaries for Linux x86-64, Linux arm64, and Apple Silicon macOS, while
Windows users cannot install or run a supported native build. Issue #136 asks for Windows and
desktop support; the maintainer's stated concern is preventing regressions without a Windows
development machine. The repository already has a mostly portable Rust core, but its inference,
automation, path resolution, updater, CI, and release seams encode Unix assumptions.

A narrowly scoped native Windows CLI release, continuously verified on GitHub-hosted Windows CI,
addresses the immediate user need and the maintainer's regression concern without coupling the work
to a desktop GUI or full integration parity.

## What Changes

### 1. Define a minimum native Windows support tier

Support Windows 10/11 x86-64 with an MSVC-built `funes.exe`. Users install through a PowerShell
installer and can run the resulting executable from either PowerShell or CMD. WSL is not required.

### 2. Make the core CLI compile and preserve memory compatibility

- Add a Windows inference platform seam while retaining the pinned embedding/reranking models.
- Gate Unix-only executable-permission code.
- Verify the exact Lance, faer/multiversion, SQLite, HF Hub, ratatui/crossterm, and MCP dependency
  graph on a native Windows runner before committing to a release backend.
- Preserve the existing memory schema, model stamp, ranking pipeline, and cross-machine memory
  compatibility.

### 3. Centralize platform path behavior

Resolve the user profile, Funes home, HF cache, Codex home, and known transcript roots through one
platform-aware layer. Correctly classify Windows drive and UNC paths as local paths and detect known
harness directories by path components rather than POSIX string suffixes.

### 4. Add minimum Codex automation on Windows

Keep the existing POSIX hook scripts unchanged and add PowerShell counterparts for Windows. The
Windows scripts preserve the existing contract: drain the hook payload, detach quickly, run an
incremental index or index-then-push worker, and append diagnostics to `funes-sync.log`.

`funes add codex` and `funes remove codex` SHALL install/remove the Windows hook scripts, merge only
Funes-owned hook groups, install the skill, and register/unregister the stdio MCP server without
modifying unrelated Codex configuration.

### 5. Add Windows installation, CI, and release assets

- Add `scripts/install.ps1` with version and SHA-256 verification.
- Add Windows compile, unit, installer, Codex-integration, and real index/recall smoke coverage.
- Publish `funes-x86_64-windows.exe` and include it in `SHA256SUMS` and the Hugging Face bucket.
- Document the supported shell/runtime boundary and known limitations.

### 6. Fail safely outside the minimum tier

On Windows, `funes add claude|pi|hermes` and in-place `funes update` MAY remain outside this first
support tier, but they SHALL fail before writing partial configuration and SHALL provide an
actionable explanation. Re-running `install.ps1` is the supported minimum upgrade path.

## Capabilities

### New Capabilities

- `windows-minimum-support`: Native Windows CLI execution, platform-correct paths, Codex automation,
  verified installation, and release/CI guarantees.

### Modified Capabilities

- None. Funes does not currently maintain an OpenSpec baseline; this change introduces one bounded
  capability without redefining its platform-neutral memory contracts.

## Impact

- `Cargo.toml`, `Cargo.lock`, `build.rs` — Windows inference/dependency target configuration.
- `src/platform.rs` (new) — profile/home/cache/path and shell-command platform seams.
- `src/inference/blas.rs` — Windows backend seam or a documented Windows release-backend selection.
- `src/agents.rs`, `src/agents/hooks.rs`, `src/agents/codex.rs` — Windows-safe command rendering,
  scripts, paths, and Codex installation/removal.
- `src/traces/harness.rs`, `src/memory/dataset.rs`, `src/hub.rs` — platform-correct path handling.
- `src/commands/update.rs` — Unix gating and Windows reinstall guidance.
- `integrations/pi/index.ts` and other non-MVP integrations — explicit Windows capability guards only;
  no partial installation.
- `scripts/automation/funes-index.ps1`, `scripts/automation/funes-push.ps1` (new) — Windows automation.
- `scripts/install.ps1` and installer tests (new) — verified Windows installation.
- `.github/workflows/ci.yml`, `.github/workflows/release.yml` — Windows regression and release jobs.
- `README.md`, `CONTRIBUTING.md`, `docs/add.md`, `docs/automation.md` — support table and instructions.

## Success Criteria

- A clean `windows-latest` runner builds and tests `funes.exe` natively.
- A Windows user can install with PowerShell and invoke `funes --version` from a new PowerShell or
  CMD session.
- Explicit-path indexing and local recall complete end to end on Windows.
- `funes add codex` installs a working MCP + skill + automatic index flow without Bash or WSL.
- Windows path tests cover drive roots, UNC paths, spaces, non-ASCII names, and an unset `HOME`.
- Existing Linux and macOS tests and release assets remain unchanged and green.
