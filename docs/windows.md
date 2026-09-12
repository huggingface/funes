# Native Windows

The minimum tier is the x86-64 native CLI plus Codex MCP, skills, and automatic indexing/publishing.
The executable runs from PowerShell or CMD without Bash or WSL. Automation and installation use
Windows PowerShell 5.1. Claude, pi, and Hermes transcript files can still be indexed explicitly;
their automatic integrations, Windows ARM64, desktop UI, and in-place self-update are excluded.

## Availability and installation

The Windows asset is `funes-x86_64-windows.exe`. It will be available in the first release containing
this change; the installer intentionally fails if a selected older release has no Windows checksum.
Until that release, build this branch with Rust 1.95.0, Visual Studio C++ build tools, and protoc 28.3:

```powershell
$env:PROTOC = 'C:\tools\protoc\bin\protoc.exe'
cargo build --release --target x86_64-pc-windows-msvc
.\target\x86_64-pc-windows-msvc\release\funes.exe --version
```

After a Windows release is published, download the installer and run it from PowerShell:

```powershell
Invoke-WebRequest https://huggingface.co/buckets/huggingface/funes/resolve/install.ps1 -OutFile "$env:TEMP\funes-install.ps1"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$env:TEMP\funes-install.ps1"
```

The default location is `%LOCALAPPDATA%\Programs\funes\bin`. The installer verifies the tagged
version, the SHA-256 manifest, and the staged executable's reported version before replacing an
existing binary. Open a new terminal after its user PATH update. Options are `-InstallDir`,
`-Version` (for example `1.3.0` or `v1.3.0`), and `-NoPathUpdate`. A custom install directory is not
added to PATH automatically. No administrator permission or persistent execution-policy change is
required; organizational policies can still restrict script execution.

Close running agents and MCP servers before reinstalling. `funes update` prints the reinstall
command and does not download or attempt to overwrite its running executable on Windows.

## CLI and Codex

```powershell
funes --version
funes index 'C:\Users\Alice\transcripts' --harness codex
funes recall 'why did we choose this design'
funes add codex
# In Codex, use /hooks to review and trust the installed commands.
funes remove codex
```

In CMD, quote paths with double quotes: `funes index "C:\Users\Alice\transcripts" --harness codex`.
Drive paths and UNC paths remain local memory paths rather than Hub repository shorthands.

`codex doctor --json` selects Codex's configuration directory when available, followed by
`CODEX_HOME`, then the user's Windows profile plus `.codex`. The installed scripts are
`hooks\funes-index.ps1` and `hooks\funes-push.ps1`; logs go beside them in `funes-sync.log`.
The foreground hook drains its input and starts a detached worker. Encoded PowerShell commands keep
paths and arguments out of CMD's expansion rules. The push worker retries indexing up to five times
before publishing what is already stored; secret-gate exit code 2 is logged as a warning.

Codex native executables and npm `.cmd` shims are resolved from PATH. Arguments are passed through
Rust's process API, including its batch-file escaping rules, rather than concatenated into a CMD
command ([Rust process documentation](https://doc.rust-lang.org/std/process/index.html)).

State defaults to `%USERPROFILE%\.funes`, with `FUNES_HOME` taking precedence. `CODEX_HOME` also
controls the sessions directory used by automated indexing. `HF_HOME` overrides the Hugging Face
cache root; `HF_TOKEN_PATH` overrides its token file. Token environment variables retain priority.
`FUNES_BIN` can point hooks and MCP registration to an explicit `funes.exe` path.

Publishing requires a Windows `trufflehog.exe` on PATH or an absolute `FUNES_TRUFFLEHOG` override.
Install the official trufflehog executable before binding an agent to a remote memory. The scan
fails closed when the executable is unavailable. WSL continues to use the existing Linux binary,
installer, and Bash automation separately.

## Verification and release checklist

The native dependency probe passed real index/recall tests with both BLAS and ONNX on an MSVC
Windows runner. The release backend remains the default BLAS/faer; the models and memory schema
are unchanged. See the [execution record](../openspec/changes/add-windows-minimum-support/execution.md)
for the initial build evidence and the [acceptance record](../openspec/changes/add-windows-minimum-support/acceptance.md)
for subsequent desktop and authenticated Hub validation.

The Windows job runs PowerShell installer/hook tests, the real-memory test under space/non-ASCII
paths, Clippy, unit tests, and Codex integration tests with a compiled native fixture. Before
advertising a desktop release, run a clean Windows 10/11 install → CMD version → index → recall →
Codex turn/hook → remove journey. CI uses a Windows Server runner; it does not constitute that
manual desktop test. Authenticated Hub round trips and Windows/Linux memory exchange passed with
synthetic data. Clean desktop installation and installation from a versioned release asset remain
unverified.
