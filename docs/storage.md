# Storage growth

How big does a funes memory get, and how does it grow? This document builds a
per-chunk cost model from two real memories and projects it forward.

Figures were measured on 2026-07-22 with funes 1.2.0+dev, the current schema,
and bge-small-en-v1.5 (`DIM = 384`). Sizes use decimal MB/GB and count only
files referenced by the current Lance manifest. They exclude manifest and
transaction history, superseded generations awaiting cleanup, and Hub Git/Xet
history. Coefficients vary with chunk content, vocabulary, schema, and Lance
versions, so projections are planning estimates rather than byte-exact
guarantees.

## Reference memories

A development memory and its curated, published project memory:

| | Local development | Published project |
|---|---|---|
| role | full working history | curated project history |
| chunks | 117,499 | 22,597 |
| live data | 249.9 MB | 46.3 MB |
| data B/chunk | 2,127 | 2,047 |

The two snapshots agree within 4% on data cost. The published snapshot's
current manifest has no index generation attached, so it validates the row
cost but is not used to size indexes below.

## What's on disk

A memory is a single Lance dataset (`chunks.lance`) with these parts:

| Part | Holds | Grows with |
|---|---|---|
| `data/` | rows — text, provenance metadata, and the embedding vector | chunks × content |
| `_indices/` | BM25 + vector index | chunks (sublinear) |
| `_versions/` | one manifest per committed version | number of index/push commits |
| `_transactions/` | transaction records | negligible |

Current local breakdown (`data/` plus the index IDs named by the manifest):

| Component | Size |
|---|---:|
| `data/` (live rows) | 249.9 MB |
| `_indices/` BM25 generation | 17.1 MB |
| `_indices/` IVF_PQ generation | 3.7 MB |
| **live total** | **270.7 MB** |

## The per-chunk model

### Data — a fixed vector floor plus bounded text

| | Local development | Published project |
|---|---:|---:|
| data B/chunk | **2,127** | **2,047** |
| — vector (fixed) | 1,536 | 1,536 |
| — text + metadata | 591 | 511 |

The vector is a hard floor: `384 × 4 B = 1,536 B/chunk`, full-precision float32
(`FixedSizeList<Float32, 384>`), incompressible — product quantization lives in
the *index*, not the data. Text is bounded (chunks are split at ~1,200 chars) and
the provenance columns are small, so text + metadata lands at ~0.5–0.6 KB in
both current snapshots.

**→ budget ~2.13 KB/chunk of data**, roughly 72–75% of it the embedding.

### Index — sublinear in chunks

| | Local development (117.5k) |
|---|---:|
| live index | **20.8 MB** |
| live index B/chunk | **177** |
| — BM25 | 146 |
| — vector (IVF_PQ) | 31 |

Index cost is not as fixed as row cost:

- **BM25** follows Heaps' law — the vocabulary saturates, so the inverted index
  usually grows sublinearly, but its exact size depends on the text and
  vocabulary.
- **Vector (IVF_PQ)** is ~24 B/vector of PQ codes plus fixed IVF codebook
  overhead. The measured 31 B/chunk is close to that floor; small memories pay
  more per chunk because the fixed overhead has not yet amortized.

**→ budget ~0.18 KB/chunk of index at this scale.** Linear projections are
slightly conservative if BM25 continues to grow sublinearly.

### Total

The current samples support a planning budget of **~2.30 KB/chunk** at
six-figure scale — roughly 92% data and 8% index. The embedding alone is about
two-thirds of the live total.

## Projection

Conservative linear sizing, using the measured 2,127 B/chunk of data and
177 B/chunk of index:

| Chunks | Data | Index | Total |
|---:|---:|---:|---:|
| 10k | 21.3 MB | 1.8 MB | 23.0 MB |
| 100k | 212.7 MB | 17.7 MB | 230.4 MB |
| 117,499 (measured local) | 249.9 MB | 20.8 MB | 270.7 MB |
| 250k | 531.8 MB | 44.3 MB | 576.0 MB |
| 500k | 1.06 GB | 88.5 MB | 1.15 GB |
| 1M | 2.13 GB | 177 MB | 2.30 GB |

**Rate.** At ~200 chunks/session and the observed heavy-development cadence of
~2,700 chunks/active-day, a memory accretes about **6.2 MB/day**, almost all of
it data. Reaching 1M chunks (~2.3 GB) is roughly a year of sustained heavy daily
use, or several years of moderate use.

## Maintenance

Re-indexing commits a new version rather than overwriting, so superseded index
generations — and, after row-rewriting ops like `scrub`, orphaned data
fragments — would otherwise accumulate. funes reaps them after each local re-index
(past a short grace window, so a concurrent read isn't cut off), keeping
on-disk size close to the live figures above. The remote push path optimizes
indexes incrementally but does not yet reap, so a published memory can run
above the live-generation estimate.

## Fit against HF Hub storage

A published memory is an HF Hub dataset repo, so it counts against the account's
Hub storage quota. The private-storage limits are 100 GB (free) and 1 TB (PRO);
public repos have separate limits. See
[HF storage limits](https://huggingface.co/docs/hub/storage-limits).

At 2.30 KB/chunk, 100 GB corresponds to about 43 million live chunks before
version-history overhead. At the observed ~6.2 MB/day rate, that reference
point remains decades away. Even allowing for published-memory history, raw
storage capacity is unlikely to be the binding constraint for this kind of
workload.
