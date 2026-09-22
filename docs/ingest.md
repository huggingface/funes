# Ingesting external sessions

`funes ingest [PATH|-] [--no-thinking]` imports sessions produced by external integrations. It
reads newline-delimited JSON from `PATH`, or standard input when the path is omitted or `-`.

This command accepts session envelopes. For [per-turn `.funes.jsonl` files](funes-jsonl.md), use
`funes index`.

Each line is one complete version 1 session:

```json
{"version":1,"harness":"opencode","session_id":"ses_example","cwd":"/work/project","turns":[{"turn_uuid":"msg_example","parent_uuid":null,"seq":0,"ts":"2026-09-17T12:00:00Z","role":"user","blocks":[{"block_type":"text","text":"Explain this project","tool_name":null,"tool_use_id":null}]}]}
```

Funes validates the entire input before changing memory. The input limit is 64 MiB. `cwd` must be
an absolute path of at most 4096 bytes and is normalized to the same internal workdir facet as native transcripts.
Timestamps must be RFC 3339 and are stored as UTC with millisecond precision, sequence numbers must be nonnegative and strictly increase within a
session, and IDs and harness names must be 1 to 1024 bytes. Raw session IDs must not contain `:`; turn, parent, and tool IDs
may contain it. Roles are `user`, `assistant`, or `tool`; block types are `text`,
`thinking`, `tool_use`, or `tool_result`. Harness names match `[a-z][a-z0-9_-]*`. Unknown fields are
rejected at every level.

Session and turn IDs are stored as `<harness>:<id>`, so identical source IDs from different
harnesses do not collide. Producers must treat submitted IDs as immutable: correcting content means
sending a new ID. Re-sending an existing ID with changed content is ignored by append-only chunk-ID
deduplication.

Imports use the normal redaction, chunking, embedding, and append-only deduplication pipeline.
Re-sending the same complete session is a no-op. Send only stable, completed message content;
pending or streaming tool content should wait for a later complete-session import. `--no-thinking`
excludes thinking blocks just as it does for `funes index`.
