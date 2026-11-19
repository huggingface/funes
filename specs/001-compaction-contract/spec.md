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
various points.

**Acceptance Scenarios**:

1. **Given** a memory that received many small writes, **When** compaction runs, **Then** the
   regrouped data ends up in fewer files, and every record keeps its content and its place in
   its conversation (FR-002, FR-003).
2. **Given** data written since the last compaction, and a memory holding more than 256 small
   pieces of older data, **When** compaction runs, **Then** it regroups both (FR-013).
3. **Given** records added since the last compaction, **When** compaction runs, **Then** search
   is brought up to date with them, and every record stays searchable (FR-001, FR-009).
4. **Given** stored data that nothing needs anymore, **When** compaction runs, **Then** that
   data is removed (FR-001).
5. **Given** a memory with nothing to regroup, update or remove, **When** compaction runs,
   **Then** it reports that it had nothing to do, and succeeds (FR-008).
6. **Given** a compaction in progress, **When** the machine crashes, loses power, the process is
   killed or the network fails, at any point, **Then** every record is intact, the memory is
   readable and searchable, and what the compaction achieved before stays in place (FR-006,
   FR-008).
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

1. **Given** a push or an index run that leaves work for compaction, **When** it finishes
   writing, **Then** it compacts the memory and says that it does (FR-007).
2. **Given** a compaction whose work partly fails, **When** the command ends, **Then** it says
   what didn't finish, and the problem stays visible, from every machine that uses the memory,
   until a later compaction fixes it (FR-010).
3. **Given** a failure that leaves search working, **When** the compaction ends, **Then** the
   compaction completes and reports the problem (FR-010).
4. **Given** `funes compact`, `funes index` or `funes push --compact`, **When** compaction is
   unfinished, ends in a conflict, or its outcome is unknown, **Then** the command exits with an
   error. `funes index` and `funes push --compact` also say that their records were saved
   (FR-011).
5. **Given** a `funes push` without `--compact` whose records are saved, **When** its compaction
   fails, **Then** the push succeeds and reports the compaction's outcome separately (FR-011).
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
  records added since the last compaction, and remove stored data that nothing needs anymore.
- **FR-002**: When compaction regroups data, the regrouped data MUST end up in fewer files than
  before. A compaction that regroups nothing MAY leave more files than before.
- **FR-003**: Compaction MUST NOT lose, change or duplicate any record, nor change a record's
  place in its conversation.
- **FR-004**: Records written while a compaction runs, by pushes to a remote memory or index
  runs on a local one, MUST NOT be lost. These writes MUST NOT be refused, nor made to wait for
  the compaction to end.
- **FR-005**: Compaction MUST NOT break a read in progress.
- **FR-006**: A crash, a power loss, the process being killed or a network failure, at any point
  of a compaction, MUST leave every record intact, and the memory readable and searchable.
- **FR-007**: Funes MUST compact a memory on its own after pushes and index runs, and MUST say
  when it does.
- **FR-008**: A compaction that fails or is interrupted MUST NOT undo what it already achieved.
  A compaction with nothing to do MUST succeed.
- **FR-009**: After a compaction, every record MUST stay searchable under the same search and
  filter rules as before, including records written while it ran. Which records a search
  returns first, and their scores, MAY change.
- **FR-010**: When part of a compaction fails, funes MUST say so on every run where it fails.
  The problem MUST stay visible, from every machine that uses the memory, until a later
  compaction fixes it. A failure that leaves search working MUST NOT make the compaction fail.
- **FR-011**: `funes compact`, `funes index` and `funes push --compact` MUST exit with an error
  when compaction is unfinished, ends in a conflict, or its outcome is unknown. `funes index`
  and `funes push --compact` MUST also say that their records were saved. `funes push` without
  `--compact` MUST succeed once its records are saved, whatever happens to its compaction, and
  MUST report the compaction's outcome separately.
- **FR-012**: When funes cannot tell whether a compaction took effect, it MUST report the
  outcome as unknown, never as a success or a conflict. Whether and how funes tries to find out
  is decided once for all writes to a remote memory, not by compaction.
- **FR-013**: Every compaction MUST regroup the data written since the previous one. When a
  memory holds more than 256 small pieces of older data, its next compaction MUST regroup those
  too. The design sets what counts as small.
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

- **SC-001**: Comparing a memory's records before and after compaction finds zero records lost,
  changed or duplicated, and zero changes of place in a conversation. This holds when the
  compaction is interrupted at any point by a crash, a power loss, the process being killed or
  a network failure (FR-003, FR-006).
- **SC-002**: Every compaction that regroups data leaves the regrouped data in fewer files than
  before (FR-002).
- **SC-003**: Every compaction regroups the data written since the previous one, and every
  compaction that starts with more than 256 small pieces of older data regroups those too
  (FR-013).
- **SC-004**: With pushes and index runs made during compactions, zero records are lost, zero
  writes are refused, and zero writes wait for a compaction to end (FR-004).
- **SC-005**: With reads made during compactions, zero reads break because of a compaction
  (FR-005).
- **SC-006**: After every compaction, every record stays searchable under the same rules
  (FR-009).
- **SC-007**: After a compaction fails or is interrupted, zero completed work is undone, and a
  compaction with nothing to do reports success (FR-008).
- **SC-008**: Every push and index run that leaves work for compaction compacts the memory and
  says so (FR-007).
- **SC-009**: A part of a compaction that fails on three runs in a row is reported on all
  three, and stays visible from another machine until a later compaction fixes it (FR-010).
- **SC-010**: Each command returns the exit status FR-011 sets, for each outcome: success,
  nothing to do, unfinished, conflict, and unknown (FR-011, FR-012).
- **SC-011**: Under repeated collisions with another compaction, `funes compact` retries at
  least once, stops at its limit, and reports a conflict, with zero records lost (FR-014).
- **SC-012**: As pushes and compactions accumulate on a remote memory, the history it keeps on
  the Hub stays under a fixed size (FR-015).
- **SC-013**: `funes compact` compacts the local memory with no argument, and the named memory
  when given a local path or a remote memory (FR-016).

## Assumptions

- A compaction starts from a valid, fully saved memory. Recovering a damaged memory is outside
  this specification.
- Memories are stored with Lance, and remote memories on the Hugging Face Hub. The platform
  constraints that bound how these requirements can be met are listed in
  [constraints.md](constraints.md), for the design.

## Decision Record

Each requirement comes from a decision of the owner. Quotes are the owner's words, with typos fixed. The full question-and-answer log of the
sessions that produced them is in [spec-before-rewrite.md](spec-before-rewrite.md).

| Requirement | Owner's words | Date |
| --- | --- | --- |
| User Story 1 (purpose) | "we need something to clean up existing memories with 2k+ files", "because the hard limit is STILL 10k", "not compacting leads to much longer recall time" | 2026-10-05 |
| FR-001 | "Compaction is an optimization of existing records storage, without modifying their order", "funes compaction is the three Lance operations" | 2026-11-17 |
| FR-002 | "the only requirement I would impose is that compaction leads to less files" (2026-11-18), "The requirement is that the compaction part of the funes compact operation returns less files" (2026-11-19) | 2026-11-18, 2026-11-19 |
| FR-003 | "just the position in the conversation", then "yes" to keeping every saved record unchanged, including its number of copies | 2026-11-17 |
| FR-004 | Selected "Allow concurrent pushes" (2026-11-18), then "The index run proceeds, as a push does on a remote" (2026-11-19) | 2026-11-18, 2026-11-19 |
| FR-005 | Selected "Preserve the files until the read finishes" | 2026-11-18 |
| FR-006 | "I am only worried about integrity", "machine crashes or power loss too". Readable and searchable is derived from integrity. | 2026-11-17 |
| FR-007 | Approved compaction on push and at the end of `funes index` (2026-10-05), then "I am not arguing about the feature, I am arguing about it being silent / hidden" (2026-10-06) | 2026-10-05, 2026-10-06 |
| FR-008 | "We never undo. No work succeeds" | 2026-11-18 |
| FR-009 | "A. This is true for any index update", then selected "Later additions remain searchable and may be indexed by a later run" | 2026-11-18 |
| FR-010 | "Worried that repeated cleanup failures get unnoticed", selected visibility between runs, "Every host" (2026-11-18), then both search indexes treated alike: "That distinction was purely arbitrary AFAICT" (2026-11-19). The decisions name cleanup and search updates. Extending them to every part of compaction follows from the same concern. | 2026-11-18, 2026-11-19 |
| FR-011 | "Yes" to a push staying successful once its records are saved, selected nonzero for both `funes index` and `funes push --compact`, and incomplete `funes compact` exiting nonzero (2026-11-18), then nonzero for an unknown outcome (2026-11-19). A conflict exiting with an error follows from FR-014. | 2026-11-18, 2026-11-19 |
| FR-012 | Selected "Leave confirmation to a shared policy. Compaction must protect potentially live files and report an unknown outcome when success cannot be established." | 2026-11-18 |
| FR-013 | Approved the 256 limit (2026-10-05), then selected "fragments no index covers are merged at every compaction, and small fragments the indexes cover are merged once there are more than 256 of them": "we need a limit, and because timings are too dependent on the dataset and host" (2026-11-19) | 2026-10-05, 2026-11-19 |
| FR-014 | Selected "Retry automatically, with a limit. Report a conflict when the limit is reached." | 2026-11-18 |
| FR-015 | "I think squashing is part of compaction too", restated as what squashing achieves. How belongs to the design. | 2026-11-19 |
| FR-016 | "ok, so you remove the local path restriction then?", then "You're so non-cooperative. Think as a user for once, not as a coder", then "go on with all commits" once `funes compact` compacted whatever memory is named | 2026-10-06 |
| Out of scope: recovery | "this goes beyond compaction" | 2026-11-17 |
