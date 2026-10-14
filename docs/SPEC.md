# flux — Implementation Specification

**Status:** Draft v0.1 · **Date:** 2026-10-14
**Scope:** An open-source, horizontally scalable source-control system for very large monorepos and agent-heavy write workloads, inspired by Anthropic's Scion ([Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control)) and Meta's Mononoke/EdenFS, with first-class git interoperability.

---

## 0. How to read this document

- **MUST / SHOULD / MAY** follow RFC 2119.
- Sections 2–5 define the **data model and wire formats**. These are versioned and frozen per major version. Changing them requires a new `*_v2` type.
- Sections 6–12 define **services**. Their internals can change freely as long as they keep the RPC contracts.
- Section 16 defines **phases and acceptance criteria**. Each phase is a shippable vertical slice.

---

## 1. Goals, non-goals, design principles

### 1.1 Goals
1. **Always-online clients.** Clients fetch objects on demand and keep a bounded cache. No client holds full history.
2. **The server owns history.** Log, blame, diff, ancestry, rebase, merge and commit creation are server RPCs.
3. **Full programmatic API.** Every operation the CLI performs is a public gRPC call. CI and agents can work with no local checkout at all.
4. **Horizontal scale.** Stateless servers, immutable content-addressed data and tiered caches. No packfiles and no repository maintenance (`gc`/`repack`).
5. **Cheap, lock-free writes.** Creating a changeset costs O(size of change), not O(size of repo). Moving a ref is a single compare-and-swap.
6. **Sparse and virtual working copies** are the default, not a special mode.
7. **Git interoperability.** Byte-exact import of git history, a git smart-HTTP frontend, and two-way mirroring with GitHub/GitLab. This is the adoption path, and Scion does not have it.

### 1.2 Non-goals (v1)
- Replacing git for small repositories.
- Offline-first or peer-to-peer workflows. A local-only mode exists for development and testing only.
- Shipping a code review UI. flux exposes the hooks that review systems integrate with (§11.4).
- Large binary asset management beyond content-defined chunking (§3.2).

### 1.3 Scale targets (design point)
| Dimension | Target |
|---|---|
| Repo history | 100 M changesets, 1 B distinct file versions, 200 GB+ compressed |
| Working copy (full) | 10 M files, 50 GB |
| Write rate | ≥ 50 landed changesets/s on a single ref; ≥ 5,000 changesets/s created repo-wide |
| Checkouts | ≥ 50 M/day (mostly sparse) |
| Cache hit rate | ≥ 99% at the daemon and regional tiers |

---

## 2. Identifiers and hashing

### 2.1 Hash function
- All object IDs are **BLAKE3-256** using `derive_key` mode with a context string per type. This gives domain separation, so a blob and a tree with identical bytes never collide.

| Type | Context string | Display prefix |
|---|---|---|
| File content | `flux 2026 content v1` | `c_` |
| Chunk | `flux 2026 chunk v1` | `k_` |
| Tree | `flux 2026 tree v1` | `t_` |
| Changeset | `flux 2026 changeset v1` | `cs_` |
| Derived value | `flux 2026 derived v1 <type-name>` | `d_` |

- **Display form:** prefix + lowercase hex (64 chars). Clients MUST accept any unique hex prefix of ≥ 8 chars where the server can resolve it (`ResolvePrefix` RPC).
- **Binary form:** 32 raw bytes on the wire and in storage.

### 2.2 Content IDs are computed over uncompressed bytes
`ContentId = H("flux 2026 content v1", raw_bytes)`. Compression and chunking are storage concerns and never change IDs.

### 2.3 Canonical encoding
Every hashed structure (changeset, tree, derived value) is serialized with **deterministic CBOR** (RFC 8949 §4.2.1: shortest-form integers, definite lengths, map keys sorted bytewise). The ID is the hash of those exact bytes. Crate `flux-core::codec` is the single implementation. Its golden test vectors are checked in under `testdata/codec/` and MUST pass on every platform.

---

## 3. Data model

### 3.1 Paths
- A path is a byte string (not necessarily UTF-8, for git fidelity) split into components by `/`.
- It MUST NOT be empty, start or end with `/`, contain `//`, `\0`, or the components `.`, `..` or `.flux`.
- Max 4,096 bytes per path and 255 bytes per component.
- Ordering is bytewise lexicographic over the full path. Trees sort entries by component name, bytewise.
- **Case-folding collisions** (`A.txt` vs `a.txt`) are allowed in storage. A repo policy flag `reject_case_conflicts` (default `true` for new repos, `false` for git imports) makes changeset creation reject them.

### 3.2 File content (blobs)
- **Inline:** content ≤ 4 MiB is stored as a single object, compressed with zstd level 3 (optionally with a per-repo trained dictionary, §5.3).
- **Chunked:** content > 4 MiB is split with FastCDC (min 512 KiB, avg 1 MiB, max 4 MiB). It is stored as a **chunk manifest** (`ContentId → [ChunkId, offset, len]`) plus chunk objects. The `ContentId` is still the hash of the full content.
- **Metadata per content** (stored in the metadata DB, §5.1): `size`, `sha1` (git blob hash, computed at upload; needed for the git frontend), `sha256`, and `is_binary` (NUL byte in the first 8 KiB).

### 3.3 Changeset (the unit of truth)
A changeset records the *delta against its parents*, not a tree pointer. This is the property that makes creation O(change).

```rust
struct Changeset {
    parents: Vec<ChangesetId>,          // ordered; parents[0] is the "first parent"
    author: String,                      // "Name <email>" (bytes allowed for git imports)
    author_time: Timestamp,              // { unix_ms: i64, tz_offset_min: i16 }
    committer: Option<String>,
    committer_time: Option<Timestamp>,
    message: Bytes,
    file_changes: BTreeMap<Path, FileChange>,  // sorted, bytewise
    extra: BTreeMap<String, Bytes>,      // namespaced keys, e.g. "git.gpgsig", "flux.review.id"
}

enum FileChange {
    Change {
        content_id: ContentId,
        size: u64,
        kind: FileKind,                  // Regular | Executable | Symlink | GitLink(sha1)
        copy_from: Option<(Path, u32)>,  // (source path, parent index)
    },
    Deletion,
}
```

**Semantics.**
- For a root changeset (no parents), `file_changes` is the full file set.
- For a single-parent changeset, the file set is `parent_files ⊕ file_changes`.
- For a merge, the changeset MUST list every path whose resolved value differs from `parents[0]`. It MUST also list every path where the parents disagree with each other, even when the resolution equals `parents[0]`. This rule makes merge resolution explicit and verifiable.
- Creating a file at path `a/b` implicitly deletes any file at `a` (a file/directory conflict). Deleting the last file in a directory removes the directory. Validation MUST reject a changeset that leaves a path as both a file and a directory.

**Generation number.** `gen(cs) = 1 + max(gen(p) for p in parents)`, and `gen = 1` for a root. It is stored with the changeset at creation. It is the basis for all ancestry pruning.

**Immutability.** A changeset is never mutated. Rewrites such as amend and rebase create new changesets. The `flux.predecessor` extra key records the rewrite chain, which the client UX uses to show obsolescence.

### 3.4 Trees (derived, not authoritative)
A tree is the materialized file set at a changeset. Trees are **derived data** (§6) of type `tree_v1`. They are computed from the parent trees plus `file_changes` and are not part of the changeset.

```rust
struct Tree {
    entries: Vec<TreeEntry>,             // sorted bytewise by name
}
struct TreeEntry {
    name: Bytes,                         // single path component
    kind: EntryKind,                     // File(FileKind) | Dir
    id: [u8; 32],                        // ContentId or TreeId
    size: u64,                           // file size, or recursive total for dirs
    descendant_files: u64,               // 1 for files, recursive count for dirs
}
```

The recursive `size` and `descendant_files` let clients plan sparse checkouts, show `du`-style summaries and enforce size budgets without fetching subtrees. Trees with more than 65,536 entries are rejected at creation. That is a sanity bound and is configurable.

### 3.5 Refs ("bookmarks")
- A ref is `(repo, name) → (changeset_id, version: u64)`.
- **Updates are compare-and-swap only:** `UpdateRef(name, expected_version | expected_changeset, new_changeset)`.
- **Ref kinds** are set by name-prefix policy:
  - `protected`: `main`, `release/*`. Moved only by the landing service or admins.
  - `scratch`: `scratch/<user>/*`. TTL-reaped.
  - `mirror`: `mirror/<remote>/*`. Moved only by the mirror service.
- **Ref log:** every move is appended to `ref_log(repo, name, seq, old, new, actor, reason, ts)`.
- **Refs are discouraged for agent work.** Agents reference changesets by ID. A changeset is reachable by ID immediately after creation without any ref.

### 3.6 Changeset retention
- A changeset not reachable from any ref and not pinned is eligible for deletion after `retention.unreferenced` (default 90 days).
- Pins come from review systems (`PinChangeset`) and from CI artifacts.
- Content objects are reference-counted lazily by a mark-and-sweep job that runs **only** over the unreferenced set. The main history is never swept.

---

## 4. Architecture

```
                         ┌─────────────────────────────── region ───────────────────────────────┐
  agent / CI / dev       │                                                                       │
 ┌──────────────┐  uds   │ ┌────────────┐   gRPC   ┌─────────────┐   gRPC   ┌──────────────────┐ │
 │ flux CLI     │───────▶│ │  fluxd     │────────▶ │ flux-cache  │────────▶ │  flux-server     │ │
 │ flux-vfs     │        │ │ (per node) │          │ (per region,│          │ (stateless, N    │ │
 │ SDKs         │        │ │ disk store │          │  in-memory) │          │  replicas)       │ │
 └──────────────┘        │ └────────────┘          └─────────────┘          └───┬─────────┬────┘ │
                         │                                                      │         │      │
                         └──────────────────────────────────────────────────────┼─────────┼──────┘
                                                                                │         │
                                                            ┌───────────────────▼──┐  ┌───▼──────────────┐
                                                            │ Metadata store       │  │ Blob store       │
                                                            │ FoundationDB /       │  │ S3 / GCS / R2 /  │
                                                            │ CockroachDB /        │  │ MinIO / local fs │
                                                            │ Postgres / SQLite    │  │                  │
                                                            └──────────────────────┘  └──────────────────┘
        side services:  flux-land (landing)  ·  flux-derive (derivation workers)  ·  flux-git (git frontend)
                        flux-mirror (two-way sync)  ·  flux-index (search/code-intel feed)  ·  flux-admin
```

**Tier rules (enforced, not advisory):**
1. Every tier implements the same `ObjectService` RPCs (§8.2).
2. A tier talks only to the tier directly above it, or to the next fallback of that same tier (for example another region's cache). It never skips a tier. When the upstream is down, the downstream tier returns `UNAVAILABLE` and does not try to bypass it.
3. Every tier enforces per-client admission control and request coalescing (singleflight per object ID). A thundering herd on a hot object turns into one upstream fetch per tier.
4. Mutable data (refs, ACLs) is never cached beyond `ref_cache_ttl` (default 0; MAY be 1 s at regional caches for `ResolveRef` with `allow_stale=true`).

---

## 5. Storage

### 5.1 Metadata store abstraction
The `flux-store` crate defines a trait with two required capabilities:
- serializable single-key CAS;
- an ordered, append-only log per repo.

```rust
#[async_trait]
trait MetaStore: Send + Sync {
    // changesets
    async fn put_changeset(&self, repo: RepoId, cs: &StoredChangeset) -> Result<LogSeq>; // idempotent
    async fn get_changesets(&self, repo: RepoId, ids: &[ChangesetId]) -> Result<Vec<Option<StoredChangeset>>>;
    async fn log_tail(&self, repo: RepoId, after: LogSeq, limit: u32) -> Result<Vec<(LogSeq, ChangesetId)>>;
    // refs
    async fn get_ref(&self, repo: RepoId, name: &RefName) -> Result<Option<RefValue>>;
    async fn cas_ref(&self, repo: RepoId, name: &RefName, expect: RefExpect, new: Option<ChangesetId>,
                     log: RefLogEntry) -> Result<CasOutcome>;
    async fn list_refs(&self, repo: RepoId, prefix: &[u8], page: Page) -> Result<Vec<(RefName, RefValue)>>;
    // derived data mapping
    async fn get_derived(&self, repo: RepoId, ty: DerivedTypeId, ids: &[ChangesetId]) -> Result<Vec<Option<DerivedRef>>>;
    async fn put_derived(&self, repo: RepoId, ty: DerivedTypeId, entries: &[(ChangesetId, DerivedRef)]) -> Result<()>;
    // content metadata
    async fn get_content_meta(&self, ids: &[ContentId]) -> Result<Vec<Option<ContentMeta>>>;
    async fn put_content_meta(&self, metas: &[(ContentId, ContentMeta)]) -> Result<()>;
    // git mapping (bidirectional)
    async fn git_map_get(&self, repo: RepoId, q: GitMapQuery) -> Result<Vec<GitMapEntry>>;
    async fn git_map_put(&self, repo: RepoId, entries: &[GitMapEntry]) -> Result<()>;
}
```

**Backends (priority order):**

| Backend | Use | Notes |
|---|---|---|
| `sqlite` | local dev, tests, single-node | Phase 1 |
| `postgres` | small and medium deployments (< ~5 M changesets, < 500 writes/s) | Phase 1. The log uses a `bigserial` plus a `visible_at_seq` watermark for gap-free tailing (§5.1.2). |
| `foundationdb` | large deployments | Phase 5. Log keys use versionstamps, giving a naturally gap-free, commit-ordered log. |
| `cockroachdb` | alternative large-scale backend | Phase 5+. Postgres-wire compatible, with a different log strategy. |

#### 5.1.1 Postgres schema (reference)
```sql
CREATE TABLE repos (
  repo_id      INT PRIMARY KEY,
  name         TEXT UNIQUE NOT NULL,
  config       JSONB NOT NULL
);

CREATE TABLE changesets (
  repo_id      INT NOT NULL,
  cs_id        BYTEA NOT NULL,           -- 32 bytes
  gen          BIGINT NOT NULL,
  log_seq      BIGINT NOT NULL,
  parents      BYTEA[] NOT NULL,
  body         BYTEA NOT NULL,           -- canonical CBOR (≤ 1 MiB; larger bodies spill to blob store, body = pointer)
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (repo_id, cs_id)
);
CREATE UNIQUE INDEX changesets_log ON changesets (repo_id, log_seq);

CREATE TABLE changeset_parents (          -- reverse edges for "children" and ancestry queries
  repo_id INT, parent BYTEA, child BYTEA, idx SMALLINT,
  PRIMARY KEY (repo_id, parent, child)
);

CREATE TABLE refs (
  repo_id INT, name BYTEA, cs_id BYTEA NOT NULL, version BIGINT NOT NULL,
  PRIMARY KEY (repo_id, name)
);
CREATE TABLE ref_log (
  repo_id INT, name BYTEA, seq BIGSERIAL, old_cs BYTEA, new_cs BYTEA,
  actor TEXT, reason TEXT, ts TIMESTAMPTZ DEFAULT now(),
  PRIMARY KEY (repo_id, name, seq)
);

CREATE TABLE derived (
  repo_id INT, type_id SMALLINT, cs_id BYTEA, value_ref BYTEA NOT NULL,  -- value_ref = DerivedId or inline small value
  PRIMARY KEY (repo_id, type_id, cs_id)
);

CREATE TABLE content_meta (
  content_id BYTEA PRIMARY KEY, size BIGINT, sha1 BYTEA, sha256 BYTEA,
  is_binary BOOL, chunked BOOL
);

CREATE TABLE git_map (
  repo_id INT, cs_id BYTEA, git_sha1 BYTEA,
  PRIMARY KEY (repo_id, cs_id)
);
CREATE UNIQUE INDEX git_map_rev ON git_map (repo_id, git_sha1);

CREATE TABLE pins (repo_id INT, cs_id BYTEA, holder TEXT, expires_at TIMESTAMPTZ,
  PRIMARY KEY (repo_id, cs_id, holder));
```

#### 5.1.2 Gap-free log tailing on Postgres
A `bigserial` assigns sequence numbers at insert time, but transactions can commit out of order. Without care, a tailer could skip a row that commits late.

Writers therefore insert under an advisory lock `pg_advisory_xact_lock(repo_id)` that is held only for the `INSERT ... RETURNING log_seq`. The lock is held for microseconds and the body is pre-validated before it is taken. This serializes sequence assignment and commit per repo, and it is the documented throughput ceiling of the Postgres backend. FoundationDB avoids the problem with versionstamps.

#### 5.1.3 FoundationDB key layout
Tuple-layer encoded, with subspace prefix `("flux", repo_id)`:
```
("cs", cs_id)                          → canonical body (or blob pointer)
("cs_gen", cs_id)                      → gen
("log", versionstamp)                  → cs_id
("child", parent_id, child_id)         → idx
("ref", name)                          → (cs_id, version)
("reflog", name, versionstamp)         → entry
("dd", type_id, cs_id)                 → value_ref
("git", "cs", cs_id) / ("git", "sha", sha1) → mapping
global: ("flux", "content", content_id) → ContentMeta
```
Values over 90 KB spill to the blob store.

### 5.2 Blob store
- Uses the `object_store` crate for S3, GCS, Azure and the local filesystem.
- **Key layout:** `{namespace}/{type}/{id[0..2]}/{id[2..4]}/{id_hex}`, where `type ∈ {c, k, t, d, cs}`.
- **Object format:** a 16-byte header followed by the payload.
  - Header fields: `magic "FLX1"`, `u8 codec` (0 = raw, 1 = zstd, 2 = zstd + dictionary), `u8 kind`, `u16 dict_id`, `u64 uncompressed_len`.
- Writes are unconditional `PUT`s. They are idempotent because keys are content-addressed. A write is acknowledged only after the blob store confirms durability.
- **Ordering invariant:** a changeset is inserted into the metadata store only after all referenced content exists (§8.3.2). Readers can therefore never observe a dangling reference.

### 5.3 Compression dictionaries
- For each repo, `flux-admin` trains up to 16 zstd dictionaries per file-extension class (`.rs`, `.py`, `.ts`, `.json`, and so on) from sampled content.
- Dictionaries are themselves content-addressed and immutable. `dict_id` maps to a dictionary `ContentId` in the repo config.
- Typical savings on source code are 30–50% over plain zstd.

---

## 6. Derived data framework

### 6.1 Contract
```rust
trait DerivedType: Send + Sync + 'static {
    const NAME: &'static str;            // e.g. "tree_v1"; the version suffix is part of the identity
    const ID: DerivedTypeId;             // stable small integer
    type Value: Serialize + DeserializeOwned + Send + Sync;
    /// Other derived types (on the same changeset or on parents) required before this one.
    fn dependencies() -> &'static [DerivedTypeId];
    /// Pure function of: the changeset, this type's values on its parents, and declared dependencies.
    async fn derive(ctx: &DeriveCtx, cs: &Changeset, parents: &[Self::Value]) -> Result<Self::Value>;
    fn mode() -> DeriveMode;             // Eager | Lazy
}
```

**Rules:**
1. `derive` MUST be deterministic. Its output MUST depend only on its inputs and on immutable stored objects.
2. Values are stored in the blob store keyed by `DerivedId`. The `derived` table maps `(type, cs_id) → value_ref`. Values ≤ 256 bytes are stored inline in the `derived` table.
3. A `derived` row is written **only after** its value and all objects the value references are durable.
4. **Derivation for changeset X:**
   - walk ancestors in descending generation order until each frontier changeset has a value;
   - derive in ascending generation order;
   - batch independent changesets of the same generation in parallel (bounded by `derive.parallelism`).
5. **Concurrency:** multiple workers may race to derive the same changeset. That is safe because output is deterministic and writes are idempotent. Workers SHOULD take a lease (`derive_lease` row with a TTL) to avoid duplicate work, but correctness never depends on the lease.
6. **Bulk backfill:** a new type is backfilled by `flux-derive backfill --type X`, which walks the log in sequence order. Workers coordinate through leased log ranges.

### 6.2 Built-in derived types

| Type | Mode | Value | Used by |
|---|---|---|---|
| `tree_v1` | Eager | root `TreeId` | checkout, `ls`, diff |
| `skiplist_v1` | Eager | skip pointers (powers-of-two ancestor jumps along first-parent, plus merge-aware fallback) | `IsAncestor`, merge-base, log ranges |
| `pathhist_v1` | Lazy (eager on protected refs) | per-path history node: `(path, content, linknode=cs_id, parents: [PathHistId])`. Created only for paths changed in a changeset. | `log -- path`, blame, follow-renames |
| `blame_v1` | Lazy | per-file line ranges → `cs_id` | blame |
| `copyidx_v1` | Lazy | inferred copies and renames (similarity ≥ 50%) for changes that lack an explicit `copy_from` | rename-aware diff and history |
| `git_v1` | Eager when git enabled | git commit SHA-1 plus git tree SHA-1s (§10.1) | git frontend, mirror |
| `filesearch_v1` | Lazy | path trigram index delta (a persistent radix tree) | `FindFiles` RPC |
| `changed_dirs_v1` | Eager | set of directory prefixes touched | CI target selection, ACL checks, ownership routing |

### 6.3 `tree_v1` derivation algorithm
Input: the parent root trees and `file_changes`. Output: the new root tree. The algorithm:
1. Group the changed paths into a prefix trie.
2. Recursively rewrite **only the touched subtrees**. Untouched subtree IDs are reused unchanged. Cost is O(changes × depth), independent of repo size.
3. For merges, start from `parents[0]` and apply `file_changes`. By the merge rule in §3.3, those changes contain every needed resolution.
4. Remove empty directories.
5. Recompute `size` and `descendant_files` up the spine.
6. Write the new trees (typically 1–20 objects) to the blob store, then return the root ID.

### 6.4 Ancestry queries
- `IsAncestor(a, b)`: if `gen(a) > gen(b)`, the answer is false. Otherwise walk from `b` using skip-list jumps, never descending below `gen(a)`.
- `MergeBase(a, b)` uses generation-bounded bidirectional BFS with skip lists.
- Target: p99 < 10 ms at 100 M changesets for typical first-parent queries.
- An adversarial-DAG benchmark is part of the test suite (§14).

---

## 7. Write-path semantics (server-side operations)

### 7.1 `CreateChangeset`
Input: the parents, metadata and a list of `FileChange`s whose `content_id`s already exist (uploaded through `UploadContent`). The server then:
1. Authorizes: repo `write`, plus path-ACL `write` for every changed path (§12.2).
2. Validates path rules, the file/directory conflict rule, that content exists (one batched metadata lookup), and the merge completeness rule.
3. Canonicalizes, hashes and computes `gen`.
4. Inserts (this is idempotent: re-submitting identical input returns the same ID).
5. Synchronously derives the eager types `tree_v1` and `changed_dirs_v1`. Agents usually read the tree immediately, so this is worth doing inline. Other eager types are derived asynchronously by `flux-derive`, which tails the log.
6. Returns `ChangesetId`, `TreeId` and `gen`.

**Latency target:** p99 < 100 ms for ≤ 100 file changes with a warm metadata cache.

### 7.2 `CreateChangesetFromTree` (convenience)
The client supplies a parent and a set of `(path → content | delete)` edits *or* a complete target root tree. The server computes the minimal `file_changes`. This RPC exists for clients that materialize state (`flux commit` from a working copy) rather than producing deltas.

### 7.3 `Rebase` (pushrebase semantics)
`Rebase(stack: [cs_1..cs_n], onto: cs_base, policy)` returns either `[cs_1'..cs_n']` or `Conflict`.

1. Let `F` be the set of files changed between `old_base = parents(cs_1)[0]` and `onto`. This is computed from `changed_dirs_v1` and `tree_v1` diffs. It is O(changes between bases), not O(repo), using the skip list and a log scan bounded by generation.
2. For each `cs_i`:
   - If `paths(cs_i) ∩ F = ∅`, the rebase is **trivial**: re-parent `cs_i` and copy its file changes verbatim.
   - Otherwise, apply `policy.on_overlap`:
     - `reject` (the default for `main`): return `Conflict{paths}`.
     - `merge3` (opt-in per path glob, for example `**/*.lock` excluded): run a line-based three-way merge (diff3 semantics, histogram diff). Succeed only if there are no conflict hunks.
3. Each rebased changeset gets `extra["flux.rebased_from"] = cs_i`.
4. Rebasing never requires a checkout. Cost is O(Σ changes).

### 7.4 `Merge`
`Merge(parents: [a, b, ...], resolutions?)` produces a merge changeset. If the server can auto-resolve every path (only one side changed it, or `merge3` succeeds), the call succeeds. Otherwise it returns a `Conflict` listing the conflicting paths and base/ours/theirs content IDs. The caller, typically an agent, resubmits with `resolutions`.

### 7.5 `UpdateRef`
A compare-and-swap. Policy hooks run first:
- protected-ref rules;
- fast-forward-only flag;
- required `extra` keys, for example a review ID.

The ref-log entry is written in the same transaction as the ref update.

---

## 8. RPC API (gRPC, `flux.v1`)

All services use `tonic`. Every message carries an optional `RequestContext { trace_id, client_id, deadline }`, and authentication is a bearer token in metadata (§12.1). Batch RPCs accept up to 10,000 IDs and stream results **in arbitrary order**, each result tagged with its ID.

### 8.1 Service map
| Service | Served by | Purpose |
|---|---|---|
| `ObjectService` | fluxd, flux-cache, flux-server | Immutable object reads (cacheable at every tier) |
| `RepoService` | flux-server (fluxd proxies it) | Refs, changesets, history, diff, blame, search |
| `WriteService` | flux-server | Content upload, changeset creation, rebase, merge, refs |
| `LandService` | flux-land | Submit, observe and cancel lands |
| `AdminService` | flux-server | Repos, config, ACLs, pins, maintenance |
| `EventService` | flux-server | Server-streaming log tail and ref-change events (for CI, indexers, mirrors) |

### 8.2 `ObjectService` (identical at all tiers)
```protobuf
syntax = "proto3";
package flux.v1;

service ObjectService {
  // Fetch trees. If depth > 0, the tier MAY also stream descendant trees up to `depth`
  // levels, filtered by `sparse` (if set). Tiers that lack a descendant forward upstream.
  rpc GetTrees(GetTreesRequest) returns (stream TreeObject);
  // Fetch file contents. Chunked contents are streamed as ordered ContentChunk frames.
  rpc GetContents(GetContentsRequest) returns (stream ContentFrame);
  // Existence + metadata without bodies (size, sha1, is_binary, chunked).
  rpc GetContentMeta(IdsRequest) returns (stream ContentMetaResult);
  rpc GetDerived(GetDerivedRequest) returns (stream DerivedObject);
}

message GetTreesRequest {
  RequestContext ctx = 1;
  repeated bytes tree_ids = 2;
  uint32 depth = 3;                 // 0 = only the requested trees
  SparseSpec sparse = 4;            // include/exclude globs, evaluated on full paths
  bytes path_prefix_hint = 5;       // path of tree_ids[0], required when depth>0 && sparse set
}
message TreeObject   { bytes id = 1; bytes path = 2; bytes canonical = 3; }   // canonical CBOR
message GetContentsRequest { RequestContext ctx = 1; repeated bytes content_ids = 2; bool allow_compressed = 3; }
message ContentFrame {
  bytes content_id = 1;
  uint64 offset = 2;
  bytes data = 3;                   // ≤ 4 MiB per frame
  bool last = 4;
  Codec codec = 5;                  // RAW, ZSTD, ZSTD_DICT (client must have dict) — only if allow_compressed
  uint32 dict_id = 6;
}
```

**Negotiation (§1.1, Scion-style).** The client never advertises what it already has. The checkout loop is:
1. `ResolveRef` or `GetChangeset`, then take the root `TreeId`.
2. Look up the root in the local store. If missing, call `GetTrees(depth=N, sparse=profile)`.
3. For each tree received, recurse into child trees that are still missing.
4. Batch-request the missing contents.

Every request names immutable objects, so every tier can answer it. Transfers are resumable per object.

### 8.3 `WriteService`
```protobuf
service WriteService {
  rpc HasContents(IdsRequest) returns (HasContentsResponse);            // bitmap
  rpc UploadContent(stream UploadFrame) returns (stream UploadResult);  // client-hashed; server re-verifies
  rpc CreateChangeset(CreateChangesetRequest) returns (CreateChangesetResponse);
  rpc CreateChangesetFromTree(CreateFromTreeRequest) returns (CreateChangesetResponse);
  rpc Rebase(RebaseRequest) returns (RebaseResponse);
  rpc Merge(MergeRequest) returns (MergeResponse);
  rpc UpdateRef(UpdateRefRequest) returns (UpdateRefResponse);
  rpc PinChangeset(PinRequest) returns (PinResponse);
}
```
#### 8.3.1 Upload
1. The client hashes locally, calls `HasContents`, and uploads only what is missing.
2. The server re-hashes while streaming. A mismatch returns `INVALID_ARGUMENT` and the object is discarded.
3. Uploads of 64 MiB or more use resumable sessions (`upload_session_id`, `offset`).

#### 8.3.2 Ordering guarantee
`CreateChangeset` succeeds only if every referenced content is present and durable. This guarantee is what lets readers trust every reference without checking.

### 8.4 `RepoService` (selected)
```protobuf
service RepoService {
  rpc ResolveRef(ResolveRefRequest) returns (ResolveRefResponse);
  rpc ListRefs(ListRefsRequest) returns (stream RefEntry);
  rpc ResolvePrefix(ResolvePrefixRequest) returns (ResolvePrefixResponse);
  rpc GetChangesets(IdsRequest) returns (stream ChangesetObject);
  rpc Log(LogRequest) returns (stream LogEntry);             // range, first-parent, path filter, limit
  rpc IsAncestor(IsAncestorRequest) returns (IsAncestorResponse);
  rpc MergeBase(MergeBaseRequest) returns (MergeBaseResponse);
  rpc Diff(DiffRequest) returns (stream DiffEntry);          // tree-level; optional unified hunks
  rpc Blame(BlameRequest) returns (BlameResponse);
  rpc PathHistory(PathHistoryRequest) returns (stream PathHistoryEntry); // follow_renames flag
  rpc ListDir(ListDirRequest) returns (ListDirResponse);     // path@cs without fetching trees
  rpc ReadFile(ReadFileRequest) returns (stream ContentFrame); // path@cs convenience for agents
  rpc FindFiles(FindFilesRequest) returns (stream PathMatch); // glob / substring over paths
  rpc ChangedTargets(ChangedTargetsRequest) returns (ChangedTargetsResponse); // dirs touched in range
}
```

`ReadFile`, `ListDir` and `Diff` make up the **no-checkout agent path**. An agent can review, bisect or revert without materializing any files.

### 8.5 `EventService`
`Subscribe(SubscribeRequest{repo, kinds: [CHANGESET_CREATED, REF_MOVED, DERIVED_READY], from_seq})` returns `stream Event`. Delivery is at-least-once and ordered by log sequence. Consumers checkpoint `from_seq`.

### 8.6 Error model
Standard gRPC codes, plus a `google.rpc.ErrorInfo` with `reason` taken from a closed enum:

`NOT_FOUND_OBJECT`, `NOT_FOUND_REF`, `CAS_MISMATCH`, `PATH_INVALID`, `PATH_CONFLICT`, `CONTENT_MISSING`, `MERGE_INCOMPLETE`, `REBASE_CONFLICT`, `ACL_DENIED`, `POLICY_REJECTED`, `TOO_LARGE`, `OVERLOADED`.

`OVERLOADED` includes `retry_after_ms`.

---

## 9. Server and cache tiers

### 9.1 `flux-server`
- Stateless. Scales horizontally behind an L7 load balancer, with consistent hashing on object ID for `ObjectService` to improve its in-memory cache hit rate.
- **In-process caches:** a bounded LRU (moka) for changesets, trees and derived values, by bytes. A separate small cache holds content metadata.
- **Admission control:**
  - a per-principal token bucket;
  - a global concurrency limit per RPC class (`read_object`, `history`, `write`, `land`);
  - load shedding by priority: `interactive > land > ci > batch`, taken from the `x-flux-priority` header.

### 9.2 `flux-cache` (regional)
- In-memory object cache with an optional NVMe second tier (foyer crate), sized for the regional working set (target 256 GB RAM + 4 TB NVMe per node).
- Shards by object ID with rendezvous hashing across cache peers. A miss on the owning peer goes to `flux-server`, never to another peer. Peers never form chains.
- Serves trees and contents **in the exact on-disk segment format fluxd ingests** (§9.3), so the daemon can append them without re-encoding.
- Holds no durable state. Restarting a node is always safe: the cache warms from traffic, and an optional `warm-from-peer` mode exists for rolling restarts.

### 9.3 `fluxd` (per node)
- **One daemon per host.** It serves clients over a Unix domain socket (`/run/flux/fluxd.sock`) with gRPC over UDS. Container sandboxes get the socket bind-mounted read-write.
- **Local store:** a log-structured object store.
  - Append-only segment files of 256 MiB, plus an in-memory index of `id → (segment, offset, len)`, persisted as an SST-style index file per sealed segment.
  - **Eviction** works by whole segments, LRU by last access, with live objects compacted forward when segment utilization falls below 30%.
  - The configurable `max_bytes` budget defaults to 20% of free disk.
- **Seeding:** at boot, fluxd fetches the latest **snapshot** (a set of sealed segments plus index) from `{blobstore}/snapshots/{repo}/{ts}/`. These are built every 6 h by `flux-admin snapshot` and contain the trees and contents for the protected refs' heads under the configured default sparse profiles.
- **Shared across jobs.** Ten CI jobs on one host fetch each object once.
- **Proxying.** fluxd proxies `RepoService` and `WriteService` upstream unchanged. It adds request coalescing and a short TTL cache for `ResolveRef`, but only when `allow_stale` is set.
- **Isolation.** Each request carries the client's own token, and fluxd forwards it. A tree or content is served from the local store only if that principal has previously been authorized for its **ACL domain** (§12.2). Unrestricted-domain objects are always served locally.

---

## 10. Git interoperability

### 10.1 Import (`flux-import-git`)
- Uses `gix` to read the repository. Processes commits in topological order with parallel content upload, and writes changesets in batches of 1,000.
- Each git commit maps to a flux changeset:
  - **parents:** mapped parents, in the same order;
  - **file_changes:** the diff against the first parent, plus the merge rule (§3.3) for merges;
  - **kind:** from the git mode (100644 → Regular, 100755 → Executable, 120000 → Symlink, 160000 → GitLink);
  - **author and committer:** raw bytes.
- The `extra` map preserves git fidelity: `git.encoding`, `git.gpgsig`, `git.mergetag` and other non-standard headers, in order. The `git_v1` derivation can then reproduce the **exact git commit bytes and therefore the same SHA-1**.
- **Non-canonical git objects** (for example malformed trees, zero-padded modes or unsorted entries, which do exist in old repos) are detected by re-deriving and comparing. Their raw object bytes are stored in a `git_raw` sidecar keyed by SHA-1, and `git_v1` returns them verbatim.
- **Resumable.** Progress is checkpointed in `import_state`. The importer is incremental: `--follow <remote>` keeps importing new commits.
- **Performance target:** ≥ 2,000 commits/s sustained on an 8-core node for typical source repos. Content upload is the bottleneck and is parallelized with 64 in-flight uploads.

### 10.2 `git_v1` derived type
For each changeset, it computes git tree objects bottom-up, reusing parent git trees for untouched subtrees, followed by the git commit object. It records `git_map(cs ↔ sha1)` and stores git tree SHA-1s alongside flux tree IDs.

### 10.3 `flux-git` (git smart-HTTP frontend)
- Implements protocol v2 `ls-refs` and `fetch`, including `filter=blob:none`, `filter=tree:0`, `shallow`/`deepen`, `want-ref` and **bundle-uri**, which points at pre-built bundles in the blob store.
- **Packs are generated on the fly** from derived git objects and are never stored. Objects are deltified only within a request, with a time budget. Large fetches are offloaded to bundle-uri.
- **Push support** (`receive-pack`, phase 3b): the incoming pack is imported as changesets, then:
  - pushes to `refs/heads/main` are routed to `flux-land`, so a push becomes a land;
  - pushes to `refs/heads/scratch/*` are a direct ref CAS.
- **Goal:** `git clone --filter=blob:none https://flux.example/repo` works, and `git fetch` stays O(new objects).

### 10.4 `flux-mirror` (two-way sync with GitHub/GitLab)
- **Inbound:** a webhook or poll triggers an incremental import, then the mirror moves `mirror/<remote>/<branch>`.
- **Outbound:** the mirror tails `EventService` for protected-ref moves and pushes `git_v1` commits to the remote. Commit SHA-1s are identical in both directions, so mirrors never diverge in history.
- **Conflict policy:** one side is designated authoritative per ref. A non-fast-forward on the other side raises an alert and is never resolved automatically.

---

## 11. Landing service (`flux-land`)

Scion relies on post-merge testing rather than a global merge-queue lock, and so does flux.

### 11.1 Model
```
LandRequest {
  stack: [ChangesetId],          // ordered, each parented on the previous
  target_ref: "main",
  rebase_policy: RebasePolicy,   // default: reject on overlap
  required: [Requirement],       // approvals, checks — validated via hooks
  priority: Interactive|Agent|Bulk,
  conflict_hook: Option<Url>,    // called on REBASE_CONFLICT, e.g. an agent fixer
  idempotency_key: String,
}
```

### 11.2 Landing algorithm (per target ref)
Each protected ref has a single **logical lander**: a lease-elected leader among `flux-land` replicas. The leader loop runs:

1. Pull up to `batch_max` (default 64) requests from the queue, ordered by priority, then FIFO.
2. Read the head (`cs_h`, `version_v`).
3. Rebase each request's stack onto the running head, in order. A rebase is cheap and server-side (§7.3).
   - If a request conflicts, mark it `CONFLICT`, fire its `conflict_hook`, and drop it from the batch. The others continue.
   - Successful stacks chain: request k+1 is rebased onto request k's new tip.
4. Run pre-land policy checks on each rebased stack: approvals still valid after the rebase and path-ACL write rights. Review systems may register **semantic approval rules** through hooks, for example "comment-only change needs no re-approval".
5. `UpdateRef(target, expect=version_v, new=batch_tip)`.
   - On `CAS_MISMATCH` (an admin or another writer moved the ref), go back to step 2 with the same batch. After 3 failures, shrink the batch by half.
6. Publish `LANDED(request, landed_cs_id)` events.

**Throughput.** A cycle costs roughly one metadata round trip for the CAS plus parallel rebases. A 64-request batch per cycle every 200 ms yields more than 300 lands/s on a single ref.

### 11.3 Post-land safety net
- **CI trigger.** CI subscribes to `REF_MOVED` and runs targeted builds using `ChangedTargets` per batch.
- **`flux-bisect`.** When a post-land check fails, it bisects the landed range using `ReadFile`/`Diff` and no checkouts, through the CI provider's API. It identifies the culprit request.
- **Auto-revert.** A revert changeset is generated server-side: the inverse of the culprit's file changes, rebased onto the head. It is submitted as a `Priority::Interactive` land with `extra["flux.revert_of"]`. Policy decides whether this is automatic or needs approval.
- **Deploy gates.** A pre-deploy check against a candidate changeset is the real safety gate, consistent with Scion's model.

### 11.4 Integration hooks (for review, CI and agents)
All hooks are HTTP POST with an HMAC-signed body, a timeout and a retry policy:
- `pre_land(stack, rebased_stack) → allow | deny(reason)`
- `on_conflict(request, conflict) → (optional) new stack`, which feeds the agent fix-up loop
- `on_landed(event)`
- `approval_check(stack_before, stack_after) → still_valid: bool`, where semantic diff rules live

---

## 12. Security

### 12.1 Authentication
- **Humans and agents:** OIDC JWT bearer tokens, verified by `flux-server` against configured issuers via cached JWKS. The token's `sub` plus configured claim mapping produces a `Principal { id, groups }`.
- **Workloads:** SPIFFE/SVID or cloud workload identity, exchanged for a short-lived flux token through `AuthService.Exchange`.
- **Between tiers:** mTLS. fluxd and flux-cache authenticate as `system:tier`, and also forward the end-client token for authorization decisions.

### 12.2 Authorization
- **Repo roles:** `read`, `write`, `land`, `admin`.
- **Path ACLs:** rules stored in the repo config, versioned as a protected file `/.flux/acl.toml` at the head of `main`, so ACL changes are reviewed like code.

```toml
[[domain]]
name = "secrets"
paths = ["infra/secrets/**", "security/keys/**"]
read  = ["group:sec-eng"]
write = ["group:sec-eng"]
```

**Enforcement in a content-addressed system.** A content hash is not a secret: knowing a hash should not grant its content. flux handles this as follows:
1. Objects whose path falls inside a restricted domain are stored under that domain's namespace. Their IDs are computed with a **domain-keyed hash**: BLAKE3 keyed mode with a per-domain secret held by flux-server. They are therefore unguessable and unlinkable to public content.
2. Trees containing a restricted subtree list that subtree's entry normally. When `GetTrees` serves such a tree to a principal outside the domain, the entry is replaced with a `Redacted` placeholder, so the client sees that a directory exists but not its contents.
3. Caches and fluxd tag stored objects with their domain. They serve domain objects only to principals that flux-server has confirmed have access. Confirmations are cached per `(principal, domain)` for at most 60 s.
4. **Moving a file across a domain boundary** re-hashes it into the target namespace, and the server does this at changeset creation.
5. **ACL changes do not rewrite history.** A newly restricted path takes effect for new changesets. Historical exposure is documented, and `flux-admin acl rehome` can re-key history as an offline operation.

### 12.3 Audit
Every write, every ref move and every read of a restricted domain is emitted to an audit log (OTLP logs, with the sink configurable).

---

## 13. Client

### 13.1 `flux` CLI (crate `flux-cli`)
The working-copy state lives in `.flux/` at the root:
- `config.toml`
- `wc.sqlite`: dirstate (`path → (content_id, size, mtime_ns, ino, mode)`), the checked-out changeset and the sparse profile
- `ops.log`: an operation log with undo support, à la jj

| Command | Behavior |
|---|---|
| `flux attach <repo-url> [dir] [--sparse profile]` | Creates the working copy; nothing is downloaded until checkout. |
| `flux checkout <rev>` | Diffs the current tree against the target tree (§8.2 negotiation), fetches what is missing, then updates files atomically per file and the dirstate. |
| `flux status` / `flux diff` | Uses the filesystem watcher journal (`notify`, FSEvents, inotify) to stat only changed paths. Falls back to a full stat walk. |
| `flux commit -m` | Uploads missing contents, then `CreateChangeset` against the checked-out parent. There is no local commit store. |
| `flux amend`, `flux split`, `flux absorb` | Server-side rewrites, with predecessors recorded. |
| `flux rebase -d <rev>` | Server `Rebase`, then the working copy moves. |
| `flux land [--stack]` | `LandService.Submit`, then streams status. |
| `flux log`, `flux blame`, `flux show`, `flux grep-paths` | Thin wrappers over `RepoService`. |
| `flux sparse add/remove/show` | Edits the profile and reconciles the working copy. |
| `flux undo` | Reverts the last working-copy operation from `ops.log`. |

**Revision syntax:** `@` (working parent), `main`, `cs_<prefix>`, `git:<sha>`, `<rev>~N`, `<rev>^N`.

### 13.2 Working-copy correctness
- Every update writes to a temporary file followed by `rename(2)`. The dirstate is updated in the same SQLite transaction as the operation log.
- After a crash, `flux doctor` reconciles the dirstate against disk.
- Executable bits and symlinks are honored. On Windows, symlinks degrade to plain files when the developer mode privilege is absent, and a warning is printed.

### 13.3 Virtual filesystem (`flux-vfs`, phase 4)
| OS | Mechanism |
|---|---|
| Linux | FUSE (`fuser`), with passthrough for materialized files (kernel ≥ 6.9 FUSE passthrough) |
| macOS | NFSv3 loopback server, avoiding the kext requirement. This is the approach EdenFS takes. |
| Windows | ProjFS |

- **Inode model:** an unmaterialized node points to `(TreeId | ContentId)`. A write or `open(O_RDWR)` *materializes* the file into the overlay (`.flux/overlay/`).
- `status` cost is O(materialized files), not O(repo size).
- **Prefetch:** directory-level heuristics, plus `flux prefetch <globs>` and build-tool integration that prefetches declared inputs.

### 13.4 SDKs
Generated gRPC clients for Rust, Go, Python and TypeScript. A thin, hand-written **agent toolkit** wraps them:
- `read_file`, `list_dir`, `diff`, `blame`, `find_files`
- `create_change(parent, edits)`, `rebase`, `land`

The toolkit is shipped as an **MCP server** (`flux-mcp`) so agents can work on the repo with zero checkout.

---

## 14. Testing and verification

| Layer | Approach |
|---|---|
| Codec and hashing | Golden vectors (`testdata/codec/`), cross-platform CI, proptest round-trips |
| Data model invariants | proptest generators for random DAGs and file changes. Check that `tree_v1(cs)` equals a naive full-materialization oracle and that `git_v1` round-trips import. |
| Git fidelity | Import and re-derive must produce byte-identical SHA-1s for git.git, linux.git, chromium (subset) and a corpus of known-malformed repos. This runs nightly. |
| Distributed behavior | **Deterministic simulation** with `turmoil`: the server, a cache, fluxd, a fake blob store and a fake metadata store, with injected partitions, latency, crashes and clock skew. Invariants: no dangling refs, CAS linearizability, and land exactly-once per idempotency key. |
| Fuzzing | `cargo-fuzz` on the CBOR decoder, path parser, git object parser, pack parser and diff3 |
| Store backends | One conformance suite run against every `MetaStore` and blob store implementation |
| Load | `flux-bench` replays synthetic traffic: N agents creating changesets, M CI checkouts with sparse profiles, landing pressure at 50–500/s. It reports p50/p99 latency, cache hit rates and CPU/memory per tier. |
| End-to-end | docker-compose stack (Postgres, MinIO, server, cache, fluxd, land) driven by scripted CLI scenarios |

---

## 15. Operations

### 15.1 Observability
- `tracing`, exported over OTLP. One trace spans CLI → fluxd → cache → server → store.
- **Required metrics** (Prometheus names):
  - `flux_rpc_duration_seconds{service,method,tier,code}`
  - `flux_cache_hits_total{tier,kind}` and `flux_cache_misses_total{tier,kind}`
  - `flux_cache_bytes{tier}`
  - `flux_derive_lag_seconds{type}`: newest changeset age minus newest derived age
  - `flux_land_queue_depth{ref}`, `flux_land_cycle_seconds`, `flux_land_conflicts_total`
  - `flux_ref_cas_conflicts_total{ref}`
  - `flux_blobstore_request_seconds{op}`, `flux_metastore_request_seconds{op}`
- **SLOs (defaults):**
  - `ObjectService` availability 99.95%;
  - `CreateChangeset` p99 < 150 ms;
  - land time from submit to landed p50 < 5 s, excluding hooks;
  - derive lag p99 < 30 s for eager types.

### 15.2 "Superfan" agent hooks
Each component exports a structured `/debug/health-narrative` endpoint containing recent anomalies, top errors and saturation. Telemetry follows stable schemas so an LLM watcher (optional, out of tree, under `contrib/superfan/`) can watch a single component as Scion's operators do.

### 15.3 Deployment
- **Packaging:** container images for every service, a Helm chart (`deploy/helm/flux`), and a single-binary `flux-allinone` (server, land, derive and SQLite/local-fs backends) for evaluation.
- **Configuration:** TOML plus environment overrides.
- **Upgrades:** the protobuf API has compatibility CI (`buf breaking`). The data format never changes in place; new derived types are added alongside the old ones.

---

## 16. Repository layout

```
flux/
├── Cargo.toml                 # workspace; Rust 2024 edition, MSRV pinned
├── proto/flux/v1/*.proto      # source of truth for the API (buf-managed)
├── crates/
│   ├── flux-core/             # ids, hashing, canonical codec, Path, Changeset, Tree, FileChange
│   ├── flux-store/            # MetaStore + BlobStore traits; sqlite, postgres, fdb, object_store impls
│   ├── flux-derive/           # derived-data framework + built-in types (tree, skiplist, pathhist, blame, git, ...)
│   ├── flux-ops/              # server-side algorithms: create, rebase, merge, diff, diff3
│   ├── flux-proto/            # generated tonic/prost code
│   ├── flux-server/           # flux-server binary
│   ├── flux-cache/            # regional cache binary
│   ├── fluxd/                 # node daemon binary + segment store
│   ├── flux-land/             # landing service
│   ├── flux-git/              # git object model bridge, import, smart-HTTP frontend
│   ├── flux-mirror/           # two-way sync
│   ├── flux-cli/              # `flux` binary
│   ├── flux-vfs/              # FUSE / NFS / ProjFS
│   ├── flux-mcp/              # MCP server for agents
│   ├── flux-bench/            # load generator
│   └── flux-sim/              # deterministic simulation harness
├── sdk/{go,python,typescript}/
├── deploy/{docker-compose,helm}/
├── testdata/
└── docs/
```

**Core dependencies:** `tokio`, `tonic`/`prost`, `blake3`, `zstd`, `fastcdc`, `ciborium` (deterministic mode, wrapped), `object_store`, `sqlx`, `rusqlite`, `foundationdb`, `moka`, `foyer`, `gix`, `imara-diff`, `notify`, `fuser`, `clap`, `tracing`, `opentelemetry`, `turmoil`, `proptest`.

**License:** Apache-2.0.

---

## 17. Phases and acceptance criteria

### Phase 1: Core vertical slice (≈ 6–8 weeks)
**Scope:**
- `flux-core`;
- `flux-store` with SQLite, Postgres, local-fs and S3;
- `tree_v1` and `skiplist_v1`;
- `flux-server` with `ObjectService`, the core of `RepoService` and `WriteService` (no rebase or merge);
- a non-virtual sparse checkout CLI (`attach`, `checkout`, `status`, `diff`, `commit`, `log`, `show`);
- `flux-import-git` (no sidecar yet);
- `flux-allinone`.

**Acceptance:**
- Import git.git and linux.git (≈ 1.3 M commits). Every imported commit's file set equals git's (oracle check on 10k sampled commits).
- Sparse checkout of a 1% subtree of linux at any revision takes < 5 s against a local server.
- Codec golden vectors pass on Linux, macOS and Windows.
- `CreateChangeset` p99 < 100 ms (Postgres, 100 changes).

**Implementation notes (phase 1, 2026-10-14).** Where the code differs from the spec above:
- **Git import** uses git's own plumbing (one long-running `cat-file --batch` and one streaming `diff-tree --stdin`) instead of `gix`. Git therefore decides what the history contains, including legacy objects. Switching to `gix` remains an option for speed.
- **`CreateChangesets` batch RPC (added).** Changesets are applied in order, and later ones may name earlier ones as parents. Each batch gets one content check, one concurrent tree flush (through `TreeWriter`) and one metadata transaction. The importer uses it.
- **Fast local blob backend.** `file://` blob URLs use a dedicated local backend (temp file + rename, cached fan-out directories) rather than `object_store::LocalFileSystem`.
- **CLI dirstate** is JSON (`.flux/state.json`). The SQLite dirstate and the filesystem-watcher journal come with fluxd in phase 2.
- **Not yet implemented:**
  - authentication and ACLs (§12);
  - the merge-completeness check (§3.3);
  - chunking of contents over 4 MiB (§3.2). Uploads are single-object, up to 512 MiB.
  - `copy_from` source validation;
  - Windows support in the CLI.

### Phase 2: Scale tier
**Scope:**
- `flux-cache`;
- `fluxd` with the segment store, snapshots and request coalescing;
- consistent hashing;
- admission control;
- `flux-derive` workers and backfill;
- `pathhist_v1`, `blame_v1`, `changed_dirs_v1`;
- `EventService`;
- `flux-bench`;
- the `flux-sim` harness.

**Acceptance:**
- ≥ 99% hit rate at both tiers in a benchmark of 10k simulated CI jobs per hour over 100 nodes.
- Checkout from a warm fluxd takes < 2 s for a 50k-file sparse profile.
- Server CPU per checkout is < 1/50 of a `git clone --filter=blob:none` baseline on the same repo.
- The simulation runs 10k seeds with no invariant violation.

### Phase 3: Write path and git compatibility
**Scope:**
- 3a: `Rebase`, `Merge` and diff3; `flux-land` with batching, hooks, bisect and auto-revert; `flux-mcp`.
- 3b: `git_v1` with the raw sidecar; `flux-git` fetch, then push; `flux-mirror`.

**Acceptance:**
- More than 300 lands/s sustained on a single ref with 10% overlap conflicts, every non-conflicting request landing exactly once.
- Byte-exact SHA-1 round-trip on the fidelity corpus.
- `git clone --filter=blob:none` and incremental `git fetch` work against flux-git.
- A GitHub repo stays in two-way sync for 7 days in a soak test.

### Phase 4: Virtual working copies
**Scope:** `flux-vfs` (Linux FUSE first, then the macOS NFS loopback, then Windows ProjFS) and prefetch hooks.

**Acceptance:**
- Mounting a full checkout of a 10M-file synthetic repo takes < 1 s.
- `status` is O(materialized files).
- `cargo build` and `bazel build` of a subtree produce the same results as on a materialized checkout.

### Phase 5: Production hardening
**Scope:**
- FoundationDB and CockroachDB backends;
- path ACL domains and redaction;
- the audit log;
- compression dictionaries;
- retention and GC of unreferenced changesets;
- the Helm chart;
- SDKs;
- chaos testing in a staging cluster;
- external security review.

**Acceptance:**
- A 30-day staging soak at design-point load (§1.3) meets the §15.1 SLOs.
- The ACL penetration test suite passes: no restricted content is reachable by hash guessing, cache poisoning or tier bypass.

---

## 18. Open questions

1. **Changeset body size limit.** Very large imports, such as vendor drops with 100k files, produce large bodies. Option A: spill to the blob store (current spec). Option B: split into a "file-changes manifest" tree, which complicates hashing.
2. **Copy tracking on create.** Should `CreateChangesetFromTree` infer `copy_from` eagerly (costly) or leave it to `copyidx_v1`? Current choice: lazy.
3. **Multi-region writes.** v1 assumes a single write region; reads are multi-region through caches. FoundationDB and Cockroach enable multi-region later, but CAS latency on `main` across regions needs a design.
4. **Hash agility.** BLAKE3 is fixed for v1. A `v2` context string reserves room for a future migration.
5. **Merge-queue semantics vs. pre-merge CI.** Some adopters will want a gated queue, meaning pre-land CI on the rebased tip. flux-land can support this as a policy (`require_check_on_rebased_tip`), at a throughput cost. This is to be specified in phase 3a.
6. **Monorepo tooling integrations** (Bazel and Buck2 remote file state, Turborepo): mapping `ChangedTargets` to build-graph targets is out of scope for core and belongs in plugins.
