# Documentation

This directory contains the user guides and design notes for
[`funes`](../README.md), durable memory for AI coding agents.

## Getting started

- [Adding and removing funes](add.md) — install or remove the tools and automation for Claude Code,
  Codex, pi, or Hermes.
- [Building the memory](index.md) — index session transcripts and understand the indexing
  pipeline.
- [Recalling](recall.md) — retrieve passages and drill into their surrounding turns.
- [Browsing sessions](sessions.md) — list what a memory holds, digest a session, scan one for a
  literal.
- [Asking](ask.md) — borrow a coding agent for one answer grounded in a memory.

## Sharing and automation

- [Automating funes](automation.md) — keep memories current with per-turn indexing and
  session-boundary publishing.
- [Publishing and sharing](push.md) — publish a memory to the Hugging Face Hub and share it across
  machines or teams.
- [Configuration and local files](configuration.md) — state paths, caches, authentication,
  integration files, and environment overrides.
- [Remote-memory caching](hub-caching.md) — how `hf://` recall caches immutable Lance files.

## Design and reference

- [Why funes](RATIONALE.md) — the rationale behind funes's core design choices.
- [Storage growth](storage.md) — measured storage costs and growth estimates.
- [Memory-tool landscape](landscape.md) — a comparison with other agent-memory tools.

The read commands print one stable layout everywhere, and the MCP tools return those same strings
verbatim. [`AGENTS.md`](../AGENTS.md) holds the conventions for working on the repo itself.
