# Memory-tool landscape

Companion to [RATIONALE.md](RATIONALE.md), which argues *why* funes is built the way it is.
This file puts funes next to the comparable memory tools — first on **how they differ**, then
on **how much traction they have** — so the comparison is grounded, not abstract.

## How they differ

| Dimension | **funes** | **claude-mem** | **mem0** |
| --- | --- | --- | --- |
| Focus | Verbatim recall over coding-agent sessions | Compressed memory for coding agents | General-purpose LLM memory layer (coding via Skills) |
| Language / footprint | Single Rust binary | TS/JS on Node + Bun; local HTTP worker | Python & TS SDK (+ Docker server / cloud) |
| Ingest | **Deterministic — no LLM** (parse → chunk → embed) | LLM makes semantic summaries + tool-use observations | LLM extraction — one call distills a turn into "memories" |
| Kept unit | The **verbatim passage** | Distilled summaries / observations | Distilled facts ("memories") |
| Memory model | Append-only, immutable | Derived, LLM-compressed summaries | Append-only (v3, Apr 2026; earlier versions reconciled ADD/UPDATE/DELETE) |
| Provenance | **Exact & verbatim** (`session_id` + `turn_uuid`) | Observation IDs — cite the summary, not the raw turn | None described |
| Retrieval | vector + BM25 → RRF → **cross-encoder rerank** → recency → neighbors | FTS5 + Chroma hybrid; progressive disclosure (search → timeline → get) | semantic + BM25 + entity, fused + temporal reasoning (no rerank noted) |
| Delivery | **Pulled only** — never injected | Injected via hooks + queryable | Pulled via SDK/API; the app decides when to inject |
| Data location / ownership | Local-first; optional sync to an **HF dataset you own** — no third-party model ever runs on your data | Local SQLite + worker; optional cloud sync to cmem.ai | Library / self-host / managed cloud (app.mem0.ai); default ingest uses OpenAI, so data leaves unless self-hosted with local models |
| Integrations | MCP (`recall`/`get`) — add to Claude, Codex, pi, Hermes | Hooks + MCP + plugin — Claude Code, Codex, Gemini, Hermes, Copilot, OpenCode… | SDK + Agent Skills — Claude, Cursor, Codex…; bundled by Hermes |

The through-line matches [RATIONALE.md](RATIONALE.md): funes keeps the **raw passage** with exact
provenance and serves it **only when asked**; the others **distill with an LLM** and lean on
**proactive injection**. Note mem0 v3 (Apr 2026) moved to an append-only memory — so *append-only*
no longer separates funes from mem0; the durable differences are **no-LLM/verbatim ingest**,
**provenance**, and **data you own**.

## Traction

GitHub stars are a rough proxy for reach, not quality — and they move. **Snapshot 2026-07-14**
(GitHub API); re-run to refresh.

| Repo | Stars | Forks | Created | Last push | Note |
| --- | --- | --- | --- | --- | --- |
| [thedotmack/claude-mem](https://github.com/thedotmack/claude-mem) | 87,192 | 7,543 | 2025-08 | 2026-07-13 | Dedicated coding-agent memory; the category incumbent, actively maintained |
| [mem0ai/mem0](https://github.com/mem0ai/mem0) | 60,808 | 7,084 | 2023-06 | 2026-07-14 | General-purpose memory layer; also the **most-starred memory provider Hermes bundles** |
