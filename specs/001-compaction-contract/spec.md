# Feature Specification: Compaction contract

**Feature Branch**: `spec/compaction`

**Created**: 2026-11-17

**Status**: Draft, rewritten on 2026-11-19 from the owner's decisions. Implementation conformance
is not established.

**Input**: User description: "I want to establish a more formal specification for compaction."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Keep a growing memory usable without losing anything (Priority: P1)

As a memory owner, I want my memory to stay under the Hub's limit of 10k files per folder, and
recall to stay fast, as I keep adding sessions, without any record being lost or changed, even
when a compaction is interrupted.

**Why this priority**: This is why compaction exists. Without it, a memory grows until the Hub
refuses it, and recall slows down as its files pile up.

**Independent Test**: Write many small batches of records to a memory, compact it, and compare.
The regrouped data sits in fewer files, every record is unchanged and in its place in its
conversation, and every record is still searchable. Repeat with the compaction interrupted at
various points: every record is still intact.

**Acceptance Scenarios**:

1. **Given** a memory that received many small writes, **When** compaction runs, **Then** the
   regrouped data ends up in fewer files, and every record keeps its content and its place in
   its conversation (FR-002, FR-003).
2. **Given** data that search is not up to date with, and more than 256 small pieces of data
   that search is up to date with, **When** compaction runs, **Then** it regroups both, as far
   as the platform constraints allow (FR-013).
3. **Given** records that search is not up to date with when a compaction starts, **When** the
   compaction runs, **Then** search is brought up to date with them, and every record stays
   searchable (FR-001, FR-009).
4. **Given** stored data that nothing needs anymore, **When** compaction runs, **Then** that
   data is removed (FR-001).
5. **Given** a memory with nothing to regroup, update or remove, **When** compaction runs,
   **Then** it reports that it had nothing to do, and succeeds (FR-008).
6. **Given** a compaction in progress, **When** the machine crashes, loses power, the process is
   killed or the network fails, at any point, **Then** every record is intact, and the
   compaction undoes nothing it achieved before (FR-006, FR-008).
7. **Given** a remote memory that keeps receiving pushes, **When** compactions run, **Then** the
   history it keeps on the Hub does not keep growing (FR-015).
8. **Given** the owner's local memory, a local memory at another path, or a remote memory,
   **When** the owner runs `funes compact` naming it, **Then** that memory is compacted (FR-016).

---

### User Story 2 - Keep working while compaction runs (Priority: P2)

As a memory owner, I want to keep pushing, indexing and recalling while a compaction runs, on
this machine or another one, without waiting for it and without losing anything.

**Why this priority**: Compaction runs on its own after writes (FR-007), so other writes and
reads happen while it runs.

**Independent Test**: Push to a remote memory, index into a local one, and read from both while
each one compacts. No write waits for the compaction or fails because of it, no read breaks,
and every record is kept.

**Acceptance Scenarios**:

1. **Given** a compaction running on a remote memory, **When** another machine pushes,
   **Then** the push completes without waiting for the compaction, and its records are kept
   (FR-004).
2. **Given** a compaction running on a local memory, **When** an index run starts, **Then** it
   proceeds without waiting for the compaction, and its records are kept (FR-004).
3. **Given** a read in progress, **When** a compaction runs and finishes, **Then** the read
   completes normally (FR-005).
4. **Given** records written while a compaction runs, **When** the compaction ends, **Then**
   those records are searchable, even if search is brought up to date with them only by a
   later compaction (FR-009).
5. **Given** two compactions of the same remote memory, **When** they collide, **Then**
   `funes compact` retries by itself a limited number of times, then reports a conflict, and no
   record is lost (FR-014).

---

### User Story 3 - Compaction takes care of itself and says how it went (Priority: P3)

As a memory owner, I want funes to compact my memory on its own and to say when it does. When
something doesn't finish, I want to be told on every run and from any machine, with exit
statuses that scripts can rely on.

**Why this priority**: Automatic compaction must not be hidden, and failures that repeat must
not go unnoticed.

**Independent Test**: Run pushes and index runs until compaction starts on its own, then make
parts of a compaction fail. Check what each command says, its exit status, and what another
machine sees.

**Acceptance Scenarios**:

1. **Given** an index run, or a push after which enough new data has piled up since the last
   compaction, **When** it finishes writing, **Then** it compacts the memory and says that it
   does. A push that leaves less does not compact (FR-007).
2. **Given** a compaction whose work partly fails, **When** the command ends, **Then** it says
   what didn't finish, and the problem stays visible, from every machine that uses the memory,
   until a later compaction fixes it (FR-010).
3. **Given** a part of a compaction that fails, **When** the compaction ends, **Then** it still
   completes and reports the problem, and `funes compact` does not exit with an error for it
   (FR-010, FR-011).
4. **Given** `funes compact`, **When** its compaction ends in a conflict or its outcome is
   unknown, **Then** it exits with an error (FR-011).
5. **Given** `funes index` or `funes push`, with or without `--compact`, whose records are saved,
   **When** part of its compaction fails, the compaction ends in a conflict, or its outcome is
   unknown, **Then** the command succeeds and reports the compaction's outcome separately
   (FR-011).
6. **Given** funes cannot tell whether a compaction took effect, **When** it reports, **Then**
   it says the outcome is unknown, never a success or a conflict (FR-012).

---

### Edge Cases

- A compaction is interrupted at any point by a crash, a power loss, the process being killed
  or a network failure (FR-006, FR-008).
- A compaction has nothing to do (FR-008).
- A compaction regroups nothing, only brings search up to date, and ends with more files than
  before (FR-002).
- Writes and reads happen while a compaction runs, on the same machine or another one (FR-004,
  FR-005).
- Two compactions of the same remote memory collide (FR-014).
- Part of a compaction fails, once or on several runs in a row (FR-010).
- Funes cannot tell whether a compaction took effect (FR-012).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Compaction MUST regroup a memory's stored data, bring search up to date with the
  records the memory holds when the compaction starts, and remove stored data that nothing needs
  anymore.
- **FR-002**: When compaction regroups data, the regrouped data MUST end up in fewer files than
  before. A compaction that regroups nothing MAY leave more files than before.
- **FR-003**: Compaction MUST NOT lose or change any record, nor change its place in its
  conversation, nor change how many copies of it the memory holds.
- **FR-004**: Records written while a compaction runs, by pushes to a remote memory or index
  runs on a local one, MUST NOT be lost. These writes MUST NOT be refused, nor made to wait for
  the compaction to end.
- **FR-005**: Compaction MUST NOT break a read in progress.
- **FR-006**: A crash, a power loss, the process being killed or a network failure, at any point
  of a compaction, MUST leave every record intact.
- **FR-007**: Funes MUST compact a memory on its own after index runs, and after a push once
  enough new data has piled up since the remote memory's last compaction. The design sets how
  much. Funes MUST say when it compacts.
- **FR-008**: A compaction that fails or is interrupted MUST NOT undo what it already achieved.
  A compaction with nothing to do MUST succeed.
- **FR-009**: After a compaction, every record MUST stay searchable under the same search and
  filter rules as before, including records written while it ran. Which records a search
  returns first, and their scores, MAY change.
- **FR-010**: When part of a compaction fails, the compaction MUST still complete, and funes MUST
  say so on every run where it fails. The problem MUST stay visible, from every machine that uses
  the memory, until a later compaction fixes it.
- **FR-011**: `funes index` and `funes push`, with or without `--compact`, MUST succeed once their
  records are saved, whatever happens to their compaction, and MUST report the compaction's
  outcome separately. `funes compact` MUST exit with an error when its compaction ends in a
  conflict or its outcome is unknown.
- **FR-012**: When funes cannot tell whether a compaction took effect, it MUST report the
  outcome as unknown, never as a success or a conflict. Whether and how funes tries to find out
  is decided once for all writes to a remote memory, not by compaction.
- **FR-013**: Every compaction MUST regroup the data that search is not up to date with when it
  starts. When a memory holds more than 256 small pieces of data that search is up to date with,
  its next compaction MUST regroup those too. Both go as far as the platform constraints allow,
  which can leave nothing to regroup. The design sets what counts as small.
- **FR-014**: When another compaction of the same remote memory collides with `funes compact`,
  it MUST retry by itself a limited number of times, then report a conflict.
- **FR-015**: The history a remote memory keeps on the Hub MUST NOT keep growing.
- **FR-016**: `funes compact` MUST compact the memory the owner names: the owner's local memory
  by default, a local memory at any path, or a remote memory.

### Key Entities

- **Memory**: a store of records. A local memory lives on one machine. A remote memory is a
  Hugging Face Hub repository that several machines push to and read from.
- **Record**: a stored passage of a session, with its content, its metadata, its embedding and
  its place in its conversation.
- **Write**: a push to a remote memory, or an index run on a local one, that adds records.
- **Read**: a recall, or any other command that reads a memory.
- **Compaction**: the maintenance a memory goes through, on its own after writes, or on demand
  with `funes compact`.
- **Problem report**: what funes shows when part of a compaction failed. It stays visible until
  a later compaction fixes the problem.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Comparing a memory's records before and after compaction finds zero records lost
  or changed, zero changes of place in a conversation, and zero changes in how many copies of a
  record the memory holds. This holds when the compaction is interrupted at any point by a
  crash, a power loss, the process being killed or a network failure (FR-003, FR-006).
- **SC-002**: Every compaction that regroups data leaves the regrouped data in fewer files than
  before (FR-002).
- **SC-003**: Every compaction regroups the data that search is not up to date with when it
  starts, and every compaction that starts with more than 256 small pieces of data that search
  is up to date with regroups those too, as far as the platform constraints allow (FR-013).
- **SC-004**: With pushes and index runs made during compactions, zero records are lost, zero
  writes are refused, and zero writes wait for a compaction to end (FR-004).
- **SC-005**: With reads made during compactions, zero reads break because of a compaction
  (FR-005).
- **SC-006**: After every compaction, every record stays searchable under the same rules
  (FR-009).
- **SC-007**: After a compaction fails or is interrupted, zero completed work is undone, and a
  compaction with nothing to do reports success (FR-008).
- **SC-008**: Every index run, and every push after which enough new data has piled up since the
  last compaction, compacts the memory and says so. No other push compacts (FR-007).
- **SC-009**: A part of a compaction that fails on three runs in a row is reported on all
  three, and stays visible from another machine until a later compaction fixes it (FR-010).
- **SC-010**: Each command returns the exit status FR-011 sets, for each outcome: success,
  nothing to do, a failed part, conflict, and unknown (FR-010, FR-011, FR-012).
- **SC-011**: Under repeated collisions with another compaction, `funes compact` retries at
  least once, stops at its limit, and reports a conflict, with zero records lost (FR-014).
- **SC-012**: As pushes and compactions accumulate on a remote memory, the history it keeps on
  the Hub does not keep growing (FR-015).
- **SC-013**: `funes compact` compacts the local memory with no argument, and the named memory
  when given a local path or a remote memory (FR-016).

## Assumptions

- A compaction starts from a valid, fully saved memory. Recovering a damaged memory is outside
  this specification.
- Memories are stored with Lance, and remote memories on the Hugging Face Hub. The platform
  constraints that bound how these requirements can be met are listed in
  [constraints.md](constraints.md), for the design.

## Decision Record

Each requirement comes from a decision of the owner. Quotes are the owner's words, with typos
fixed. "Selected" marks the text of an option the owner picked, which is the agent's wording.
The quotes come from the owner's messages in these agent sessions: `38995acb` (2026-10-05),
`0dfec655` (2026-10-06 and 2026-10-07), `01a1eaa1` (2026-11-17), `01a1eae8` (2026-11-17 and
2026-11-18), `01a1ee59` and `01a1eef6` (2026-11-18), `58330009` (2026-11-19), and `bf088dba`
(2026-11-20).

| Requirement | Owner's words | Date |
| --- | --- | --- |
| User Story 1 (purpose) | "we need something to clean up existing memories with 2k+ files", "because the hard limit is STILL 10k", "not compacting leads to much longer recall time" | 2026-10-05 |
| FR-001 | "Compaction is an optimization of existing records storage, without modifying their order", "funes compaction is the three Lance operations" (2026-11-17), then selected "Cover the starting snapshot. Later additions remain searchable and may be indexed by a later run." (2026-11-18) | 2026-11-17, 2026-11-18 |
| FR-002 | "the only requirement I would impose is that compaction leads to less files" (2026-11-18), "The requirement is that the compaction part of the funes compact operation returns less files" (2026-11-19) | 2026-11-18, 2026-11-19 |
| FR-003 | "just the position in the conversation", then "yes, but I thought it was already settled" to "Preserve every committed record unchanged", which included "the number of copies of each record" | 2026-11-17 |
| FR-004 | Selected "Allow concurrent pushes" (2026-11-18), then selected "The index run proceeds, as a push does on a remote" (2026-11-19). Asked again with the 2026-10-07 choice in view (the writers of a local memory never write at the same time), selected "The index run proceeds" (2026-11-20). | 2026-11-18, 2026-11-19, 2026-11-20 |
| FR-005 | Selected "Preserve the files until the read finishes" | 2026-11-18 |
| FR-006 | "This is not a matter of external visibility. I am only worried about integrity", "machine crashes or power loss too" | 2026-11-17 |
| FR-007 | Approved compaction on push and at the end of `funes index` (2026-10-05), then "I am not arguing about the feature, I am arguing about it being silent / hidden" (2026-10-06). For pushes, selected "A push compacts its remote memory only once enough new data has piled up since the last compaction. The design sets how much." (2026-11-20) | 2026-10-05, 2026-10-06, 2026-11-20 |
| FR-008 | "We never undo. No work succeeds" | 2026-11-18 |
| FR-009 | "A. This is true for any index update", then selected "Cover the starting snapshot. Later additions remain searchable and may be indexed by a later run." | 2026-11-18 |
| FR-010 | "Worried that repeated cleanup failures get unnoticed", then selected visibility between runs and "Every host" (2026-11-18). For search updates, selected "Treat both alike: a failed update leaves a visible finding, and the compaction still completes": "That distinction was purely arbitrary AFAICT" (2026-11-19). Asked for one rule on every failed part: "the fact that we already have a difference in behaviour when cleanup / search up to date fail is not a good sign", then "Align on push then" (2026-11-20). The decisions name cleanup and search updates. Extending the reporting to every part of compaction follows from the same concern. | 2026-11-18, 2026-11-19, 2026-11-20 |
| FR-011 | "Yes" to a push staying successful once its records are saved (2026-11-18). Selected "Nonzero, like an incomplete compaction or a conflict" when `funes compact` can't tell whether its change reached the Hub (2026-11-19). "Align on push then" (2026-11-20), read as extending the push rule to `funes index` and `funes push --compact`, which replaces the nonzero exit selected for them on 2026-11-18. | 2026-11-18, 2026-11-19, 2026-11-20 |
| FR-012 | Selected "Leave confirmation to a shared policy. Compaction must protect potentially live files and report an unknown outcome when success cannot be established." | 2026-11-18 |
| FR-013 | Approved the 256 limit (2026-10-05), then "When a compaction has nothing to merge because of Lance constraints, then the funes compact operation cannot result in less files", and selected "fragments no index covers are merged at every compaction, and small fragments the indexes cover are merged once there are more than 256 of them": "we need a limit, and because timings are too dependent on the dataset and host" (2026-11-19). "No index covers" is written as "search is not up to date with". | 2026-10-05, 2026-11-19 |
| FR-014 | Selected "Retry automatically, with a limit. Report a conflict when the limit is reached." | 2026-11-18 |
| FR-015 | "I think squashing is part of compaction too", restated as what squashing achieves. How belongs to the design. | 2026-11-19 |
| FR-016 | "ok, so you remove the local path restriction then?", then "You're so non-cooperative. Think as a user for once, not as a coder", then "go on with all commits" once `funes compact` compacted whatever memory is named | 2026-10-06 |
| Out of scope: recovery | "this goes beyond compaction" | 2026-11-17 |
