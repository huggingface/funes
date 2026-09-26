# Contributing to funes

Contributions are welcome — bug reports, fixes, documentation, and features. Bug reports and
docs improvements can go straight to an issue or PR. For features in funes itself, please read
the section below first.

**Adding an agent integration?** Start with [Writing an integration](#writing-an-integration).
You build and publish it in your own repository, and can list it in
[funes-integrations/COMMUNITY.md](https://github.com/huggingface/funes-integrations/blob/main/COMMUNITY.md).
You can develop and release it without an issue or code PR here. Fixes to the integrations Hugging
Face maintains go to [huggingface/funes-integrations](https://github.com/huggingface/funes-integrations).

## Before you build a feature

Open an issue describing the **problem** before investing time in an implementation. funes is
built on a small set of deliberate, load-bearing constraints — append-only storage, no LLM in
the ingest path, local-first, recall pulled rather than injected — documented in
[docs/RATIONALE.md](docs/RATIONALE.md). A feature that fights one of these (LLM summarization
at ingest, mutable "facts", proactive memory injection) is a different product, not a missing
feature, and will be declined; an issue costs you an hour less than a PR does.

Two more places to look before proposing:

- [AGENTS.md](AGENTS.md) holds the conventions and the hardened decisions, and names the four
  surfaces that describe a verb — CLI help, MCP tool descriptions, docs, and the output shape. A
  change to one of them is a change to all four.
- [Why integrations live outside this repository](docs/RATIONALE.md#why-integrations-live-outside-this-repository)
  explains the boundary between funes's shared interfaces and each agent's integration.

## Development setup

You need:

- **Rust** (stable; CI builds with 1.95.0)
- **`protoc`** — `lance`'s build scripts compile protobuf at build time:

  ```bash
  sudo apt-get install -y protobuf-compiler   # Debian/Ubuntu
  brew install protobuf                        # macOS
  ```

- **[trufflehog](https://github.com/trufflesecurity/trufflehog)** — the pre-publish secret
  gate shells out to it (CI pins v3.95.5). Needed on `PATH` (or via `FUNES_TRUFFLEHOG`) to run
  the secret-scan tests and `funes push`.
- **`expect`** — the `funes add` tests run the binary at a pty to answer the prompts it only asks
  at a terminal. macOS ships it; `sudo apt-get install -y expect` elsewhere.

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

- **One directory per layer**, each with one job — `src/` holds `main.rs`, `lib.rs` and the layer
  roots, nothing else:

  | Layer | Job |
  |---|---|
  | `traces/` | where turns come from: turns files, the spools integrations write them into, Hub parquet, and the `Turn`/`Block` model they produce |
  | `chunk.rs`, `scan.rs` | the models the layers share: chunk text and ids, secret findings |
  | `inference/` | embedding and reranking behind traits (backend chosen at build time) |
  | `hub.rs` | **transport**: the Hub's client, credentials, and dataset-repo identity and lifecycle. Knows nothing about memories — `memory`, `traces` and `commands` all call it |
  | `memory/` | the memory itself: Lance and object-store **mechanics** (`dataset`, `fetch_store`, `capture_store`), its remote side (`remote`), under a **domain** (`memory.rs`) that says what a memory is and what state it's in |
  | `commands/` | what funes does when you run it: orchestration and decisions |
  | `ui/` | how a result reaches the terminal |
  | `agents/` | registering funes with a coding agent (MCP + automation hooks) |

  Where does a new function go? Names an HF concept → transport. Names Lance → mechanics.
  Answers *what is this memory, what state is it in* → domain. Decides *what to do about it* →
  command.

  Commands **ask** the domain for state — `Memory::state()` returns a `MemoryState`, and the
  message for a state a command can only stop on comes from the memory itself
  (`memory.missing_error()`). Don't read state out of an error shape: four different answers to
  "does this memory exist" grew that way, and collapsing them was the point of the layering.

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`): short imperative subject, the
  "why" in the body when it isn't obvious.

## Writing an integration

Keep your integration's source, tests, releases, installation instructions, and support in a
repository you maintain. The four in
[huggingface/funes-integrations](https://github.com/huggingface/funes-integrations) are worked
examples maintained by Hugging Face; community integrations are published by their authors.

### Build and test

Choose the funes interfaces your integration needs. A client can use the CLI or
[MCP](docs/add.md#other-mcp-clients) to recall memory, and a converter can write
[turns files](docs/funes-jsonl.md) for `funes index`. A native plugin or a converter alone can be
shared and listed too.

For installation through `funes add`, make a bundle with a `manifest.json` and an executable
`setup` implementing `setup add [MEMORY]` and `setup remove`. Follow the
[integration contract](docs/add.md#the-integration-contract) for the manifest, environment,
and spool. The [pi bundle](https://github.com/huggingface/funes-integrations/tree/main/pi)
shows how setup, a converter, automation, and tests fit together.

If your integration indexes sessions, convert the agent's existing history at setup and use its
hooks or events to keep the spool current, then run `funes index --harness <id>`. Keep a real
transcript and its expected turns as a converter test, with stable session and turn ids across
re-runs and releases. Test setup and removal against a fake agent and temporary configuration,
as the maintained bundles do. Validate the converter's output, then try the full install in a
test account or isolated agent configuration:

```bash
funes index --check /path/to/converted-turns
funes add <id> local --from /absolute/path/to/bundle
funes remove <id>
```

funes asks for confirmation before running your bundle's setup.

### Publish and list

Publish your source and installation instructions in your repository. Users can clone it and
install from the bundle directory with `--from`. For a downloadable bundle, publish an
`hf://buckets/<owner>/<bucket>/<path>/<id>.tar.gz` archive with `manifest.json` and `setup` at its
root and a `SHA256SUMS` beside it; users pass that archive URL to `--from`. Keep release and
compatibility notes with your integration.

To make it discoverable, open a **documentation-only PR in huggingface/funes-integrations**
adding one row to
[COMMUNITY.md](https://github.com/huggingface/funes-integrations/blob/main/COMMUNITY.md#listing-yours).
Include its name, publisher, target harness or client, what it does, the funes interfaces it uses,
and links to your source, installation instructions, compatibility notes, and support.
The listing points users to your project; its review checks attribution and links, and does not
install or execute your code. A listing adds no catalog entry or `funes add <id>` alias: your
instructions remain the way users install it.

## Pull requests

1. Branch from `main`; keep the PR focused on one concern.
2. Add or update tests for any behavior you change.
3. Run fmt, both clippy variants, and the test suite before pushing.
4. In the description, say what changed and why, and link the issue it resolves.

Found a security issue? Please **don't** open a public issue — follow
[SECURITY.md](SECURITY.md).
