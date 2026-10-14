# flux

An open-source, horizontally scalable source-control system for very large monorepos and agent-heavy write workloads. It is inspired by Anthropic's Scion ([Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control)) and Meta's Mononoke/EdenFS, and it can import git history.

- **The server owns history.** Changesets are created, diffed, logged and read on the server. Clients fetch only the objects they need.
- **Changesets are deltas.** Creation costs O(change), not O(repo size). Trees are *derived data*.
- **Everything is content-addressed** with BLAKE3 and domain separation, and every object is verified on fetch. There are no packfiles and no `gc`.
- **Refs move only by compare-and-swap.** Agents can work purely with changeset ids.
- **Sparse checkouts are the default.** One RPC expands the whole sparse tree on the server.

The full design is in [`docs/SPEC.md`](docs/SPEC.md).

## Status: phase 1 (core vertical slice)

| Component | State |
|---|---|
| `flux-core`: ids, deterministic CBOR, paths, changesets, trees | ✅ golden vectors in `testdata/codec/` |
| `flux-store`: SQLite and Postgres metadata; local/S3/memory blobs | ✅ one conformance suite, run on both backends |
| `flux-derive`: `tree_v1` derivation, walk, diff | ✅ property-tested against a naive oracle |
| `flux-server`: gRPC Object/Repo/Write/Admin services | ✅ |
| `flux-client`: Rust client, verifies every object it fetches | ✅ |
| `flux` CLI: attach, checkout, status, diff, commit, log, show, cat, ls, refs, set-ref, sparse | ✅ macOS/Linux |
| `flux-import-git`: incremental import plus `verify` against `git ls-tree` | ✅ |
| fluxd / flux-cache tiers, skip lists, blame, landing, git frontend, VFS | ⏳ phases 2–5 |

## Quick start

```sh
cargo build --release

# All-in-one server: SQLite + local blob directory.
./target/release/flux-server --data-dir ./data --create-repo demo &

# Import a git repository (resumable and incremental), then check it.
./target/release/flux-import-git --repo demo import /path/to/repo.git
./target/release/flux-import-git --repo demo verify /path/to/repo.git --sample 200

# Sparse working copy.
./target/release/flux attach http://127.0.0.1:7878 demo wc --sparse src/app
cd wc && echo hi >> src/app/main.rs
flux status && flux diff
flux commit -m "change"          # advances `main` by compare-and-swap
flux log -n 5
flux cat main~3 README.md        # no checkout needed
```

For production storage, point the server at Postgres and S3-compatible storage. MinIO and R2 work through `AWS_ENDPOINT`.

```sh
flux-server --meta-url postgres://user@db/flux --blob-url s3://bucket/flux
```

## Development

```sh
cargo test --workspace
# Also run the store conformance suite against Postgres:
FLUX_TEST_POSTGRES_URL=postgres://localhost/flux_test cargo test -p flux-store
# Regenerate golden vectors (only for an intentional format change):
FLUX_BLESS=1 cargo test -p flux-core --test golden
```

`protoc` is vendored through the build script, so no system install is needed.

## License

Apache-2.0
