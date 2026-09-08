## Purpose

Define the minimum supported native Windows tier for Funes: x86-64 core CLI and Codex integration,
with platform-correct paths, PowerShell installation/automation, and continuous Windows CI.

## ADDED Requirements

### Requirement: Supported Windows target
The system SHALL provide a native `x86_64-pc-windows-msvc` executable for supported Windows 10 and
Windows 11 systems without requiring WSL.

#### Scenario: Execute from PowerShell
- **WHEN** a user invokes `funes.exe --version` from PowerShell
- **THEN** the process SHALL exit successfully
- **AND** print the embedded Funes version

#### Scenario: Execute from CMD
- **WHEN** a user invokes `funes.exe --version` from CMD
- **THEN** the process SHALL exit successfully
- **AND** print the same version as PowerShell

#### Scenario: Unsupported Windows architecture
- **WHEN** the installer runs on an architecture without a published native asset
- **THEN** it SHALL stop before changing the installation
- **AND** report the supported architecture and alternatives

### Requirement: Cross-platform memory compatibility
The Windows build SHALL preserve the existing chunk schema, pinned model identity, provenance, and
memory compatibility contract.

#### Scenario: Open a memory created on another supported OS
- **WHEN** Windows Funes opens a compatible memory created by Linux or macOS Funes of the same schema
- **THEN** it SHALL read and query that memory without migration

#### Scenario: Open a Windows-created memory on another supported OS
- **WHEN** Linux or macOS Funes opens a compatible memory created on Windows
- **THEN** it SHALL read and query that memory without migration

### Requirement: Native core command flow
The Windows build SHALL support `index`, `recall`, `get`, `sessions`, `sketch`, `scan`, `status`,
`scrub`, `push`, `ask codex`, and `mcp` with the same command arguments and output contracts as
other supported OSes.

#### Scenario: Explicit path index and recall
- **WHEN** a user indexes a Windows transcript directory containing spaces and non-ASCII characters
- **AND** runs `funes recall` for text present in those transcripts
- **THEN** Funes SHALL return the matching passage with session and turn provenance

#### Scenario: MCP stdio transport
- **WHEN** an MCP client starts `funes mcp`
- **THEN** stdout SHALL contain only the MCP JSON-RPC transport
- **AND** diagnostics SHALL be written to stderr

#### Scenario: Remote-memory push
- **WHEN** a user has a valid HF token and the documented trufflehog executable is available
- **AND** runs `funes push <org>/<repo>`
- **THEN** the Windows build SHALL preserve the existing overlap and fail-closed secret gates

#### Scenario: Grounded Codex answer
- **WHEN** Codex is available on `PATH`
- **AND** a user runs `funes ask codex <question>` against a readable memory
- **THEN** Funes SHALL launch Codex and return a grounded answer under the existing command contract

### Requirement: Platform-correct user directories
The system SHALL resolve the Windows user profile and dependent Funes, Codex, agent transcript, and
HF cache paths without requiring the POSIX `HOME` variable.

#### Scenario: HOME is unset
- **WHEN** `HOME` is unset on Windows
- **AND** a valid Windows user profile is available
- **THEN** Funes SHALL resolve its default state and known agent roots under that profile
- **AND** SHALL NOT fall back to the current working directory

#### Scenario: Explicit environment override
- **WHEN** `FUNES_HOME`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `FUNES_BIN`, or `HF_HOME` is set
- **THEN** the corresponding override SHALL retain precedence over the platform default

### Requirement: Windows local path classification
The system SHALL classify Windows drive-rooted, slash-normalized drive, and UNC paths as local paths
before evaluating Hub shorthand syntax.

#### Scenario: Backslash drive path
- **WHEN** a memory or transcript path is `C:\\work\\funes memory`
- **THEN** the system SHALL treat it as a local path
- **AND** SHALL NOT interpret it as an HF Hub repository

#### Scenario: Forward-slash drive path
- **WHEN** a memory or transcript path is `C:/work/funes-memory`
- **THEN** the system SHALL treat it as a local path
- **AND** SHALL NOT interpret it as an HF Hub repository

#### Scenario: UNC path
- **WHEN** a memory or transcript path begins with `\\\\server\\share`
- **THEN** the system SHALL treat it as a local path

#### Scenario: Hub shorthand remains supported
- **WHEN** a memory argument is a valid `<org>/<repo>` shorthand and not a Windows path
- **THEN** the system SHALL resolve it to the existing HF dataset URI form

### Requirement: Platform-independent harness detection
Known harness roots SHALL be detected from path components rather than rendered path separators.

#### Scenario: Codex root on Windows
- **WHEN** a path ends in `.codex\\sessions`
- **THEN** it SHALL be detected as the Codex harness

#### Scenario: Codex root on Unix
- **WHEN** a path ends in `.codex/sessions`
- **THEN** it SHALL continue to be detected as the Codex harness

### Requirement: Verified PowerShell installation
The repository SHALL provide a non-admin PowerShell installer for the published Windows asset.

#### Scenario: Clean installation
- **WHEN** a user runs the documented installer on supported Windows
- **THEN** it SHALL download release metadata and the tagged Windows asset
- **AND** verify the SHA-256 checksum before executing the staged binary
- **AND** verify the binary's reported version before replacing the target
- **AND** install `funes.exe` into the selected directory

#### Scenario: Checksum mismatch
- **WHEN** the downloaded asset does not match `SHA256SUMS`
- **THEN** the installer SHALL fail
- **AND** SHALL leave any existing installed binary unchanged

#### Scenario: User PATH update
- **WHEN** the default install completes without `-NoPathUpdate`
- **THEN** the installer SHALL add the install directory to the user PATH only when absent
- **AND** explain that a new terminal is required

### Requirement: Windows Codex integration
The Windows build SHALL support `funes add codex` and `funes remove codex` without Bash or WSL.

#### Scenario: Add local Codex integration
- **WHEN** a Windows user runs `funes add codex`
- **THEN** Funes SHALL install its Codex skill
- **AND** merge a per-turn indexing hook into Codex configuration
- **AND** register the Funes stdio MCP server
- **AND** preserve unrelated skills, hooks, and MCP servers

#### Scenario: Add a bound-memory integration
- **WHEN** a Windows user runs `funes add codex <org>/<repo>`
- **THEN** Funes SHALL additionally install session-boundary publication hooks
- **AND** preserve the existing first-index and first-push safety gates

#### Scenario: Remove Codex integration
- **WHEN** a Windows user runs `funes remove codex`
- **THEN** Funes SHALL remove only Funes-owned MCP, skill, hook, script, and log artifacts
- **AND** leave local memory, transcripts, remote memory, and unrelated configuration untouched

### Requirement: Windows automation semantics
Windows Codex automation SHALL preserve the existing non-blocking index/push behavior using
PowerShell scripts.

#### Scenario: Per-turn hook returns promptly
- **WHEN** Codex invokes the Funes index hook with a payload on stdin
- **THEN** the hook SHALL consume the payload
- **AND** start a hidden worker
- **AND** return before the configured 15-second hook timeout

#### Scenario: Detached index worker
- **WHEN** the per-turn worker runs
- **THEN** it SHALL execute `funes index --harness codex`
- **AND** append timestamped success or failure diagnostics to `funes-sync.log`

#### Scenario: Session-boundary push worker
- **WHEN** a bound-memory session-start or session-end hook runs
- **THEN** the worker SHALL attempt to index the Codex harness before publishing
- **AND** retry transient writer-lock conflicts up to the configured bound
- **AND** invoke `funes push` for the bound memory

#### Scenario: Path with shell metacharacters
- **WHEN** the installed script or binary path contains spaces or supported Windows shell
  metacharacters
- **THEN** the hook command SHALL pass the path and arguments without truncation or command injection

### Requirement: Safe behavior for out-of-tier Windows features
Features not included in the minimum Windows tier SHALL fail explicitly and without partial writes.

#### Scenario: Unsupported agent integration
- **WHEN** a Windows user requests an agent integration not declared supported by this change
- **THEN** Funes SHALL report that the integration is not yet supported on native Windows
- **AND** SHALL make no integration-file or config changes

#### Scenario: Windows update command
- **WHEN** a Windows user runs `funes update`
- **THEN** Funes SHALL not attempt to rename over the running executable
- **AND** SHALL print the supported PowerShell reinstall command

### Requirement: Windows regression coverage
The repository SHALL continuously verify the supported Windows tier on a GitHub-hosted native
Windows runner.

#### Scenario: Pull request changes platform-sensitive code
- **WHEN** a pull request changes Rust sources, dependencies, scripts, tests, installer, or workflows
- **THEN** required Windows CI SHALL compile the selected release feature set
- **AND** run Windows unit, path, installer, Codex integration, MCP, and index/recall smoke coverage

#### Scenario: Unix non-regression
- **WHEN** the Windows change is validated
- **THEN** existing Linux and macOS CI SHALL remain green
- **AND** existing POSIX hook behavior and release asset names SHALL remain unchanged

### Requirement: Published Windows release asset
Every release that advertises Windows support SHALL publish the verified Windows binary with the
same version metadata and checksum process as existing assets.

#### Scenario: Release publication
- **WHEN** a supported release is published
- **THEN** `funes-x86_64-windows.exe` SHALL be present in the GitHub release and Hugging Face bucket
- **AND** `SHA256SUMS` SHALL contain exactly one digest entry for it
- **AND** publication SHALL require successful Linux, macOS, and Windows builds
