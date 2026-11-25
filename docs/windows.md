# funes on Windows

funes runs natively on x86-64 Windows 10 and 11, without WSL: the same commands, the same output,
and the same memory format, on disk and on the Hub. What differs from the other pages is here, and
only here.

## Installing

From PowerShell — Windows PowerShell 5.1, which every Windows has, or a later one:

```powershell
irm "https://huggingface.co/buckets/huggingface/funes/resolve/install.ps1" | iex
```

The [installer](../scripts/install.ps1) downloads `funes-x86_64-windows.exe`, verifies its tagged
release checksum and version, installs it as `~\.local\bin\funes.exe`, and appends that directory to
your user `PATH` when it is not on it — as written, so a `PATH` spelled with `%VARIABLES%` keeps
them. It needs no administrator rights. Run from a file it takes `-InstallDir`, `-Version` and
`-NoPathUpdate`; piped to `iex`, set `FUNES_INSTALL_DIR` or `FUNES_VERSION` first.

`funes update` works as elsewhere. Windows will not write over a running program, and an agent's
MCP server is funes running, so the installer and the update both rename the old binary aside, as
`funes.exe.<pid>.old`, and a later run deletes it. Nothing needs closing first; running agents keep
the old funes until they restart.

Other processors have no prebuilt binary: [build from source](#building-from-source).

## Where things live

`~` is your user profile, `%USERPROFILE%`: funes's home is `%USERPROFILE%\.funes`, the registry of
integrations `%USERPROFILE%\.funes\agents`, and the models and your token are in the Hugging Face
cache `huggingface_hub` uses, `%USERPROFILE%\.cache\huggingface`. `HOME` is not consulted: a POSIX
shell sets it and PowerShell does not, and funes must find the same home from both. `FUNES_HOME`,
`HF_HOME` and the other [overrides](configuration.md) work as elsewhere.

A memory is named as elsewhere. A Windows path names a local one however it is slashed —
`C:\memories\team`, `C:/memories/team`, `\\server\share\team` — and is never taken for an
`<org>/<repo>` shorthand.

## Adding an agent

`funes add <agent>` runs the agent's integration, whose `setup` is a script for a POSIX shell.
Windows runs no file by its first line, so funes hands `setup` to an `sh.exe`: the first on `PATH`,
else [Git for Windows](https://gitforwindows.org)'s — beside the `git.exe` on `PATH`, or where its
installer puts it. Without one, `funes add` says so before it installs anything. WSL's `bash.exe`
is not one: it would run `setup` in another system, with another home.

funes hands `setup` its paths spelled with `/`, which Windows takes as well as `\`. Whether a
given integration supports Windows, and what else it needs there, is its own README's to say: the
maintained four are in
[huggingface/funes-integrations](https://github.com/huggingface/funes-integrations).

Two checks on [the registry](add.md#where-the-integrations-files-come-from) differ from elsewhere:

- A file has no permission bits, so funes does not judge who else could write to the registry. It
  is closed to other users by living under your profile, whose access list every file created there
  inherits. A link on the way to `setup` is refused as elsewhere, a directory junction included.
- Every file counts as executable; a `setup` that cannot run fails when funes runs it.

## Asking

[`funes ask`](ask.md) runs the agent's `.exe` on `PATH` — else, where npm installed it, the
launcher npm puts there, `codex.cmd`.

## Publishing

`funes push` scans with [trufflehog](https://github.com/trufflesecurity/trufflehog) as elsewhere:
`trufflehog.exe` on `PATH`, or named by `FUNES_TRUFFLEHOG`.

## Building from source

The MSVC toolchain (the "Desktop development with C++" workload of Visual Studio or its Build
Tools), Rust through rustup, and `protoc`:

```powershell
winget install Google.Protobuf
cargo build --release      # target\release\funes.exe
```

Inference runs on the same pure-Rust backend as on Linux. The tests need Git for Windows, for the
shell that runs a `setup`. Those that answer `funes add`'s prompts drive a pty with `expect` and
are Unix's; `tests/add_installed_integration.rs` covers the run that asks nothing.
