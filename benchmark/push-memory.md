# Push preparation and capture measurements

These opt-in workloads compare the previous full-row preparation path with staged preparation, and exercise native Lance capture separately. They never publish data or update a receipt. Ordinary tests establish correctness; timing is not a correctness assertion.

## Tool choice and scope

Keep one workload invocation per process. This disk- and scanner-heavy operation needs wall time and OS process-tree memory, including native dependencies and TruffleHog. Criterion and Divan are useful for smaller repeatable component benchmarks, but their iteration and value-retention choices need care when measuring large buffers. Gungraun measures simulated instruction/cache costs on Valgrind-supported platforms, rather than this native macOS I/O workload. Hyperfine is an optional command-timing tool; it does not replace resource guards. A custom workload harness is also an established option in the [Rust Performance Book](https://nnethercote.github.io/perf-book/benchmarking.html).

No additional Cargo benchmark dependency or performance CI gate is needed for this change. Run small correctness fixtures in ordinary CI, manual paired workloads when changing preparation/capture, and a guarded large capacity check when smaller results or an unresolved scaling concern justify it. Synthetic workloads are controlled test shapes, not estimates of typical user data or user count.

## Build and runtime

Build before timing:

```sh
cargo test --release --lib --no-run
```

Use the emitted test executable directly for every observation; do not include compilation. Both benchmark tests use the CLI's default multithread Tokio runtime. Keep features, worker-count overrides, compiler, scanner and build profile identical between modes. Record `rustc -Vv`, the source commit and any diff, binary SHA-256, scanner version, CPU/OS, profile, `RUSTFLAGS`, `TOKIO_WORKER_THREADS` and other explicitly set performance overrides. Never record authentication environment values.

Cargo's [release profile](https://doc.rust-lang.org/cargo/reference/profiles.html) is optimized; this project's `ci` profile inherits unoptimized development settings. Distribution builds additionally use thin LTO. Use `--profile dist` when making claims specifically about shipped-binary throughput, and record that choice. The test harness still differs from the complete CLI entrypoint. [Tokio test defaults](https://docs.rs/tokio/latest/tokio/attr.test.html) differ from [CLI runtime defaults](https://docs.rs/tokio/latest/tokio/attr.main.html) unless configured explicitly.

Earlier unoptimized, single-thread preparation measurements are diagnostic evidence only. They do not establish optimized CLI throughput or capacity.

## Reproducible inputs

Generate a new local fixture once, outside measurement, using the built test executable:

```sh
FUNES_BENCH_DATASET=/absolute/new/medium.lance \
FUNES_BENCH_ROWS=10000 FUNES_BENCH_TEXT_BYTES=2048 \
FUNES_BENCH_SPLITS=8 FUNES_BENCH_ROWS_PER_FRAGMENT=1000 \
  /absolute/path/test-executable --exact \
  commands::push::benchmark::generate_push_fixture --ignored --nocapture
```

The generator uses a fixed algorithm/seed, bounded input batches, varied text and finite 384-dimensional vectors. It writes a sibling `medium.fixture.json` with actual row, text/vector-byte, block, fragment and dataset-version metadata. Generated text is clean synthetic prose; secret-bearing decisions are covered separately with generated-key correctness fixtures. Do not overwrite or mutate fixtures between modes.

Start with these independent shapes; there is no need for a Cartesian product:

| Shape | Rows | Text bytes/row | Splits/block | Requested rows/fragment |
|---|---:|---:|---:|---:|
| Small | 1,000 | 2,048 | 8 | 1,000 |
| Medium | 10,000 | 2,048 | 8 | 1,000 |
| Larger | 100,000 | 2,048 | 8 | 1,000 |
| More fragments | 10,000 | 2,048 | 8 | 10 |
| More block files | 10,000 | 2,048 | 1 | 1,000 |
| Wide text | 10,000 | 16,384 | 8 | 1,000 |

Wide text is an explicit stress case. A row limit does not imply a byte limit: Lance also documents row width, decoding parallelism and I/O buffers as [memory costs](https://lance.org/guide/performance/). Record actual fragment count from the fixture metadata. Add selective-ID or first-publication workloads only when those paths are under investigation; the preparation benchmark currently measures filtered selection of all fixture IDs, matching append preparation.

For private real-data stress checks, freeze a pinned version and verify every referenced file, with data kept outside Git. Hardlinks retain old timestamps and can be removed from an OS temporary directory by age-based cleanup; independent fixture files with fresh timestamps avoid that particular failure. Record the frozen manifest/version and input provenance. A live remote revision is relevant only when a run actually accesses the remote.

## Execution and comparisons

For one preparation observation:

```sh
FUNES_BENCH_DATASET=/absolute/path/medium.lance FUNES_BENCH_MODE=staged \
  /absolute/path/test-executable --exact \
  commands::push::benchmark::benchmark_push_preparation --ignored --nocapture
```

Use `FUNES_BENCH_MODE=legacy` for the reference implementation. Require TruffleHog before performance runs; a missing scanner is a failure. Before claiming scanner-dependent correctness coverage, also verify the key-generation tool is present: some ordinary tests return early when their prerequisites are absent.

Run an explicit warm-up in each mode, then begin with five alternating A/B pairs for the baseline sizes. Keep first-run results separate. Three pairs are exploratory, not a basis for a statistical confidence claim. Report every observation, median/range and paired ratios; add repetitions only when variability or the proposed conclusion warrants them. Single paired stress-axis probes can find a scaling issue but do not establish a throughput distribution. A warm-up is not proof that a large fixture fits in OS cache. Do not claim cold-cache results without controlling cache state. Run builds and other benchmarks separately from timed observations.

Compare ordered ID/text/vector digests (including null validity) and clean/held counts for completed pairs. These are selected-column checks; small regression fixtures compare all columns and directly inspect reopened IPC schema and field metadata. Empty/all-held cases belong in correctness coverage. A guard-stopped legacy run is a censored result: report neither a completed peak nor a full-size speedup/equality result from it.

`BENCH` markers include the workload's monotonic seconds. Preparation ends at `fingerprint`; hashing is validation work. The `secret_gate` interval still combines text fetch/reconstruction/file creation, TruffleHog, result parsing and cleanup. It cannot identify a filesystem or scanner bottleneck by itself. Use a separate targeted Instruments/File Activity diagnostic if that combined interval needs explanation.

## Native capture and resource accounting

```sh
FUNES_BENCH_DATASET=/absolute/path/medium.lance \
  /absolute/path/test-executable --exact \
  memory::remote::tests::benchmark_capture_only --ignored --nocapture
```

Capture-only uses a bounded reader and a tiny `memory://` seed, forcing Lance through the object-store writer used for `hf://`. It verifies actual data files and the entire unchanged underlying seed-store inventory. A local-file seed can bypass capture and is unsuitable. This run stages all fixture rows locally, including rows a real gate could hold; it has no scanner, receipt, network or indexing phase. `FUNES_BENCH_CAPTURE=1` also enables capture after staged preparation, but fingerprinting has already warmed its IPC spool; label that cache state explicitly.

On macOS, `measure_push.py` wraps one built workload with `/usr/bin/time -l` and samples simultaneous process-tree footprint/RSS plus system swap and scratch free space. For example:

```sh
FUNES_BENCH_DATASET=/absolute/path/medium.lance FUNES_BENCH_MODE=staged \
  python3 benchmark/measure_push.py --output /absolute/new-run \
  --scratch /absolute/dedicated-scratch -- /absolute/path/test-executable \
  --exact commands::push::benchmark::benchmark_push_preparation --ignored --nocapture
```

See `python3 benchmark/measure_push.py --help` for guard settings. The native output supplies exit CPU accounting and the main workload's lifetime peak footprint; these differ from sampled simultaneous tree memory. [Apple's time implementation](https://github.com/apple-oss-distributions/shell_cmds/blob/main/time/time.c) uses `wait4` and process rusage. Do not add sampled CPU to exit totals or describe the main process's lifetime peak as a tree peak.

Use 32 GiB sampled tree footprint and 4 GiB additional global swap as protection thresholds, with a free-disk guard. Sampling may detect a breach after it occurs; these are not hard resource caps. Global swap includes unrelated applications. Keep raw output, binary/wrapper identity, per-run phase markers and status. A stopped/failed run is not a successful measurement. Check sampler overhead on a smaller fixture against a native-time-only run before interpreting small differences. For subsecond capture runs, one-second tree samples can miss the entire allocation phase; reduce the interval, check its overhead, and retain the native main-process lifetime peak separately.

Do not repeatedly walk millions of staged files with `du` during timing. Capture reports logical file bytes; filesystem allocation and actual disk I/O are different measurements. Inspect disk occupancy in a separate diagnostic run, and use free-space checks for protection. The runner does not claim a disk-usage high-water mark.

Actual Hub transport, commit conflicts and native indexing require separate integration evidence. Offline replay proves repeatable payload consumption and capture behavior, not a live HTTP conflict. The existing Hub tests require their configured test token. ID/block metadata, external scanner memory, native indexes and hf-hub's non-Xet inline request buffering remain outside any claim of constant total memory.
