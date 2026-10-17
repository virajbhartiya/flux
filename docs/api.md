# REST API

Base URL: `https://<server>/api/v1`. JSON in and out. The response shapes follow GitHub's API where that helps tools migrate. Timestamps are Unix milliseconds.

**Authentication.** Send `Authorization: Bearer <token>`; `token <token>` also works, as does HTTP Basic with the token as the password. Browser sessions work for reads, and for writes that come from the same origin. Tokens come from Settings → Access tokens, `flux login`, or `POST /user/tokens`.

**Errors** look like `{"message": "...", "error": "not_found|invalid|conflict|unauthenticated|forbidden|internal"}`, with status 404, 422, 409, 401 or 403. Private repositories you cannot read return 404.

**Pagination.** `?per_page=` (default 30, max 100) and `?page=`. Issue lists also return the total in `x-total-count`.

Repository paths take either `repos/{owner}/{name}` or `repos/{name}`.

## Account

| | |
|---|---|
| `GET /user` | You: login, kind, token scopes, unread notifications, 2FA |
| `GET/POST /user/keys`, `DELETE /user/keys/{id}` | SSH keys |
| `POST /user/tokens` `{label, scope: read\|write\|all, repos[], refs[], expires_in_days}` | Mint a token (repository- and ref-bound for agents) |
| `GET /notifications?all=1`, `PATCH /notifications[/{id}]` | Inbox; mark read |

## Repositories

| | |
|---|---|
| `GET /repos`, `POST /repos {name, visibility, description, default_branch}` | List readable; create (`owner/name`) |
| `GET/PATCH/DELETE /repos/{r}` | Info; settings (name, description, default branch, visibility, topics, archived, has_issues, secret_scanning); delete |
| `GET /repos/{r}/refs?prefix=`, `/branches`, `/tags` | Refs |
| `GET/PUT/DELETE /repos/{r}/refs/{name}` `{target, expect?, force?}` | Read or move a ref (compare-and-swap; protection applies) |
| `GET /repos/{r}/commits?rev=&path=&since=&first_parent=1&files=1&git=1` | History |
| `GET /repos/{r}/commits/{rev}?patch=1` | One changeset, its files, combined status, and optionally the patch |
| `GET /repos/{r}/tree/{rev}/{path}?recursive=1` | Directory listing or every file |
| `GET /repos/{r}/contents/{path}?ref=&start_line=&end_line=` | File as JSON (UTF-8, or base64 for binary) |
| `GET /repos/{r}/raw/{rev}/{path}` | Raw bytes |
| `POST /repos/{r}/files` `{rev, paths[], files[{path,start_line,end_line}], max_bytes}` | **Batch read** |
| `POST /repos/{r}/changes` | **Commit without a clone** (see [agents.md](agents.md)) |
| `GET /repos/{r}/compare/{base}...{head}?format=patch` | Commits and unified diff (from the merge base) |
| `GET /repos/{r}/search?q=&regex=1&case_sensitive=1&path=&rev=` | Code search |
| `GET /repos/{r}/find?q=` | Fuzzy file finder |
| `GET /repos/{r}/blame/{rev}/{path}` | Line ranges with their changesets |
| `GET /repos/{r}/context` | **Orientation for agents** |
| `POST /repos/{r}/merge-check {head, target}` | Would it land cleanly? (writes nothing) |
| `GET /repos/{r}/archive/{rev}.tar.gz` or `.zip` | Source archive |
| `GET/POST /repos/{r}/forks` | List forks; fork to `{you}/{name}` or `{name}` |
| `PUT /repos/{r}/subscription {state: watching\|ignored\|none}` | Watch a repository |

## Collaboration

| | |
|---|---|
| `GET/POST /repos/{r}/issues` (`state`, `label`, `assignee`, `author`, `milestone`, `q`, `sort`) | Issues |
| `GET/PATCH /repos/{r}/issues/{n}` | View (with timeline); edit title, body, state, state_reason, labels, assignees, milestone, locked |
| `GET/POST /repos/{r}/issues/{n}/comments`, `PATCH/DELETE /repos/{r}/issues/comments/{id}` | Comments |
| `PUT /repos/{r}/issues/{n}/labels`, `/assignees`, `/subscription` | Labels, assignees, thread subscription |
| `GET/POST /repos/{r}/labels`, `PATCH/DELETE /repos/{r}/labels/{name}` | Labels |
| `GET/POST /repos/{r}/milestones`, `PATCH/DELETE …/{n}` | Milestones |
| `GET/POST /repos/{r}/reviews` (alias `pulls`) `{title, body, head, target, draft, reviewers, labels, source}` | Reviews; `source` opens one from a fork |
| `GET/PATCH /repos/{r}/reviews/{n}` | Detail (stack, timeline, gate, checks); edit, close/reopen, new head, draft |
| `POST /repos/{r}/reviews/{n}/comments {body, path?, line?}` | Comment (line comments: `line > 0` new side, `< 0` old side) |
| `POST /repos/{r}/reviews/{n}/approve`, `/request-changes`, `/dismiss` | Review decisions |
| `GET /repos/{r}/reviews/{n}/gate` | What blocks landing (approvals, checks, code owners, draft) |
| `POST /repos/{r}/reviews/{n}/land {method: rebase\|squash}` | Land |
| `GET /repos/{r}/reviews/{n}/diff` | Unified diff |
| `GET/PUT /repos/{r}/protection {rules: [...] \| null}` | Branch protection (`null` restores the defaults) |

## Checks, CI and webhooks

| | |
|---|---|
| `POST /repos/{r}/statuses/{rev} {state, context, description, target_url}` | Report a status (`pending`, `success`, `failure`, `error`) |
| `GET /repos/{r}/commits/{rev}/status?wait=<s>&min=<n>` | Combined status; `wait` blocks until settled |
| `GET /repos/{r}/runs?rev=`, `POST /repos/{r}/runs {workflow, ref}` | CI runs; manual dispatch |
| `GET /repos/{r}/runs/{id}`, `POST …/cancel`, `POST …/rerun` | One run |
| `GET /repos/{r}/jobs/{id}/log?after=<seq>` | Incremental log (`x-flux-log-seq`, `x-flux-job-state`) |
| `GET/PUT/DELETE /repos/{r}/secrets/{name}`, `/orgs/{o}/secrets/…` | CI secrets (write-only) |
| `GET/POST /runners`, `DELETE /runners/{id}` | Runners (site admin) |
| `GET/POST /repos/{r}/hooks`, `GET/PATCH/DELETE …/{id}` | Webhooks; also `/orgs/{o}/hooks` |
| `GET …/hooks/{id}/deliveries`, `POST …/deliveries/{d}/redeliver`, `POST …/hooks/{id}/ping` | Deliveries |
| `GET /events?repo=` | Server-sent events (ref moves, reviews, issues, statuses, CI) |

Webhook payloads use GitHub's shapes and event names (`push`, `pull_request`, `pull_request_review`, `issues`, `issue_comment`, `status`, `release`, `workflow_run`, `ping`). They are signed with `X-Hub-Signature-256`, carry both `X-Flux-*` and `X-GitHub-*` headers, are stored durably and retried with backoff for about six hours. By default they cannot target private networks; use `--webhook-allow-private` to allow it.

## Releases

| | |
|---|---|
| `GET/POST /repos/{r}/releases` `{tag, name, body, draft, prerelease, target}` | Creating one also creates the tag |
| `GET /repos/{r}/releases/latest`, `/releases/tags/{tag}`, `GET/PATCH/DELETE /releases/{id}` | |
| `POST /repos/{r}/releases/{id}/assets?name=` (raw body), `DELETE …/assets/{aid}` | Assets |
| `GET /repos/{r}/releases/download/{tag}/{name}` | Download (also at `/{r}/-/releases/download/…`) |

## Bulk import

`POST /repos/{r}/import` (repository admin; placeholder users need a site admin) takes `{users, labels, milestones, issues, reviews, releases}`. Numbers, authors and timestamps are kept, and re-sending is idempotent. `flux migrate github` uses this endpoint; see [migrating-from-github.md](migrating-from-github.md).

## MCP

`POST /api/mcp` speaks the Model Context Protocol (JSON-RPC, streamable HTTP). See [agents.md](agents.md).
