# flux

**Source control for huge monorepos and agent-heavy write workloads.** flux is an open-source take on the ideas behind Anthropic's Scion ([Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control)), Meta's Mononoke/EdenFS and Google's Piper:

- **The server owns history.** Clients never download the whole repository; sparse working copies, server-side log/diff/blame/search, and landing all happen on the server.
- **Changesets are deltas.** Creating one costs O(change), not O(repo). Trees, history indexes and blame are *derived data*.
- **Everything is content-addressed** (BLAKE3, domain separated) and verified on every fetch, by every tier.
- **Refs move only by compare-and-swap**, and **landing is a server-side rebase** with line-level three-way merges, so many people and agents can land on `main` concurrently without a merge queue lock.
- **Git compatible:** import any git repository (incremental and resumable), then `git clone`, `fetch` and `push` straight from the flux server over HTTP or SSH, Git LFS included, with commit SHA-1s preserved.
- **A complete forge:** issues, code review with approvals, code owners and required checks, branch protection, built-in CI that runs GitHub Actions workflows, signed webhooks, releases, forks, notifications, organizations, teams, SSO and two-factor auth.
- **Built for agents:** an MCP server, commits without a clone, batch reads, waiting on checks, and tokens bound to a repository and to `agent/*` branches ([docs/agents.md](docs/agents.md)).
- **Leaving GitHub takes one command.** `flux migrate github` brings history, issues, pull requests (as reviews, with line comments and approvals), labels, milestones, releases, branch protection and users, keeping numbers. `flux mirror` keeps flux in sync during a gradual cutover ([docs/migrating-from-github.md](docs/migrating-from-github.md)).

It ships as one binary for the server, REST and gRPC APIs, MCP endpoint and web UI (`flux-server`), one CLI (`flux`), a per-machine cache daemon (`fluxd`), a CI runner (`flux-runner`) and a git importer (`flux-import-git`).

![Repository browser](docs/img/tree.png)

| | |
|---|---|
| ![Review](docs/img/review.png) | ![Split diff](docs/img/diff-split.png) |
| ![File view](docs/img/file.png) | ![Blame](docs/img/blame.png) |

## Speed

Measured on an M-series laptop, git.git imported (85,558 changesets, 4,857 files at `master`), release build, SQLite + local segments unless noted:

| Operation | Time |
|---|---|
| Web: repository home, directory listing with last-commit per entry | **1–3 ms** server time (warm) |
| Web: 3,000-line file, syntax highlighted | 2–9 ms warm; first view capped at ~50 ms (highlighting finishes in the background) |
| Web: history of one file (`commits/master/builtin/commit.c`) | 5 ms |
| Web: code search over the whole tree (`strbuf_addf`) | 7 ms warm, 330 ms cold |
| CLI: `flux status` on 4,857 files | 3–11 ms |
| CLI: `flux commit` (upload + create + CAS) | ~20 ms |
| CLI: full checkout, 4,857 files / 48 MB | 350 ms |
| Land with server-side rebase + three-way merge | ~10 ms |
| `git clone` of git.git from flux (full history, 420k objects) | **8.7 s** (stock `git` serving its own repo: 14.4 s) |
| git import, git.git, full history | 41–55 s (1,550–2,090 commits/s), SQLite; every commit keeps its SHA-1 |
| Storage for git.git | 312 MB blobs, history index included (git's own pack: 334 MB) |

Every page shows its render time in the footer and in a `Server-Timing` header.

## Quick start

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"

# All-in-one server: SQLite + local segment files, web UI on the same port.
flux-server --data-dir ./data --create-repo demo &
open http://127.0.0.1:7878

# Import a git repository (resumable, incremental), then check it.
flux-import-git --repo demo import /path/to/repo.git
flux-import-git --repo demo verify /path/to/repo.git --sample 200

# Plain git works against the same server (add --ssh-listen 0.0.0.0:2222
# and `flux key add` for ssh://git@host:2222/demo.git).
git clone http://127.0.0.1:7878/demo.git

# Move a GitHub repository over: history, issues, pull requests, releases
# (docs/migrating-from-github.md). `flux mirror` keeps it in sync.
flux migrate github acme/widget --server 127.0.0.1:7878 --to demo2

# Or start from scratch.
flux attach 127.0.0.1:7878 demo work && cd work
echo hello > README.md
flux commit -A -m "First commit" && flux branch main
```

### A day with the CLI

```sh
flux attach 127.0.0.1:7878 demo app --sparse services/api   # sparse by default
flux status                      # parallel stat; honours .gitignore/.fluxignore
flux diff                        # working copy vs parent
flux commit -m "Fix retry loop"  # advances the followed branch by CAS
flux branch feature/retry        # create + follow a branch
flux review create --target main # open a code review for the stack
flux review land 7               # server-side rebase + three-way merge + CAS
flux land --onto main            # or land directly, no review
flux sync                        # fast-forward to the branch's latest

# No checkout needed for any of these:
flux log builtin/commit.c        # file history (indexed, not a full scan)
flux blame src/lib.rs
flux grep -i "todo" src/         # server-side search
flux find cmtc                   # fuzzy file finder
flux cat main~3 Makefile
flux show @ --stat
flux watch                       # live ref moves and review activity
flux daemon enable               # local cache daemon: objects from disk, new revisions prefetched
flux mount /mnt/repo --rev main  # Linux: read-only FUSE view, files load on first read
flux web                         # open this file/directory in the browser

# Collaboration, CI and automation:
flux issue create -t "Login loop" -l bug
flux issue list --label bug
flux checks wait @               # block until CI and other checks settle
flux checks logs 42 --follow
flux key add                     # your SSH key, for git over SSH
flux token create --scope write --for-repo acme/app --ref 'agent/*'
flux api repos/acme/app/reviews  # any REST endpoint (docs/api.md)
claude mcp add flux -- flux mcp  # give Claude Code (or Codex, Cursor) flux tools
```

### The web UI

Served by `flux-server` on the same port as gRPC (HTTP/1.1 and HTTP/2):

- **Code:** tree browser with the last change per entry, README rendering, syntax highlighting, line and range links (`#L10-L20`), raw, blame with age heat-map, file history, permalinks (`y`).
- **Changes:** history grouped by day, changeset pages with unified or split diffs, compare views (`/-/compare/main...feature`).
- **Reviews:** open, comment, approve, close, and **land** from the browser. flux tells you whether landing is a fast-forward or a rebase and reports real conflicts.
- **Edit in the browser:** change or add a file, then commit straight to the branch (landed with a server-side rebase) or to a new branch with a review. Admins can create repositories from the home page.
- **Search** across the whole tree with regex, case and path filters. **Go to file** (`t`) is a fuzzy finder over every path.
- **Branches** with ahead/behind counts, **Activity** (every ref move, updated live over server-sent events).
- **Keyboard:** `t` finder, `/` search, `y` permalink, `Esc`. Light and dark themes.
- Links are prefetched on hover and pages swap in place, so navigation feels instant.

## Production

```sh
# Postgres metadata + S3-compatible blobs (S3, R2, MinIO via AWS_ENDPOINT).
flux-server --meta-url postgres://flux@db/flux --blob-url s3://bucket/flux --auth token

# Or the whole stack with Docker: Postgres + MinIO + flux.
docker compose up -d
docker compose exec flux flux-server token --user admin --admin
```

- **Accounts:** the first account created on a server (`/-/signup`) becomes the site admin. After that, sign-up is `--signup closed` (admins create accounts under Site admin) or `open`. Passwords are hashed with argon2id, and sign-in is throttled per account and client. Web sessions last 30 days and are revoked on sign-out. Personal access tokens (Settings → Access tokens) are scoped `read`, `write` or `all`, can expire, and are shown once. `flux login` asks for your username and password and stores a token (`--password-stdin` for scripts, `--token` to paste one).
- **Organizations and teams:** organizations own repositories (`acme/api`), and have owners and members with a base role. Teams group members and get roles on repositories. User and organization pages live at `/{name}`.
- **Roles:** `read` < `triage` < `write` < `maintain` < `admin` per repository. A user's role is the highest of: their ownership, org ownership, the org's base role, team and direct grants, and visibility (`public`, `internal`, `private`). Token scopes cap it. Private repositories are invisible to people without access, in the UI, the API and git. Security events (sign-ins, failed sign-ins, grants, membership and visibility changes) go to an audit log under Site admin.
- **Server auth mode** (`--auth`): `open` (no authentication; for a single user or a trusted network), `token` (anonymous reads, authenticated writes) or `private` (everything needs a token).
- **Path ACLs:** admins set per-path rules on the settings page (`infra/secrets | ops | ops`). The rules are enforced on gRPC, the web UI and git, and the most specific rule wins. See SPEC §12.2 for the limits of v1.
- **Git:** `/<repo>.git` speaks smart HTTP. Use your token as the password (`git clone http://x:TOKEN@host/repo.git`) or send it as a Bearer header. `--ssh-listen 0.0.0.0:2222` adds git over SSH with account keys (`flux key add`, or Settings → SSH keys); the host key is created on first start. **Git LFS** works over both, and objects are chunked and deduplicated across repositories.
- **Sign-in:** `--github-client-id/--github-client-secret`, `--google-client-id/…`, or any OpenID Connect IdP (`--oidc-issuer`, endpoints discovered automatically) add "Continue with …" buttons. Identities link by verified email or from Settings. Accounts imported from GitHub are claimed the first time their owner signs in with GitHub. **Two-factor auth** uses TOTP with QR enrollment and ten recovery codes; every first factor (password or provider) then asks for a code, as does `flux login`.
- **Branch protection:** per-pattern rules (`main`, `release/*`, `tags/v*`). Options: require reviews, require N approvals from writers, require code-owner approval (`CODEOWNERS`, `.github/CODEOWNERS`), dismiss stale approvals, require status checks, allow or deny force pushes and deletions, a push allow-list, and whether the rules apply to admins. They are enforced on every path a ref can move: git (HTTP and SSH), gRPC, the REST API, landing and the web UI. Without rules, `main`, `master`, `trunk` and `release/*` only fast-forward.
- **CI:** workflows in `.flux-ci/workflows` or `.github/workflows` run on `flux-runner` machines, using a GitHub Actions subset: matrices, `needs`, `if`, expressions, `GITHUB_OUTPUT`/`GITHUB_ENV`, `actions/checkout`, containers and encrypted secrets. Each job reports a commit status that protection can require. See [crates/flux-runner](crates/flux-runner/README.md).
- **Webhooks:** per repository or organization, with GitHub-shaped payloads, `X-Hub-Signature-256` signatures, durable retries and a delivery log with redelivery. Private network targets are refused unless `--webhook-allow-private` is set.
- **Secrets:** CI secrets and 2FA secrets are sealed with ChaCha20-Poly1305 under `--secret-key-file`, which is generated in `--data-dir` mode. Every instance of a deployment needs the same key.
- **Durability:** objects are fsynced before any metadata references them. Segment files are append-only with per-record CRCs, and a torn tail is truncated on restart. The importer resumes after any crash. A test kills the server with `SIGKILL` three times during a git.git import, then resumes, verifies and runs `fsck --full`.
- **Integrity:** `flux-server fsck [--full]` re-hashes changesets, trees and file contents, checks graph closure and refs, and re-derives missing derived data. It is safe to run online.
- **Storage tuning:** `flux-server storage-stats` breaks stored bytes down by kind and codec. `flux-server train-dict REPO` trains zstd dictionaries for small trees and files; new writes use them.
- **Backups:** `flux-server backup DIR` takes a consistent online snapshot of the all-in-one layout (metadata first, then append-only blobs). Restore with `flux-server --data-dir DIR`. For Postgres use `pg_dump` or PITR; for S3, bucket versioning.
- **Caching tier:** `flux-server cache --upstream http://primary:7878 --dir /var/cache/flux` is a regional cache (SPEC §9.2). It serves verified trees and contents from local disk and proxies everything else, web UI included. Clients point at it exactly as they would at the server.
- **Observability:** Prometheus metrics at `/-/metrics` (request counts and latency histograms per surface, blob stats), `/-/healthz`, `/-/readyz` and `Server-Timing` on every response. The server shuts down gracefully on SIGTERM.
- **Web security:** strict CSP (no inline scripts, no third-party code), escaped and sanitized repository Markdown, and raw files served sandboxed with `nosniff`. Same-origin checks run on every form POST.

## Architecture

```
 flux CLI ─┐                        ┌─ flux-server ─ SQLite or Postgres (changesets, refs, reviews, derived ids)
 browsers ─┼─ gRPC + HTTP ─► (cache)┤               └ segment files or S3 (trees, contents; deltas + chunks)
 agents  ──┘                        └─ web UI, events, landing, history indexes
```

| Crate | What it does |
|---|---|
| `flux-core` | ids, deterministic CBOR, paths, changesets, trees; golden vectors pin object identity |
| `flux-store` | metadata stores (SQLite, Postgres; one conformance suite), blob store: segment files with delta chains and FastCDC chunking, or any object store |
| `flux-derive` | `tree_v1` and `lastmod_v1` derivation, tree walk and diff |
| `flux-server` | core operations, commit graph, landing, reviews, issues, protection, CI scheduling, webhooks, search, auth (SSO, 2FA) and ACLs, REST, MCP, gRPC, web UI, git over HTTP and SSH, LFS, cache tier, fsck and backup |
| `flux-client` | Rust client; verifies every object it fetches |
| `flux-cli` | the `flux` command |
| `flux-daemon` | `fluxd`: per-machine cache with automatic routing and prefetch |
| `flux-git` | `flux-import-git` (import and verify) |
| `flux-migrate` | `flux migrate github` and `flux mirror`: history and metadata from GitHub |
| `flux-runner` | the CI job runner |

The full design, with the deviations implemented so far, is in [`docs/SPEC.md`](docs/SPEC.md).

## Development

```sh
cargo test --workspace
FLUX_TEST_POSTGRES_URL=postgres://localhost/flux_test cargo test -p flux-store   # Postgres conformance
FLUX_BLESS=1 cargo test -p flux-core --test golden                               # only for format changes
```

`protoc` is vendored, so no system install is needed.

**Not done yet:** shallow and partial git clones (use `flux attach --sparse`), GitHub Actions marketplace actions beyond the native ones, Pages, Packages and Discussions, and email notifications.

## License

Apache-2.0
