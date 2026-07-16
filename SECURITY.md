# Security Policy

## Reporting a vulnerability

If you believe you have found a security issue in funes, please do **not** open a public
GitHub issue.

Instead, email [security@huggingface.co](mailto:security@huggingface.co) with a description of
the issue, steps to reproduce, and any relevant details. Someone from the Hugging Face
security team will review your report and recommend next steps.

You may also disclose your report through [Huntr](https://huntr.com), a vulnerability
disclosure program for open-source projects.

## What funes does with your data

funes indexes transcripts of your AI agent sessions — among the most sensitive data on your
machine. The design keeps you in control of it:

- **Everything runs locally by default.** Parsing, chunking, embedding, and reranking all
  happen on your machine; the store is a local dataset. Nothing leaves your machine unless you
  run `funes push` (or bind a shared store, whose hooks push at session boundaries).
- **A published store is a Hugging Face dataset repo you own**, gated by your token. Verify
  the repo's visibility on the Hub before pushing history you don't want public.
- **Recall over a remote store** downloads dataset files into a local cache; queries are
  embedded and reranked locally. The Hub serves storage, it never processes your data.

## Secrets

Three layers keep credentials out of a store and off the Hub:

- **At index time**, credentials are redacted from each session before it is stored.
- **At publish time**, an always-on gate scans every outgoing chunk (via
  [trufflehog](https://github.com/trufflesecurity/trufflehog)) and *withholds* any row that
  still contains a secret, exiting non-zero rather than uploading it. The gate is fail-closed:
  if the scanner is missing or crashes, nothing is published.
- **`funes scrub`** removes secrets from rows already in your local store, so a subsequent
  push goes through clean.

### If a secret reached a remote store anyway

The gate stops *future* rows; it cannot unpublish. If a credential made it to a remote store:

1. **Rotate the credential immediately** — assume it is compromised; the repo's history
   retains it even if later commits remove it.
2. Run `funes scrub` locally, then delete the dataset repo on the Hub, recreate it, and
   `funes push` the scrubbed store fresh.

## Recalling from stores you don't own

Recalled passages are inserted into your agent's context. A store published by someone else is
**untrusted input**: a malicious store could embed instructions aimed at your agent (prompt
injection). Bind only stores you trust as a default; for a one-off read of a third-party
store, prefer the per-call form (`funes recall "…" --store org/repo`) over binding it.

## Tokens

Store access uses your Hugging Face token. Prefer
[fine-grained tokens](https://huggingface.co/docs/hub/security-tokens): write scope only for
machines that push, read-only for machines and teammates that only recall.
