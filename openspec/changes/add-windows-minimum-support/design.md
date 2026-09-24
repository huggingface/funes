## Context

Funes has a portable domain core surrounded by several Unix-specific delivery seams. The domain
pipeline is agent transcript -> generic turn/block -> deterministic chunk -> local embedding ->
Lance memory -> hybrid recall/rerank. Platform-specific behavior is concentrated in inference,
filesystem/home discovery, hook automation, installation, update, and CI/release.

The goal is not abstract “Windows compatibility.” The goal is one explicit support contract that a
maintainer can continuously validate without owning a Windows machine: native x86-64 core CLI plus
Codex integration, installable with PowerShell and callable from PowerShell or CMD.

## Goals / Non-Goals

**Goals:**

- Build a native `x86_64-pc-windows-msvc` binary on every relevant pull request.
- Keep local indexing, local recall, MCP stdio, and remote memory formats compatible across OSes.
- Support Windows profile paths, drive paths, UNC paths, spaces, and non-ASCII components.
- Support Codex MCP, skill, and automatic indexing/publishing without Bash or WSL.
- Publish an authenticated-by-checksum Windows asset and a non-admin installer.
- Preserve all existing Linux/macOS behavior and avoid a cross-platform automation rewrite.

**Non-Goals:**

- Desktop or tray UI.
- Windows ARM64-native binaries.
- Full Claude, pi, and Hermes Windows integration in this change.
- Package-manager manifests or MSI packaging.
- A new background service or daemon.
- Any change to chunk schema, retrieval scoring, model identity, or privacy boundaries.
- In-place Windows self-update in the first release.

## Decisions

### 1. Supported runtime and shell boundary

The first supported target SHALL be Windows 10/11 x86-64 using the MSVC Rust target. PowerShell
5.1+ is the installer and hook-script runtime because it provides reliable HTTP, SHA-256, process,
and filesystem APIs on supported Windows versions. The installed `funes.exe` itself SHALL be a
normal native executable usable from both PowerShell and CMD.

CMD batch files are not the automation implementation. Batch quoting, error propagation, detached
process handling, and retry logic would create a second fragile shell program. CMD remains a valid
interactive caller of `funes.exe`.

### 2. Dependency and backend spike is an implementation gate

Before product changes, a native `windows-latest` job SHALL compile the current crate with the
candidate Windows backend. This validates upstream dependencies rather than assuming that portable
Rust APIs imply a portable dependency graph.

Preferred backend:

- reuse the existing pure-Rust faer seam currently used on Linux;
- compile faer/multiversion for `cfg(any(target_os = "linux", target_os = "windows"))`;
- keep Accelerate exclusive to macOS;
- retain the same tokenizer, safetensors, model identifiers, dimension, and model stamp.

Fallback decision:

- if the default dependency graph cannot be made to pass native Windows CI without patching an
  upstream crate, build the Windows release with the existing ONNX feature;
- the fallback is acceptable only if a real index/recall smoke test passes and a memory created on
  one backend can be opened and queried by the other under the existing compatibility rules;
- the chosen backend and reason SHALL be recorded in this design before implementation proceeds.

### 3. One platform module owns OS conventions

Introduce `src/platform.rs` for shared host conventions. HF-specific cache/token paths remain in
`hub.rs`; hook command rendering and permissions remain in `agents/hooks.rs`, following the
repository's placement rules. Both call the host layer where appropriate.

The module SHALL own:

- `user_home()` with `FUNES_HOME` remaining an explicit higher-level override;
- default Funes state directory;
- default HF cache/token path;
- path classification (`local`, Hub shorthand, `hf://` URI);
- known-path tail matching using `Path` components;
- rendering the hook command for the host platform;
- setting executable permissions as a Unix-only no-op on Windows.

Windows home resolution SHOULD use a standard Rust directory API backed by the Windows profile
known folder. If a dependency is not accepted, fallback order is `USERPROFILE`, then
`HOMEDRIVE` + `HOMEPATH`, then `HOME`; an empty result is an actionable error, never the current
working directory.

`FUNES_HOME`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `FUNES_BIN`, `HF_HOME`, and token environment
variables retain precedence.

### 4. Local path detection precedes Hub shorthand detection

Windows paths such as `C:\\work\\memory`, `C:/work/memory`, and `\\\\server\\share\\memory` must
never be parsed as `<org>/<repo>`.

Resolution order SHALL be:

1. literal `local`;
2. explicit `hf://` URI;
3. platform-recognized absolute/rooted local path or existing local path;
4. valid Hub shorthand;
5. relative local path.

Known harness detection SHALL compare path components, not rendered separators. Tests SHALL run on
Windows for `\\` paths and continue running on Unix for `/` paths.

### 5. Preserve Unix scripts; add PowerShell counterparts

The minimum-risk approach is additive:

- Unix continues to install and invoke `funes-index.sh` and `funes-push.sh` unchanged.
- Windows installs `funes-index.ps1` and `funes-push.ps1`.
- `hooks::command` renders a POSIX Bash command on Unix and a PowerShell command on Windows.
- executable permission changes are compiled and executed only on Unix.

The PowerShell scripts SHALL preserve the shell-script behavior:

- consume hook stdin so the caller does not block on an unread payload;
- start a hidden/detached worker and return within the hook timeout;
- resolve `FUNES_BIN` first, then `funes.exe` on `PATH`, then the installed executable location;
- run `index --harness <agent>` for per-turn hooks;
- at a session boundary, retry a conflicting index up to five times and then push what is stored;
- append timestamped diagnostics beside the script to `funes-sync.log`;
- treat secret-gate exit code 2 as a warning and preserve other exit codes in the log;
- quote paths and remote names without command injection.

A later change may move automation into hidden Rust subcommands, but doing so now would alter the
working Unix execution path and expand regression risk.

### 6. Codex is the only required agent integration for the first tier

Codex provides the smallest complete user journey: native agent transcripts, a native MCP client,
skills, and JSON hooks. `funes add codex` SHALL:

1. discover Codex home via `codex doctor --json`, then `CODEX_HOME`, then the platform home;
2. install the Funes skill under Codex home;
3. merge only Funes-owned hook groups into `hooks.json`;
4. install `.ps1` automation scripts on Windows;
5. register `funes mcp [memory]` using an argv-based `Command`, not by executing the rendered hint;
6. leave a manual command only as user-facing fallback text.

Removal SHALL attempt MCP unregister and owned-file cleanup independently, preserving the current
failure semantics.

Claude, pi, and Hermes installs SHALL either be proven and explicitly added to this change or return
an unsupported-on-Windows error before any write. They must not be accidentally half-enabled.

### 7. PowerShell installer is non-admin and verification-first

Add `scripts/install.ps1` with parameters equivalent to the POSIX installer:

- `-InstallDir` (default `%LOCALAPPDATA%\\Programs\\funes\\bin`);
- `-Version` (default latest);
- `-NoPathUpdate` for users who do not want persistent PATH changes.

The installer SHALL:

1. detect x86-64 Windows and reject unsupported architectures clearly;
2. download `VERSION`, `SHA256SUMS`, and the tagged asset over HTTPS;
3. require exactly one manifest entry for the Windows asset;
4. verify SHA-256 with `Get-FileHash` before executing the staged binary;
5. verify `funes.exe --version` matches the release metadata;
6. replace only the target Funes binary after all checks pass;
7. add the default install directory to the user PATH unless opted out, without duplicating entries;
8. explain that a new terminal is required for a persistent PATH update.

The installer must leave the previous binary untouched on download, checksum, or version failure.

### 8. Windows self-update is explicitly deferred

Windows does not share Unix's “rename over a running inode” behavior. The first Windows release SHALL
not pretend the existing updater is safe. `funes update` on Windows SHALL stop before download or
replacement and print the supported PowerShell reinstall command. `funes status` MAY still report
that an update exists, but its guidance must be platform-correct.

A future change can implement a verified helper process that waits for the parent to exit, retries
replacement, and preserves rollback. That behavior is deliberately not hidden inside this minimum
port.

### 9. Windows CI is the support contract

Add a `windows-latest` job that is required for relevant pull requests and main:

- install a pinned Rust toolchain and protoc;
- compile and clippy the selected Windows feature set;
- run library/unit tests with Windows-safe fake executable fixtures;
- run PowerShell installer tests against a local fixture server or mocked download layer;
- run Codex add/remove integration tests using a fake `codex.exe`;
- run one real explicit-path index -> recall smoke test with cached model files;
- assert MCP stdio emits no non-JSON logs on stdout.

Unix CI remains unchanged. OS-specific tests use `cfg` gates only when behavior is genuinely
platform-specific; domain tests remain shared.

### 10. Release asset and checksum are first-class

The release workflow SHALL build `x86_64-pc-windows-msvc`, verify its embedded version, upload
`funes-x86_64-windows.exe`, include it in the exact asset count and `SHA256SUMS`, and publish it to
both the GitHub release and the existing Hugging Face bucket. Publication remains all-or-nothing:
the publish job must require Linux, macOS, and Windows build success.

## Risks / Trade-offs

**Risk: upstream native dependencies fail on MSVC.**
Mitigation: the dependency/backend spike is task 0 and a go/no-go gate; ONNX is a bounded fallback.

**Risk: PowerShell execution policy blocks hooks.**
Mitigation: invoke `powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File`
for the current process only; document the exact command and test it on `windows-latest`.

**Risk: quoting paths through agent-owned command strings.**
Mitigation: keep the executable token unquoted (`powershell.exe`), quote only the `-File` path and
arguments with a dedicated renderer, and test spaces, quotes, ampersands, parentheses, and Unicode.

**Risk: antivirus/indexers briefly lock the installed binary.**
Mitigation: installer replacement uses a staged file and bounded retries; failure leaves the old
binary and reports the staged path.

**Risk: “Windows supported” is mistaken for full agent or GUI parity.**
Mitigation: README support matrix labels this tier `Core CLI + Codex`; unsupported integrations fail
before writes and link to follow-up issues.

## Rollout Plan

1. Land the Windows build spike and record the chosen inference backend.
2. Land platform/path abstractions with shared and Windows-specific tests.
3. Land core CLI and explicit-path index/recall smoke coverage.
4. Land PowerShell automation and Codex integration tests.
5. Land installer, release job, docs, and required CI status.
6. Publish a prerelease asset, run a clean Windows 10/11 smoke checklist, then promote support.

## Rollback Plan

- Remove the Windows asset from the release manifest and support table if the required Windows job
  cannot stay green.
- Keep all Unix paths behind existing branches so rollback does not alter Linux/macOS behavior.
- Never migrate the memory schema, so a Windows-created memory remains readable by prior supported
  platforms even if Windows distribution is paused.

## Open Questions

1. Does the exact Lance 7.0.0 graph pass MSVC without patches?
2. Does the faer/multiversion seam pass the real-model smoke test on Windows, or should the release
   use the existing ONNX feature?
3. Does the supported native Codex build execute hook command strings through CMD exactly as assumed?
4. Should the default installer directory be `%LOCALAPPDATA%\\Programs\\funes\\bin` or a project-wide
   convention preferred by the maintainers?

Questions 1-3 are resolved by task 0/CI evidence, not by assumption. Question 4 is a maintainer
choice that does not alter the runtime architecture.

## Implementation decisions from the native probe

Questions 1 and 2 are resolved: both BLAS and ONNX compiled and passed real index/recall on native
MSVC in [run 34232202952](https://github.com/wellorbetter/funes/actions/runs/34232202952). The release
keeps the default BLAS/faer backend with the existing Lance lockfile and no upstream patch.

Hook rendering uses PowerShell UTF-16LE `-EncodedCommand` containing single-quoted literals instead
of a raw `-File` command string. CMD never sees the path or memory argument, so percent expansion,
ampersands, parentheses, and apostrophes cannot change the invocation. Cleanup decodes the command
to identify Funes scripts. Native PowerShell tests verify argument preservation and detachment;
the actual Codex desktop turn remains an explicit manual release check.
