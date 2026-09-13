> [!NOTE]
> **Agent-written document.** An agent produced this document. Austin did not necessarily write, review, endorse, or verify its contents. Evaluate its claims with the same care as other agent-generated output.
>
> **Last Updated** 2026-09-12

---

# Push memory comparison

`commands::push::tests::benchmark_push_preparation` is an opt-in local benchmark. It compares the pre-change full-row selection and block gate with staged preparation. It never uploads or changes a receipt. Ordinary tests do not run it.

Freeze a representative local dataset first, preserving every file needed by its pinned version. Use the same fixture, build profile, compiler and scanner for both modes. Do not run against an actively changing backlog. Run one warm-up and at least three alternating paired samples on smaller fixtures before a guarded full-backlog run.

```sh
FUNES_BENCH_DATASET=/absolute/path/chunks.lance FUNES_BENCH_MODE=legacy cargo test --profile ci --lib benchmark_push_preparation -- --ignored --nocapture
FUNES_BENCH_DATASET=/absolute/path/chunks.lance FUNES_BENCH_MODE=staged cargo test --profile ci --lib benchmark_push_preparation -- --ignored --nocapture
```

Build first, then invoke the emitted test executable directly under a process-tree sampler to exclude compilation. On macOS record physical footprint and resident size separately, CPU time, elapsed phase times, temporary disk usage and system swap. Swap is global and may include unrelated desktop activity. Guard runs at 32 GiB process-tree footprint or 4 GiB additional system swap; an interrupted run is a lower bound, not a completed peak. Record source revision, binary SHA-256, OS/hardware, scanner version, dataset version and fixture row count. Keep private evidence outside the repository.

Both modes run in the same test executable, sharing dependency versions and scanner invocation; the legacy mode retains the old full-row collection and reconstruction code as a reference. The benchmark reports clean and held counts plus ordered digests of IDs, text and vector values. These must match across modes; unit tests additionally check every column and schema metadata. Both modes report preparation and fingerprint time separately. Set `FUNES_BENCH_CAPTURE=1` in staged mode to additionally measure native Lance append through the file-backed capture store; that phase uses a tiny `memory://` seed to force Lance through the object-store writer, checks that actual data files were captured, and compares the underlying store's complete inventory before and after. It excludes network and native index construction. Do not conflate it with remote end-to-end publication. Reindex and actual upload costs require separate measurement.

Memory for selected payloads should no longer grow with the entire backlog. ID sets, block metadata, scanner results and dependency caches can still scale; the external scanner and native indexing have separate requirements. OS scheduling and caches make timings repeatable estimates rather than deterministic values. Report all repetitions and throughput tradeoffs, not just the best run.

To measure capture independently without repeating the secret scan, run the bounded-reader fixture benchmark:

```sh
FUNES_BENCH_DATASET=/absolute/path/chunks.lance cargo test --profile ci --lib benchmark_capture_only -- --ignored --nocapture
```

This stages all fixture rows locally, including any rows a real push would hold. It neither uploads nor updates receipts. Use the same memory/swap guards and private temporary directory. A local-file seed is unsuitable: Lance can bypass the object store for local data writes, which would measure only captured manifests.
