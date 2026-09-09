# Windows acceptance review — 2026-09-09

Tested implementation: `176a3790bc7a750c5af5f3202ee2dafbe76c2dc3`.

## Decision

Automated CI acceptance passes for the covered scenarios. Full desktop/release acceptance remains
open. This review inspected completed job logs and artifact metadata; it did not execute a new
Windows desktop session. Keep the PR as a draft until the intended acceptance scope is resolved.

## Verified evidence

| Area | Result | Evidence and limit |
| --- | --- | --- |
| Windows native tests | Pass | Windows Server 2025: 255 unit tests, one real index/recall integration test, add_codex_hooks and windows_codex each passed |
| Configuration regression fixes | Pass | The 255 unit tests include the four review regressions for mixed hook groups, invalid containers, preservation, and read errors |
| Installer and background hooks | Pass with fixtures | PowerShell suite reports success; installation uses mocked downloads and hooks invoke a fixture executable |
| Codex registration and cleanup | Pass with fixture | Compiled codex.exe verifies argument forwarding, home selection, idempotency and cleanup; this does not prove a real Codex conversation triggers hooks |
| Windows static checks | Pass | Native dependency check and all-target Clippy completed successfully |
| Linux regression CI | Pass | Completed CI workflow at the tested implementation |
| Release builds | Pass | Windows x64, Linux x64/ARM64 and macOS ARM64 built successfully; Windows job checks the staged executable's version |
| Published release | Not performed | Publish Release was skipped for this PR build |

- [Windows tests and full log](https://github.com/wellorbetter/funes/actions/runs/34239759269/job/102106600876)
- [Linux CI](https://github.com/wellorbetter/funes/actions/runs/34239759267)
- [Release builds](https://github.com/wellorbetter/funes/actions/runs/34239759213)

The Windows artifact is `windows-x86_64`, artifact ID `10064575172`, ZIP size 74,993,499 bytes.
GitHub reports ZIP SHA-256
`880ac0448b453e61e21507391ed219c1918a5669c5f9f9b35699e57bc95620a4`.
This is the archive digest, not the executable digest. The artifact download connector succeeded,
but transferring its returned URL into the local inspection process returned HTTP 403; independent
ZIP/PE/hash inspection was therefore not completed or claimed.

## Remaining acceptance checks

| Check | Required evidence |
| --- | --- |
| Clean Windows 11 and Windows 10 journey | Standard user installation, new CMD and PowerShell sessions, version, explicit-path index, recall, real Codex add/turn/background index/remove; retain logs and exact OS versions |
| Real Codex and npm shim | Record supported Codex version, test native executable and npm .cmd invocation with space/Unicode paths, trust hooks, complete a turn and observe real indexing |
| MCP stdio | Initialize/list tools/call a recall tool through the actual funes process; verify stdout contains protocol messages only |
| Full CLI surface | Exercise status, get, sessions, sketch, scan, scrub, ask codex and relevant error paths; a passing index/recall test is not evidence for every CLI command |
| Authenticated Hub and scanner | Use an authorized disposable memory and synthetic data; verify read/push, real trufflehog discovery, missing-scanner failure and secret rejection |
| Cross-OS memory compatibility | Open/query a Windows-created memory on Linux and a Linux-created memory on Windows; assert records and metadata survive both directions |
| Installer with the actual release asset | Use the staged release executable and matching manifest in a clean profile; verify first install, reinstall, version, PATH and failed-update preservation |
| Repeatability | Two native runs succeeded, but no deliberate cold-cache/warm-cache comparison was performed; do not mark cache-independent repeatability complete |

No Windows desktop/VM runtime, real Codex session, or authenticated Hub test environment was available
for this review. The existing OpenSpec checklist remains authoritative for uncompleted work. A
successful macOS build also does not establish that macOS automation integration tests ran.

## Next acceptance session

Use a clean Windows machine or connected Windows runner and a synthetic transcript. Keep application
state under a temporary FUNES_HOME and Codex configuration under a temporary CODEX_HOME. Capture
commands, exit codes, tool versions and hook logs. Establish the local and MCP journeys before
performing authenticated Hub tests with a separately authorized disposable target. Do not use real
conversation history as acceptance data.
