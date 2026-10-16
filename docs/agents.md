# flux for coding agents

flux is built for many agents writing to one repository at once. Agents get:

- **MCP built in.** `/api/mcp` is a Model Context Protocol server, so Claude Code, Codex, Cursor and other MCP clients use flux tools directly. `flux mcp` bridges stdio clients.
- **Commits without a clone.** One call creates a changeset from file edits (full contents, base64, or exact search/replace), moves a branch, and opens a review or lands it. There is no checkout, no `git add`, and no push negotiation; it takes milliseconds on any repository size.
- **Batch reads.** Up to 1,000 files at one revision in one request, with optional line ranges.
- **A one-call orientation.** `repo_context` returns agent instructions (`AGENTS.md`, `CLAUDE.md`), the README, the layout, languages, recent changes, open work, workflows and branch protection.
- **Waiting instead of polling.** Waiting on checks blocks until CI settles. Merge checks list conflicting files without writing anything.
- **Tokens that keep an agent in its lane.** A token can be bound to repositories (`repo:<id>`) and to ref patterns (`ref:agent/*`). Branch protection, path rules and code owners apply to agents exactly as they do to people.
- **Concurrency without a merge queue.** Landing is a server-side rebase with three-way merges and a compare-and-swap on the ref, so hundreds of agents can land on `main` concurrently.

## Connect an agent

Mint a token for the agent. Write access is limited to its own branches on one repository; it can read but not move anything else:

```sh
flux token create --label claude --scope write --for-repo acme/app --ref 'agent/*'
```

### Claude Code

```sh
# Over HTTP (no local install):
claude mcp add --transport http flux https://flux.example.com/api/mcp \
  --header "Authorization: Bearer flx_…"

# Or through the CLI (uses the token from `flux login` or FLUX_TOKEN):
claude mcp add flux -- flux mcp --server https://flux.example.com
```

### Codex

```toml
# ~/.codex/config.toml
[mcp_servers.flux]
command = "flux"
args = ["mcp", "--server", "https://flux.example.com"]
env = { FLUX_TOKEN = "flx_…" }
```

### Cursor and other clients

Use `https://<server>/api/mcp` (streamable HTTP) with an `Authorization: Bearer` header, or the stdio command `flux mcp --server <url>`.

## Tools

| Tool | What it does |
|---|---|
| `repo_context` | Orientation in one call; start here |
| `read_files` | Many files at once, with line ranges |
| `list_tree`, `find_files`, `search_code` | Layout, fuzzy paths, literal or regex search |
| `history`, `show_changeset`, `blame`, `compare` | History, diffs and line authorship |
| `commit_changes` | Create a changeset from edits; optionally branch, review or land |
| `merge_check` | Would this land cleanly? Lists conflicting files |
| `checks`, `ci_runs`, `ci_log` | CI status (`wait_seconds` blocks until settled), runs and logs |
| `list_reviews`, `get_review`, `create_review`, `review_action`, `comment` | Code review, including line comments, approvals and landing |
| `list_issues`, `get_issue`, `create_issue`, `update_issue` | Issue triage |

Every tool maps onto the REST API, so permissions behave identically in both.

## A typical loop

1. `repo_context { repo }`: read the instructions, layout and protection.
2. `search_code` and `read_files`: gather context in a few calls.
3. `commit_changes { branch: "agent/fix-login", files: [{ path, edits: [{ old, new }] }], review: { title } }`: one call creates the change and opens the review.
4. `checks { rev: "agent/fix-login", wait_seconds: 600 }`: block until CI finishes.
5. If CI fails, `ci_log`, then fix with another `commit_changes` on the same branch (it fast-forwards) and `review_action { action: "update", head: "agent/fix-login" }`.
6. A human approves, or `review_action { action: "land", method: "squash" }` lands it if the rules allow.

## REST equivalents

```sh
# Batch read
curl -H "Authorization: Bearer $T" -X POST $FLUX/api/v1/repos/acme/app/files \
  -d '{"paths": ["README.md", "src/lib.rs"], "files": [{"path": "src/main.rs", "start_line": 1, "end_line": 40}]}'

# Commit without a clone, open a review
curl -H "Authorization: Bearer $T" -X POST $FLUX/api/v1/repos/acme/app/changes -d '{
  "branch": "agent/fix-login",
  "message": "Fix login redirect",
  "files": [
    {"path": "src/auth.rs", "edits": [{"old": "Redirect::to(\"/\")", "new": "Redirect::to(next)"}]},
    {"path": "tests/auth.rs", "content": "…"},
    {"path": "old.rs", "delete": true}
  ],
  "review": {"title": "Fix login redirect", "reviewers": ["alice"]}
}'

# Wait for checks (blocks up to 10 minutes)
curl -H "Authorization: Bearer $T" "$FLUX/api/v1/repos/acme/app/commits/agent/fix-login/status?wait=600"
```

Edits are exact: `old` must match once, unless you pass `"all": true`. A failed match returns a 422 that names the file and the edit. Writing to a branch that moved since your base returns 409: re-read and retry, or omit `base` to build on the branch's current head.
