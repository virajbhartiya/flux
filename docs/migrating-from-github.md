# Migrating from GitHub

`flux migrate github` moves a GitHub (or GitHub Enterprise Server) repository to flux: the full git history with every commit SHA-1 unchanged, issues, pull requests (as flux reviews) with their comments, line comments and approvals, labels, milestones, releases with their assets, repository settings and branch protection. Re-running it syncs only what changed, and `flux mirror` keeps doing that on a schedule, so a team can move over gradually.

## Prerequisites

- A flux server and a flux token with admin access to the target repository (`flux login <server>`, or `FLUX_TOKEN`). Creating placeholder accounts for GitHub users (`--create-users`) needs a site admin token.
- For `owner/name` targets, the flux user or organization `owner` must exist. A site admin can also create server-wide repositories (`--to name`).
- `git` on the machine running the migration, and disk space for a bare mirror of the repository (kept in `~/.cache/flux/migrate/<host>/<owner>/<repo>.git`, or `--cache-dir`).
- A GitHub token: `--github-token`, `GITHUB_TOKEN` or `GH_TOKEN`, otherwise `gh auth token` if the GitHub CLI is signed in. A classic token needs `repo` (and `read:org` for organization listings); a fine-grained token needs read access to contents, issues, pull requests and metadata, plus administration (branch protection, webhooks) and secrets (secret names) for a complete report. Without admin access the migration still runs; it reports what it could not read.

The token is sent to git in an `http.extraHeader` through git's environment. It never appears in a URL, a command line, the git config or the logs.

## Migrate one repository

```sh
flux migrate github acme/widget --server https://flux.example.com
```

This creates `acme/widget` on flux if it does not exist (visibility as on GitHub, or `--visibility`), then:

1. **git**: fetches branches, tags and pull request heads (`refs/pull/N/head`, not the `refs/pull/N/merge` test merges) into the local mirror and imports them. Branch `x` becomes flux ref `x`, tag `v1` becomes `tags/v1`, and pull request heads become `pull/N/head` (hidden from branch lists, so reviews of deleted branches and forks keep their commits).
2. **metadata**: labels, milestones, issues, pull requests, releases and placeholder accounts go through the bulk import API (`POST /api/v1/repos/{repo}/import`) in batches. Numbers, authors and timestamps are kept, so `#123` means the same thing on both sides. Links to `https://github.com/acme/widget/issues/N` and `…/pull/N` in bodies and comments are rewritten to the flux pages (`--no-rewrite-links` keeps them).
3. **settings**: description, homepage, topics, default branch, whether issues are enabled and archived state; the default branch's protection.
4. **report**: what was imported, how long each phase took, and what needs a person (see below). `--json` prints it as JSON.

Useful options:

| Option | |
|---|---|
| `--to <name>` | flux repository name (default: the GitHub `owner/repo`) |
| `--github-api <url>` | GitHub Enterprise Server: `https://ghe.example.com/api/v3` |
| `--no-issues`, `--no-reviews`, `--no-releases`, `--no-protection` | skip a part |
| `--create-users` | create placeholder accounts for everyone who appears in the data |
| `--clone-url <url>` | fetch git from somewhere else (a closer mirror, or a local path) |
| `--cache-dir <dir>` | where the git mirror and sync state live |

Then point clones at flux:

```sh
git remote set-url origin https://flux.example.com/acme/widget.git
```

### Large repositories

The git import is incremental and resumable: if it is interrupted, run the command again and it continues from the last imported commit. Metadata uploads are idempotent too, so re-running after a failure never duplicates issues or comments. GitHub's rate limits are respected: the migration waits for `x-ratelimit-reset` when the hourly budget is used up, honours `retry-after` for secondary limits, and retries server errors with backoff. Pull request reviews take one request per pull request, so a first migration of a repository with 10,000 pull requests needs a few hours of rate limit; later syncs only fetch what changed.

## Migrate an organization

```sh
flux migrate github --org acme --to-org acme --server https://flux.example.com
```

Every repository of the GitHub organization (or user) is migrated in turn to `<to-org>/<name>`, with a progress line per repository. Archived repositories are skipped unless `--include-archived`; they are archived on flux too. One repository failing does not stop the others; the command exits non-zero at the end and names the failures.

## Gradual cutover with a mirror

```sh
flux mirror acme/widget --server https://flux.example.com --interval 5m
```

`flux mirror` runs the same sync in a loop: new commits, branches and tags (and deleted branches), new and updated issues and pull requests, new comments, releases and settings. It uses `since=` for issues and comments, conditional requests (ETags) for labels, milestones, releases and collaborators, and the git importer skips commits it already has, so an idle sync costs a handful of requests. It logs one line per sync, stops on Ctrl-C, and exits non-zero after `--max-failures` (default 5) failed syncs in a row. `--once` syncs once, for cron or CI.

A typical cutover:

1. Start a mirror. People keep working on GitHub, and flux CI starts running the same workflows, so you can compare results.
2. Try flux for reading, reviewing and CI.
3. At cutover, make the GitHub repository read-only (archive it, or restrict pushes), let the mirror run once more, then stop it.
4. Run `flux migrate github acme/widget` once more. Mirrors run with `enforce_admins` off on flux (the mirror pushes as an admin while GitHub stays the gatekeeper); the final migrate applies GitHub's setting.
5. Switch remotes, re-enter secrets, recreate webhooks (see the report), and tell people to sign in with GitHub.

GitHub stays the source of truth while mirroring: changes made only on flux to refs that GitHub also has are overwritten by the next sync.

Sync state lives next to the git mirror (`<repo>.state.json`): the last sync time, cached ETags, the users already created and the refs imported. Deleting it makes the next run a full re-sync, which is safe.

## What maps to what

| GitHub | flux |
|---|---|
| Repository, description, topics, homepage, default branch | Same (`PATCH /api/v1/repos/{repo}`) |
| Commits, branches, tags | Same commits with the same SHA-1s; tags as `tags/<name>`. Annotated tags are imported as the commit they tag. |
| Issues, comments, labels, milestones, assignees, locked, close reason | Issues with the same numbers, authors and timestamps |
| Pull requests | Reviews with the same numbers (`/-/reviews/N`): title, body, draft, labels, assignees, requested reviewers, head and target branch. Merged pull requests are `landed`, at their merge commit. |
| PR conversation comments | Review comments |
| PR line comments | Line comments on the same path and line (lines on the old side of the diff are negative) |
| PR reviews: approved / changes requested / commented | `approved` / `changes_requested` / `comment` events with their text |
| Releases and assets | Releases on the same tags, assets uploaded (up to 512 MiB each) |
| GitHub Actions (`.github/workflows/*.yml`) | flux CI reads `.github/workflows` directly: `push`, `pull_request` (reviews) and `workflow_dispatch` triggers, matrices, `needs`, `if`, containers, `actions/checkout`. Other actions run on runners that have a handler for them. The report lists triggers, actions, service containers and reusable workflows that need attention. |
| Branch protection (default branch) | A flux branch rule: required approvals, required status checks (same names; flux CI reports `<workflow> / <job>`, like Actions), code owner reviews, dismiss stale approvals, enforce for admins, allow force pushes and deletions. Push restrictions and rulesets are reported, not migrated. |
| `CODEOWNERS` | Used as is (`CODEOWNERS`, `.github/CODEOWNERS` or `docs/CODEOWNERS`) |
| Webhooks | Recreate them (`POST /api/v1/repos/{repo}/hooks`). flux sends GitHub-shaped payloads (`push`, `pull_request`, `issues`, `issue_comment`, `status`, `release`, …) with `X-GitHub-Event`, `X-GitHub-Delivery` and `X-Hub-Signature-256`, so receivers usually work unchanged. GitHub does not reveal hook secrets; the report lists every hook. |
| Actions secrets | Must be re-entered: `PUT /api/v1/repos/{repo}/secrets/{name}` with `{"value": …}`. GitHub does not reveal secret values; the report lists their names. |
| Actions variables | Not migrated (listed in the report) |
| Users | Optional placeholder accounts (`--create-users`), linked to the GitHub account id |
| Pages, Packages, Discussions, Projects, wiki, stars, forks | Not migrated |

Pull requests whose head commit no longer exists (a deleted fork whose `refs/pull/N/head` GitHub has dropped) are imported against their target branch and listed in the report.

## Claiming placeholder accounts

With `--create-users`, everyone who opened, commented on, reviewed or was assigned to an issue or pull request, or is a collaborator, gets a placeholder account with their GitHub login (bots and deleted accounts excluded), linked to their numeric GitHub user id. Their issues, comments and reviews are attributed to that account.

When the server has GitHub sign-in configured, a person who signs in with GitHub is matched by GitHub user id (not by login or email, so renamed GitHub accounts still match) and takes over the placeholder: same login, same history, no password to reset. Placeholders have no password, so they cannot sign in any other way until claimed. Without `--create-users`, authors are kept as plain names on issues and reviews.

## Troubleshooting

- **`placeholder accounts need a site admin token`**: use a site admin's token, or drop `--create-users`.
- **Rate limited**: the migration waits and continues; with no token at all, GitHub allows only 60 requests an hour, so pass one.
- **`pull request #N was skipped: none of its commits are in flux`**: the git import did not run or the target branch is missing; re-run the migration.
- **Starting over**: deleting the flux repository and the state file (`~/.cache/flux/migrate/<host>/<owner>/<repo>.state.json`) gives a fresh migration; the git mirror can stay.
