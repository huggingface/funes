# Platform constraints

Facts about the Hugging Face Hub and Lance that any funes built on them would face, whatever its
code. Each one comes with its source: the Hub's documentation, or a line of a dependency's pinned
source.

**Checked**: 2026-11-19, against the versions Cargo.lock pins (lance, lance-index, lance-table and
lance-io 13.0.0, object_store 0.14.2, opendal-service-hf 0.59.5, hf-hub 1.0.0) and the Hub
documentation as published that day. A constraint can change with any of them. Source paths are
relative to each crate's root in the cargo registry.

## The Hub

| ID | Constraint | Source |
| --- | --- | --- |
| H1 | A folder holds at most 10k files. | [Storage limits](https://huggingface.co/docs/hub/storage-limits), Explanations: "The maximum number of files per folder cannot exceed 10k files per folder." |
| H2 | A repo should hold fewer than 100k files. | [Storage limits](https://huggingface.co/docs/hub/storage-limits), Recommendations: "Files per repo <100k" (advice, not a limit). |
| H3 | A repo degrades after a few thousand commits. Squashing its history is the remedy, and it can't be undone. | [Storage limits](https://huggingface.co/docs/hub/storage-limits), Explanations: "the user experience on the Hub starts to degrade after a few thousand commits", and `super_squash_history` "is a non-revertible operation." |
| H4 | No hard limit on files per commit, but the server checks each file within a 60 s request timeout. The Hub advises about 50 to 100 files per commit. | [Storage limits](https://huggingface.co/docs/hub/storage-limits), Explanations: "a timeout of 60s is set on the request", "keep around 50-100 files per commit." |
| H5 | A commit carries at most 25k LFS files and a 1 GB payload of regular files. | [`create_commit`](https://huggingface.co/docs/huggingface_hub/en/package_reference/hf_api#huggingface_hub.HfApi.create_commit): "`create_commit` is limited to 25k LFS files and a 1GB payload for regular files." |
| H6 | A commit guarded by a parent fails if the branch no longer points to that parent. | [`create_commit`](https://huggingface.co/docs/huggingface_hub/en/package_reference/hf_api#huggingface_hub.HfApi.create_commit): "the commit will fail if `revision` does not point to `parent_commit`." hf-hub 1.0.0 `src/repository/upload.rs:733`. |
| H7 | A commit can complete on the server after its request timed out on the client. | [Storage limits](https://huggingface.co/docs/hub/storage-limits), Explanations: "even if the timeout is raised client-side, the process is still completed server-side." |
| H8 | A file deleted by a commit stays readable at the older commits, until the history is rewritten or squashed (H9). | [Removing sensitive data](https://huggingface.co/docs/hub/security-removing-sensitive-data): "older commits still contain it, and URLs such as `/resolve/<old_sha>/<path>` […] keep serving it." |
| H9 | Deleting a file frees no storage. Rewriting or squashing the history does, and the old commits then stop being readable, usually within an hour. | [Storage limits](https://huggingface.co/docs/hub/storage-limits): "Deleting only LFS pointers doesn't free up space." [Removing sensitive data](https://huggingface.co/docs/hub/security-removing-sensitive-data), When old commits disappear: "within about an hour after a force-push, a `super_squash_history`". |
| H10 | Every change to a repo is a new commit, and commits accumulate. Only rewriting or squashing the history removes them. | [Storage limits](https://huggingface.co/docs/hub/storage-limits), Explanations: "Under the hood, the Hub uses Git to version the data", "squash all the commits to get a fresh start". [Removing sensitive data](https://huggingface.co/docs/hub/security-removing-sensitive-data), Overview: deleting a file "in a new commit" leaves "All previous commits". |
| H11 | A squash replaces the history of one branch with a single commit, made from the branch's head. It takes no parent commit, so nothing guards it against a commit that lands meanwhile, and the docs don't say what happens to such a commit. It can't be undone. | [`super_squash_history`](https://huggingface.co/docs/huggingface_hub/en/package_reference/hf_api#huggingface_hub.HfApi.super_squash_history): "Squashing commits can only be performed from the head of a branch", a signature without `parent_commit`, "Once squashed, the commit history cannot be retrieved. This is a non-revertible operation." [Removing sensitive data](https://huggingface.co/docs/hub/security-removing-sensitive-data), Overview: `super_squash_history` removes "The history of one branch". |
| H12 | The pinned Hub client has no call to squash a history. | hf-hub 1.0.0 `src/` (no squash operation) |

## Lance

| ID | Constraint | Source |
| --- | --- | --- |
| L1 | Every commit writes a version file (in `_versions/`) and a transaction file (in `_transactions/`). Cleanup removes both when it removes the version. | lance-table `src/io/commit.rs:70`, lance `src/io/commit.rs:384`, lance `src/dataset/cleanup.rs:672-676`, `:1158-1172` |
| L2 | A write puts its data and index files in storage before the version that references them. Until that version exists, nothing tells them apart from the leftovers of an abandoned write. | lance `src/dataset/cleanup.rs:19-33` |
| L3 | Cleanup removes versions by age. It always keeps the latest version and tagged ones, and it knows nothing of readers. | lance `src/dataset/cleanup.rs:598-603`, `:1474-1486` |
| L4 | Cleanup keeps a file no version references until the file is 7 days old, unless the caller asks it to delete unverified files and guarantees that no write is running. | lance `src/dataset/cleanup.rs:27-33`, `:362` |
| L5 | Cleanup carries on past a failed delete. It counts the failure in `failed_deletes` and returns success. | lance `src/dataset/cleanup.rs:879-890` |
| L6 | A tagged version survives cleanup at any age. | lance `src/dataset/cleanup.rs:598-603` |
| L7 | Compaction only merges neighboring fragments smaller than the target size that the same index segments cover. So it never merges an indexed fragment with an unindexed one, nor a fragment covered by a delta segment with one covered by another segment. It leaves a lone small fragment alone. | lance `src/dataset/optimize.rs:790-797`, `:882-890`, `:911-917`, `:940`, `:964-966`, `:2190-2199`, `:2259-2278` |
| L8 | Compaction commits its new fragments and the indexes remapped onto them in one transaction. Remapping writes each index segment that covered a merged fragment anew, under a new id. A segment Lance can't open is dropped instead. | lance `src/dataset/optimize.rs:3583-3587`, lance `src/index.rs:1333`, `:1399-1420` (rewrite), `:1403-1412` (drop) |
| L9 | A local commit publishes its version by renaming a temporary file into place, only if that version doesn't exist yet. A commit that loses the race is checked against the commits made since it read, and committed after them if they don't conflict. An append doesn't conflict with a compaction. | lance-table `src/io/commit.rs:1544-1576`, lance `src/io/commit/conflict_resolver.rs:414`, `:1244` (rewrite), `:1585` (append) |
| L10 | Vector search works without a vector index, by scanning. With one, it also scans the rows the index doesn't cover. | lance `src/dataset/scanner.rs:6362`, `:6646-6660` |
| L11 | Keyword search works without a text index when the query names its columns, by scanning. A query that names no column takes its columns from the text indexes, and fails when there is none. With an index, the search also scans the rows the index doesn't cover. | lance `src/dataset/tests/dataset_index.rs:4227` (no index), `:3974` (rows outside the index), lance-index `src/scalar/inverted/query.rs:1164-1180` (no column, no index) |
| L12 | A PQ vector index needs at least 2^bits training vectors, so 256 at 8 bits. | lance-index `src/vector/pq/builder.rs:227-232` |
| L13 | Each index update adds a delta segment, until the deltas are merged. Each text index partition writes three files, plus metadata. | lance-index `src/optimize.rs:15`, lance `src/index/append.rs:3560`, lance-index `src/scalar/inverted/builder.rs:2184-2203` |
| L14 | On a local disk, Lance doesn't flush what it writes to the disk (no fsync). After a power loss, the operating system may have lost a commit Lance reported. | lance-io `src/object_store/providers/local.rs:121-124` (`LocalFileSystem::new()`), object_store 0.14.2 `src/local.rs:274` (`fsync: false`), lance-io `src/object_writer.rs:695-720` |
| L15 | Lance's `hf://` store commits each file it writes as its own Hub commit. A version written through it is never a single commit. | opendal-service-hf 0.59.5 `src/writer.rs:160`, used by lance-io `src/object_store/providers/huggingface.rs` |
| L16 | A write adds its rows as new fragments. It never adds rows to an existing fragment. | lance-table `src/transaction/operation.rs:40-42` |
| L17 | A merge writes its rows into as many fragments as they fill at the target size, rounded down, and at least one. Every fragment it reads is smaller than the target (L7), so merging two or more fragments always writes fewer fragments than it reads. | lance `src/dataset/optimize.rs:2203-2225` (merge size), `:2580-2618` (fragments written) |

## Unverified

Hub behavior that no documentation backs yet.

| ID | Behavior | Evidence so far |
| --- | --- | --- |
| U1 | The Hub refuses a commit with 409 "Another commit operation is in progress" while it processes another commit on the repo. Such a commit did not apply. | Observed by funes (`src/memory/remote.rs:756-759`). |
| U2 | The Hub answers a stale parent with 412 Precondition Failed. | Observed by funes (`src/memory/remote.rs:760-770`). |
| U3 | A Hub commit applies all its operations or none. | The Hub runs on Git ([Storage limits](https://huggingface.co/docs/hub/storage-limits): "Under the hood, the Hub uses Git to version the data"). The commit API's documentation doesn't say it. |
| U4 | The commit a squash leaves holds the files the branch had at its head. | Implied by "Squashing commits can only be performed from the head of a branch" and by the advice to delete a file, then squash ([Removing sensitive data](https://huggingface.co/docs/hub/security-removing-sensitive-data)). The docs don't state it. |
