# funes-pi

A [pi](https://github.com/badlogic/pi-mono) extension that exposes funes recall
as first-class `recall` / `get` tools.

pi has no MCP client, so the extension *is* one: it spawns `funes mcp` once over
stdio, keeps it warm for the session, and forwards each call as an MCP
`tools/call`. Same `funes mcp` surface every other agent integration consumes —
just fronted by a thin pi tool.

## Install

Once `funes` is on your PATH, one command extracts this extension to a fixed
`~/.funes/integrations/pi` and registers it with pi, user-wide:

```sh
funes add pi
```

funes embeds the extension in its binary, so this always matches the installed
funes version — no separate package to fetch, and a re-run after an upgrade
re-extracts the refreshed copy automatically (`--force` rewrites even when the
on-disk copy is already current).

For development from a funes checkout you can also install the package directly
with `pi install ./integrations/pi`, or load it for a single run with
`pi -e ./integrations/pi`.

> There's no `pi install git:…/funes`: pi has no subdir/monorepo install syntax,
> and the funes repo root is a Cargo project rather than a pi package.

## Requirements

- `funes` on `PATH` (set `FUNES_BIN` to override the binary path).
- A funes store the binary can read — local, or a live `hf://` remote configured
  via `FUNES_HOME`/`funes.json` (needs network + an HF token for a private remote).
  Set `FUNES_STORE` to pin one explicitly — forwarded as `funes mcp --store <spec>`.

`typebox` and the pi SDK are provided by pi's loader, so the extension declares
no dependencies.
