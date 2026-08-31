# funes-pi

A [pi](https://github.com/earendil-works/pi) extension that gives pi the funes
read tools.

pi has no MCP client, so the extension *is* one: it spawns `funes mcp` once over
stdio, keeps it warm for the session, and forwards each call as an MCP
`tools/call`. Same `funes mcp` surface every other agent integration consumes —
just fronted by a thin pi tool. Which tools those are isn't listed here: it
registers whatever `tools/list` returns, so a pi session sees exactly the surface
of the funes binary on PATH.

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
- pi >= 0.84.0, which is where pi validates the nullable-union arguments the funes tool schemas
  declare. `funes add pi` refuses to register the extension on an older pi rather than leave you
  with tools that reject their own calls.
- A funes memory the binary can read — local, or a live `hf://` remote (needs network + an HF
  token for a private remote). Bind one with `funes add pi <memory>`, or set `FUNES_MEMORY` to pin
  it explicitly — forwarded as the `funes mcp <memory>` positional.

The extension declares no dependencies: it talks to `funes mcp` over stdio and to pi through the
extension API pi's loader provides.
