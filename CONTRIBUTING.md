# Contributing to funes

Contributions are welcome — bug reports, fixes, documentation, and features. Bug reports and
docs improvements can go straight to an issue or PR. For features, please read the section
below first.

## Before you build a feature

Open an issue describing the **problem** before investing time in an implementation. funes is
built on a small set of deliberate, load-bearing constraints — append-only storage, no LLM in
the ingest path, local-first, recall pulled rather than injected — documented in
[docs/RATIONALE.md](docs/RATIONALE.md). A feature that fights one of these (LLM summarization
at ingest, mutable "facts", proactive memory injection) is a different product, not a missing
feature, and will be declined; an issue costs you an hour less than a PR does.

Two more places to look before proposing:

- [AGENTS.md](AGENTS.md) is the specification of the CLI surface — flags, output formats,
  defaults. If your change touches any of those, the spec changes with it.
- Adding support for another agent means implementing the [`TraceSource`](src/source.rs)
  trait — the indexing and query paths should not need to change.

## Development setup

You need:

- **Rust** (stable; CI builds with 1.95.0)
- **`protoc`** — `lance`'s build scripts compile protobuf at build time:

  ```bash
  sudo apt-get install -y protobuf-compiler   # Debian/Ubuntu
  brew install protobuf                        # macOS
  # …or repo-local, no sudo:
  ./scripts/bootstrap-protoc.sh
  export PROTOC="$PWD/.tools/protoc/bin/protoc"
  ```

- **[trufflehog](https://github.com/trufflesecurity/trufflehog)** — the pre-publish secret
  gate shells out to it (CI pins v3.95.5). Needed on `PATH` (or via `FUNES_TRUFFLEHOG`) to run
  the secret-scan tests and `funes push`.

## Building and testing

```bash
cargo build --release          # binary at target/release/funes
cargo test --lib               # unit tests — hermetic, no network
cargo test                     # full suite; first run downloads the embedder/reranker weights
```

The tests that talk to the Hugging Face Hub (`remote_recall`, `push_round_trip`) skip
themselves unless `HF_FUNES_TEST_TOKEN` is set — on a fork they simply don't run, and that's
fine; CI runs them with the repository secret.

## Style

- **Format** with `cargo fmt` (stable rustfmt; [rustfmt.toml](rustfmt.toml) carries the one
  setting that differs from defaults).

- **Lint** both backend variants — warnings are errors in CI:

  ```bash
  cargo clippy --all-targets -- -D warnings
  cargo clippy --all-targets --no-default-features --features onnx -- -D warnings
  ```

- Doc-comment the public surface; comments carry the non-obvious *why*, not a restatement of
  the code.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`): short imperative subject, the
  "why" in the body when it isn't obvious.

## Pull requests

1. Branch from `main`; keep the PR focused on one concern.
2. Add or update tests for any behavior you change.
3. Run fmt, both clippy variants, and the test suite before pushing.
4. In the description, say what changed and why, and link the issue it resolves.

Found a security issue? Please **don't** open a public issue — follow
[SECURITY.md](SECURITY.md).
