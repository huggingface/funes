# Storage growth

How big does a funes memory get, and how does it grow? This document builds a
per-chunk cost model from three real memories and projects it forward.

All figures were measured with funes 0.8.0 against bge-small-en-v1.5 memories
(`DIM = 384`). Coefficients shift a little with chunk length and vocabulary, but
the shape holds.

## Reference memories

Two local memories on the same author, plus the published aggregate:

| | Cloud dev host | Main dev host | Remote (aggregate) |
|---|---|---|---|
| role | focused sessions | foundation / design work | published union |
| chunks | 5,762 | 65,421 | 93,684 |
| size (data + live index) | ~14 MB | ~148 MB | ~214 MB (est.) |

Both memories hold chunks of the **same size** — ~2.1 KB each. The main memory is
larger only because it has more of them, not heavier ones; per-chunk cost is
essentially content-independent.

## What's on disk

A memory is a single Lance dataset (`chunks.lance`) with these parts:

| Part | Holds | Grows with |
|---|---|---|
| `data/` | rows — text, 13 metadata cols, the embedding vector | chunks × content |
| `_indices/` | BM25 + vector index | chunks (sublinear) |
| `_versions/` | one manifest per committed version | number of index/push commits |
| `_transactions/` | transaction records | negligible |

Breakdown (`data/` plus the live index generation):

| Component | Cloud dev | Main dev |
|---|---|---|
| `data/` (live rows) | 11.9 MB | 136 MB |
| `_indices/` (live generation) | 2.2 MB | 12.0 MB |

## The per-chunk model

### Data — a fixed vector floor plus bounded text

| | Cloud dev | Main dev |
|---|---|---|
| data B/chunk | **2,168** | **2,080** |
| — vector (fixed) | 1,536 | 1,536 |
| — text + metadata | 632 | 544 |

The vector is a hard floor: `384 × 4 B = 1,536 B/chunk`, full-precision float32
(`FixedSizeList<Float32, 384>`), incompressible — product quantization lives in
the *index*, not the data. Text is bounded (chunks are split at ~1,200 chars) and
the ~13 provenance columns are small, so text + metadata lands at ~0.5–0.6 KB
regardless of session type. The two memories agree within ~4%.

**→ budget ~2.1 KB/chunk of data**, ~73% of it the embedding.

### Index — sublinear in chunks

| | Cloud dev (5.8k) | Main dev (65k) |
|---|---|---|
| live index B/chunk | **383** | **184** |
| — BM25 | 286 | 150 |
| — vector (IVF_PQ) | 97 | 34 |

Both index types get *cheaper per chunk* as the memory grows:

- **BM25** follows Heaps' law — the vocabulary saturates, so the inverted index
  grows sublinearly. Per-chunk cost roughly halved from 5.8k → 65k chunks.
- **Vector (IVF_PQ)** is ~24 B/vector of PQ codes plus fixed IVF codebook
  overhead; that fixed cost amortizes away at scale, trending toward ~30 B/chunk.

**→ budget ~0.18 KB/chunk of index at scale** (the 0.38 KB from the small memory
is small-N overhead, not the trend).


### Total

A memory costs roughly **~2.3 KB/chunk** at scale — ≈92% data, ≈8% index, and the
embedding alone is most of the data.

## Projection

Size, using ~2.1 KB/chunk data and ~0.18 KB/chunk index:

| Chunks | Data | Index | Total |
|---|---|---|---|
| 10k | 21 MB | 2 MB | 23 MB |
| 65k (Main dev today) | 137 MB | 12 MB | 149 MB |
| ~94k (remote today) | 197 MB | 17 MB | ~214 MB |
| 250k | 525 MB | 45 MB | 570 MB |
| 500k | 1.05 GB | 90 MB | 1.14 GB |
| 1M | 2.1 GB | 180 MB | 2.3 GB |

**Rate.** At ~200 chunks/session and heavy-development cadence (~2,700
chunks/active-day, measured), a memory accretes on the order of **~6 MB/day**,
almost all of it data. Reaching 1M chunks (~2.3 GB) is roughly a year of
sustained heavy daily use, or several years of moderate use.

## Maintenance

Re-indexing commits a new version rather than overwriting, so superseded index
generations — and, after row-rewriting ops like `scrub`, orphaned data
fragments — would otherwise accumulate. funes reaps them after each local re-index
(past a short grace window, so a concurrent read isn't cut off), keeping on-disk
size at the figures above. The remote push path optimizes indexes incrementally
but does not yet reap, so a published memory runs somewhat above these figures.

## Fit against HF Hub storage

A published memory is an HF Hub dataset repo, so it counts against the account's
Hub storage quota. The private-storage limits are 100 GB (free) and 1 TB (PRO);
public repos have separate limits. See
[HF storage limits](https://huggingface.co/docs/hub/storage-limits).

At ~2.1 KB/chunk, 100 GB corresponds to tens of millions of chunks (~45M); the
current remote (93,684 chunks, ~214 MB) uses ~0.2% of it. At the measured
~6 MB/day rate that is on the order of decades. Storage is not the binding
constraint on this workload.
