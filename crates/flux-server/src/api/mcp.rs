//! An MCP server (Model Context Protocol, streamable HTTP transport) so
//! coding agents — Claude Code, Codex, Cursor and others — work with flux
//! directly: read and search code, commit multi-file changes without a
//! clone, open and land reviews, wait for checks, triage issues.
//!
//! Connect with the server URL `https://<host>/api/mcp` and an
//! `Authorization: Bearer <token>` header (a token bound to repositories
//! and `ref:agent/*` keeps an agent in its lane). `flux mcp` bridges stdio
//! clients to it.
//!
//! Each tool is a thin mapping onto the REST API, so permissions, path
//! rules and branch protection apply exactly as they do everywhere else.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use super::Ctx;
use crate::core::Flux;

const PROTOCOL: &str = "2025-06-18";

struct Tool {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Value,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

fn repo_prop() -> Value {
    json!({ "type": "string", "description": "Repository name, e.g. acme/app" })
}

fn rev_prop() -> Value {
    json!({ "type": "string", "description": "Branch, tag, changeset id or git SHA (default: the default branch)" })
}

const TOOLS: &[Tool] = &[
    Tool { name: "list_repos", description: "List repositories you can read.", schema: || obj(json!({}), &[]) },
    Tool {
        name: "repo_context",
        description: "One-call orientation for a repository: description, default branch, agent instructions (AGENTS.md/CLAUDE.md), README, top-level layout, languages, recent changes, open work, CI workflows and branch protection. Call this first.",
        schema: || obj(json!({ "repo": repo_prop(), "rev": rev_prop() }), &["repo"]),
    },
    Tool {
        name: "read_files",
        description: "Read many files at one revision in one call. Prefer this over reading files one at a time. Optional line ranges per file.",
        schema: || {
            obj(
                json!({
                    "repo": repo_prop(), "rev": rev_prop(),
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "files": { "type": "array", "items": { "type": "object", "properties": { "path": { "type": "string" }, "start_line": { "type": "integer" }, "end_line": { "type": "integer" } }, "required": ["path"] } },
                    "max_bytes": { "type": "integer", "description": "Per-file limit (default 1 MiB)" }
                }),
                &["repo"],
            )
        },
    },
    Tool {
        name: "list_tree",
        description: "List a directory, or every file under it with recursive=true.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "rev": rev_prop(), "path": { "type": "string" }, "recursive": { "type": "boolean" } }),
                &["repo"],
            )
        },
    },
    Tool {
        name: "search_code",
        description: "Search file contents (literal by default, regex=true for regular expressions). Returns matching lines with paths and line numbers. Fast on large repositories.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "query": { "type": "string" }, "regex": { "type": "boolean" }, "case_sensitive": { "type": "boolean" },
                    "path": { "type": "string", "description": "Only under this directory" }, "rev": rev_prop(), "limit": { "type": "integer" } }),
                &["repo", "query"],
            )
        },
    },
    Tool {
        name: "find_files",
        description: "Fuzzy-find files by name or path fragment.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "query": { "type": "string" }, "rev": rev_prop(), "limit": { "type": "integer" } }),
                &["repo", "query"],
            )
        },
    },
    Tool {
        name: "history",
        description: "Recent changesets, optionally for one path.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "rev": rev_prop(), "path": { "type": "string" }, "limit": { "type": "integer" } }),
                &["repo"],
            )
        },
    },
    Tool {
        name: "show_changeset",
        description: "One changeset: message, author, files; patch=true adds the unified diff.",
        schema: || obj(json!({ "repo": repo_prop(), "rev": rev_prop(), "patch": { "type": "boolean" } }), &["repo", "rev"]),
    },
    Tool {
        name: "blame",
        description: "Who last changed each line of a file, and in which changeset.",
        schema: || obj(json!({ "repo": repo_prop(), "path": { "type": "string" }, "rev": rev_prop() }), &["repo", "path"]),
    },
    Tool {
        name: "compare",
        description: "Unified diff and commits between two revisions (three-dot: from their merge base).",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "base": { "type": "string" }, "head": { "type": "string" }, "path": { "type": "string" } }),
                &["repo", "base", "head"],
            )
        },
    },
    Tool {
        name: "commit_changes",
        description: "Create a changeset from file edits in one call — no clone or working copy. Each file op is one of: content (full text), content_base64, edits ([{old, new}] exact search/replace; old must match once unless all=true), delete=true, move_from, symlink, executable. Optionally put it on a branch (created or fast-forwarded), open a review, or land it.",
        schema: || {
            obj(
                json!({
                    "repo": repo_prop(),
                    "message": { "type": "string" },
                    "branch": { "type": "string", "description": "Branch to create or advance, e.g. agent/fix-login" },
                    "base": { "type": "string", "description": "Parent revision (default: the branch, else the default branch)" },
                    "files": { "type": "array", "items": { "type": "object", "properties": {
                        "path": { "type": "string" }, "content": { "type": "string" }, "content_base64": { "type": "string" },
                        "edits": { "type": "array", "items": { "type": "object", "properties": { "old": { "type": "string" }, "new": { "type": "string" }, "all": { "type": "boolean" } }, "required": ["old", "new"] } },
                        "delete": { "type": "boolean" }, "move_from": { "type": "string" }, "symlink": { "type": "string" }, "executable": { "type": "boolean" }
                    }, "required": ["path"] } },
                    "review": { "type": "object", "properties": { "title": { "type": "string" }, "body": { "type": "string" }, "target": { "type": "string" }, "draft": { "type": "boolean" }, "reviewers": { "type": "array", "items": { "type": "string" } } }, "required": ["title"] },
                    "land": { "type": "string", "description": "Land directly on this branch (subject to branch protection)" }
                }),
                &["repo", "message", "files"],
            )
        },
    },
    Tool {
        name: "merge_check",
        description: "Would a revision land cleanly on a branch? Lists conflicting files without changing anything.",
        schema: || {
            obj(json!({ "repo": repo_prop(), "head": { "type": "string" }, "target": { "type": "string" } }), &["repo", "head"])
        },
    },
    Tool {
        name: "checks",
        description: "Combined CI/status checks for a revision. wait_seconds blocks until checks settle (or time out) — use it instead of polling.",
        schema: || {
            obj(json!({ "repo": repo_prop(), "rev": rev_prop(), "wait_seconds": { "type": "integer" } }), &["repo", "rev"])
        },
    },
    Tool {
        name: "ci_runs",
        description: "CI runs for a repository or one revision, with job states.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "rev": rev_prop(), "run": { "type": "integer", "description": "Show one run with its jobs" } }),
                &["repo"],
            )
        },
    },
    Tool {
        name: "ci_log",
        description: "A CI job's log output.",
        schema: || obj(json!({ "repo": repo_prop(), "job": { "type": "integer" } }), &["repo", "job"]),
    },
    Tool {
        name: "list_reviews",
        description: "Reviews (pull requests): open by default.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "state": { "type": "string", "enum": ["open", "closed", "landed", "all"] } }),
                &["repo"],
            )
        },
    },
    Tool {
        name: "get_review",
        description: "A review with its stack, discussion, line comments, approvals, landing gate and checks; diff=true returns the patch instead.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "number": { "type": "integer" }, "diff": { "type": "boolean" } }),
                &["repo", "number"],
            )
        },
    },
    Tool {
        name: "create_review",
        description: "Open a review for a revision (usually a branch you committed to).",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "title": { "type": "string" }, "body": { "type": "string" }, "head": { "type": "string" }, "target": { "type": "string" }, "draft": { "type": "boolean" }, "reviewers": { "type": "array", "items": { "type": "string" } } }),
                &["repo", "title", "head"],
            )
        },
    },
    Tool {
        name: "review_action",
        description: "approve, request_changes, close, reopen, ready (leave draft), update (new head), or land (method rebase|squash) a review.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "number": { "type": "integer" }, "action": { "type": "string", "enum": ["approve", "request_changes", "close", "reopen", "ready", "update", "land"] },
                    "body": { "type": "string" }, "head": { "type": "string" }, "method": { "type": "string", "enum": ["rebase", "squash"] } }),
                &["repo", "number", "action"],
            )
        },
    },
    Tool {
        name: "comment",
        description: "Comment on an issue or review; path+line anchors a review comment to a diff line.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "number": { "type": "integer" }, "body": { "type": "string" }, "path": { "type": "string" }, "line": { "type": "integer" } }),
                &["repo", "number", "body"],
            )
        },
    },
    Tool {
        name: "list_issues",
        description: "Issues, filtered by state, text, label or assignee.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "state": { "type": "string" }, "q": { "type": "string" }, "label": { "type": "string" }, "assignee": { "type": "string" } }),
                &["repo"],
            )
        },
    },
    Tool {
        name: "get_issue",
        description: "An issue with its full timeline.",
        schema: || obj(json!({ "repo": repo_prop(), "number": { "type": "integer" } }), &["repo", "number"]),
    },
    Tool {
        name: "create_issue",
        description: "Open an issue.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "title": { "type": "string" }, "body": { "type": "string" }, "labels": { "type": "array", "items": { "type": "string" } } }),
                &["repo", "title"],
            )
        },
    },
    Tool {
        name: "update_issue",
        description: "Close/reopen, retitle, relabel or assign an issue.",
        schema: || {
            obj(
                json!({ "repo": repo_prop(), "number": { "type": "integer" }, "state": { "type": "string", "enum": ["open", "closed"] }, "title": { "type": "string" }, "body": { "type": "string" },
                    "labels": { "type": "array", "items": { "type": "string" } }, "assignees": { "type": "array", "items": { "type": "string" } } }),
                &["repo", "number"],
            )
        },
    },
];

fn rpc_ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_err(id: &Value, code: i64, msg: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
}

fn s<'a>(a: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn enc(p: &str) -> String {
    p.split('/').map(|seg| url::form_urlencoded::byte_serialize(seg.as_bytes()).collect::<String>()).collect::<Vec<_>>().join("/")
}

/// Map a tool call to (method, API path, query, JSON body).
fn plan(name: &str, a: &Map<String, Value>) -> Result<(Method, String, Vec<(String, String)>, Option<Value>), String> {
    let repo = || s(a, "repo").map(str::to_string).ok_or("repo is required");
    let r = |tail: &str| -> Result<String, String> { Ok(format!("repos/{}/{tail}", repo()?)) };
    let mut q: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            q.push((k.to_string(), v));
        }
    };
    let str_of = |k: &str| s(a, k).map(str::to_string);
    let num_of = |k: &str| a.get(k).and_then(Value::as_i64);
    let bool_of = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
    let number = || num_of("number").ok_or("number is required");
    Ok(match name {
        "list_repos" => (Method::GET, "repos".into(), q, None),
        "repo_context" => {
            put("rev", str_of("rev"));
            (Method::GET, r("context")?, q, None)
        }
        "read_files" => (
            Method::POST,
            r("files")?,
            q,
            Some(
                json!({ "rev": str_of("rev").unwrap_or_default(), "paths": a.get("paths").cloned().unwrap_or(json!([])), "files": a.get("files").cloned().unwrap_or(json!([])), "max_bytes": a.get("max_bytes") }),
            ),
        ),
        "list_tree" => {
            if bool_of("recursive") {
                put("recursive", Some("1".into()));
            }
            let rev = str_of("rev").unwrap_or_default();
            let path = str_of("path").unwrap_or_default();
            let tail = if rev.is_empty() && path.is_empty() {
                "tree".to_string()
            } else {
                format!("tree/{}/{}", enc(if rev.is_empty() { "HEAD" } else { &rev }), enc(&path))
            };
            if rev.is_empty() && !path.is_empty() {
                return Ok((Method::GET, r(&format!("contents/{}", enc(&path)))?, q, None));
            }
            (Method::GET, r(&tail)?, q, None)
        }
        "search_code" => {
            put("q", str_of("query"));
            put("rev", str_of("rev"));
            put("path", str_of("path"));
            put("limit", num_of("limit").map(|n| n.to_string()));
            if bool_of("regex") {
                put("regex", Some("1".into()));
            }
            if bool_of("case_sensitive") {
                put("case_sensitive", Some("1".into()));
            }
            (Method::GET, r("search")?, q, None)
        }
        "find_files" => {
            put("q", str_of("query"));
            put("rev", str_of("rev"));
            put("limit", num_of("limit").map(|n| n.to_string()));
            (Method::GET, r("find")?, q, None)
        }
        "history" => {
            put("rev", str_of("rev"));
            put("path", str_of("path"));
            put("per_page", Some(num_of("limit").unwrap_or(20).to_string()));
            (Method::GET, r("commits")?, q, None)
        }
        "show_changeset" => {
            if bool_of("patch") {
                put("patch", Some("1".into()));
            }
            (Method::GET, r(&format!("commits/{}", enc(&str_of("rev").ok_or("rev is required")?)))?, q, None)
        }
        "blame" => {
            let rev = str_of("rev").unwrap_or_else(|| "HEAD".into());
            (Method::GET, r(&format!("blame/{}/{}", enc(&rev), enc(&str_of("path").ok_or("path is required")?)))?, q, None)
        }
        "compare" => {
            put("path", str_of("path"));
            let spec = format!("{}...{}", str_of("base").ok_or("base is required")?, str_of("head").ok_or("head is required")?);
            (Method::GET, r(&format!("compare/{}", enc(&spec)))?, q, None)
        }
        "commit_changes" => {
            let mut body = a.clone();
            body.remove("repo");
            (Method::POST, r("changes")?, q, Some(Value::Object(body)))
        }
        "merge_check" => {
            (Method::POST, r("merge-check")?, q, Some(json!({ "head": str_of("head"), "target": str_of("target") })))
        }
        "checks" => {
            put("wait", num_of("wait_seconds").map(|n| n.clamp(0, 600).to_string()));
            (Method::GET, r(&format!("commits/{}/status", enc(&str_of("rev").ok_or("rev is required")?)))?, q, None)
        }
        "ci_runs" => match num_of("run") {
            Some(id) => (Method::GET, r(&format!("runs/{id}"))?, q, None),
            None => {
                put("rev", str_of("rev"));
                (Method::GET, r("runs")?, q, None)
            }
        },
        "ci_log" => (Method::GET, r(&format!("jobs/{}/log", num_of("job").ok_or("job is required")?))?, q, None),
        "list_reviews" => {
            put("state", str_of("state"));
            (Method::GET, r("reviews")?, q, None)
        }
        "get_review" => {
            let n = number()?;
            if bool_of("diff") {
                (Method::GET, r(&format!("reviews/{n}/diff"))?, q, None)
            } else {
                (Method::GET, r(&format!("reviews/{n}"))?, q, None)
            }
        }
        "create_review" => {
            let mut body = a.clone();
            body.remove("repo");
            (Method::POST, r("reviews")?, q, Some(Value::Object(body)))
        }
        "review_action" => {
            let n = number()?;
            let action = str_of("action").ok_or("action is required")?;
            match action.as_str() {
                "approve" | "request_changes" => (
                    Method::POST,
                    r(&format!("reviews/{n}/{}", action.replace('_', "-")))?,
                    q,
                    Some(json!({ "body": str_of("body").unwrap_or_default() })),
                ),
                "land" => (
                    Method::POST,
                    r(&format!("reviews/{n}/land"))?,
                    q,
                    Some(json!({ "method": str_of("method").unwrap_or_default() })),
                ),
                "close" => (Method::PATCH, r(&format!("reviews/{n}"))?, q, Some(json!({ "state": "closed" }))),
                "reopen" => (Method::PATCH, r(&format!("reviews/{n}"))?, q, Some(json!({ "state": "open" }))),
                "ready" => (Method::PATCH, r(&format!("reviews/{n}"))?, q, Some(json!({ "draft": false }))),
                "update" => (
                    Method::PATCH,
                    r(&format!("reviews/{n}"))?,
                    q,
                    Some(json!({ "head": str_of("head").ok_or("head is required")? })),
                ),
                other => return Err(format!("unknown action {other}")),
            }
        }
        "comment" => {
            // Issues and reviews share numbers; the server routes by kind.
            let n = number()?;
            (
                Method::POST,
                format!("__comment/{}/{n}", repo()?),
                q,
                Some(json!({ "body": str_of("body"), "path": str_of("path"), "line": num_of("line") })),
            )
        }
        "list_issues" => {
            put("state", str_of("state"));
            put("q", str_of("q"));
            put("label", str_of("label"));
            put("assignee", str_of("assignee"));
            (Method::GET, r("issues")?, q, None)
        }
        "get_issue" => (Method::GET, r(&format!("issues/{}", number()?))?, q, None),
        "create_issue" => {
            let mut body = a.clone();
            body.remove("repo");
            (Method::POST, r("issues")?, q, Some(Value::Object(body)))
        }
        "update_issue" => {
            let n = number()?;
            let mut body = a.clone();
            body.remove("repo");
            body.remove("number");
            (Method::PATCH, r(&format!("issues/{n}"))?, q, Some(Value::Object(body)))
        }
        other => return Err(format!("unknown tool {other}")),
    })
}

async fn call_api(
    flux: &Arc<Flux>,
    who: &crate::auth::Principal,
    headers: &HeaderMap,
    name: &str,
    args: &Map<String, Value>,
) -> (bool, String) {
    let (method, mut path, query, body) = match plan(name, args) {
        Ok(p) => p,
        Err(e) => return (false, e),
    };
    // Comments: pick the issue or review endpoint by what the number is.
    if let Some(rest) = path.strip_prefix("__comment/") {
        let (repo, n) = rest.rsplit_once('/').unwrap_or((rest, "0"));
        let kind = match flux.repo(repo).await {
            Ok(id) => flux.meta.item_kind(id, n.parse().unwrap_or(0)).await.ok().flatten(),
            Err(_) => None,
        };
        path = match kind {
            Some("issue") => format!("repos/{repo}/issues/{n}/comments"),
            Some(_) => format!("repos/{repo}/reviews/{n}/comments"),
            None => return (false, format!("#{n} not found in {repo}")),
        };
    }
    let ctx = Ctx {
        flux: flux.clone(),
        who: who.clone(),
        method,
        headers: headers.clone(),
        query: query.into_iter().collect(),
        body: body.map(|b| Bytes::from(b.to_string())).unwrap_or_default(),
        origin: flux.settings.base_url(),
    };
    let segs: Vec<String> = path.split('/').filter(|x| !x.is_empty()).map(super::percent_encoding_decode).collect();
    let seg: Vec<&str> = segs.iter().map(String::as_str).collect();
    let resp = match super::route(&ctx, &seg).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    let ok = resp.status().is_success();
    let bytes = axum::body::to_bytes(resp.into_body(), 32 << 20).await.unwrap_or_default();
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if ok && text.is_empty() {
        text = "ok".into();
    }
    if !ok
        && let Ok(v) = serde_json::from_str::<Value>(&text)
        && let Some(m) = v.get("message").and_then(Value::as_str)
    {
        text = m.to_string();
    }
    (ok, text)
}

async fn handle_one(flux: &Arc<Flux>, who: &crate::auth::Principal, headers: &HeaderMap, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    // Notifications get no response.
    let id = id?;
    Some(match method {
        "initialize" => {
            let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL);
            rpc_ok(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "flux", "title": "flux source control", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "flux is a fast source control server. Start with repo_context. Read many files at once with read_files. Make changes with commit_changes (no clone needed) onto a branch like agent/<topic>, open a review, then use checks with wait_seconds to wait for CI instead of polling.",
                }),
            )
        }
        "ping" => rpc_ok(&id, json!({})),
        "tools/list" => rpc_ok(
            &id,
            json!({ "tools": TOOLS.iter().map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": (t.schema)() })).collect::<Vec<_>>() }),
        ),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").and_then(Value::as_object).cloned().unwrap_or_default();
            if !TOOLS.iter().any(|t| t.name == name) {
                return Some(rpc_err(&id, -32602, &format!("unknown tool {name}")));
            }
            let started = std::time::Instant::now();
            let (ok, text) = call_api(flux, who, headers, name, &args).await;
            tracing::debug!(tool = name, ok, ms = started.elapsed().as_millis() as u64, "mcp tool call");
            rpc_ok(&id, json!({ "content": [{ "type": "text", "text": text }], "isError": !ok }))
        }
        "resources/list" => rpc_ok(&id, json!({ "resources": [] })),
        "prompts/list" => rpc_ok(&id, json!({ "prompts": [] })),
        _ => rpc_err(&id, -32601, &format!("method not found: {method}")),
    })
}

pub async fn handle(State(flux): State<Arc<Flux>>, headers: HeaderMap, body: Bytes) -> Response {
    let who = match super::principal(&flux, &headers, &Method::POST).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, axum::Json(rpc_err(&Value::Null, -32700, &format!("parse error: {e}"))))
                .into_response();
        }
    };
    let out = match &msg {
        Value::Array(batch) => {
            let mut replies = Vec::new();
            for m in batch {
                if let Some(r) = handle_one(&flux, &who, &headers, m).await {
                    replies.push(r);
                }
            }
            if replies.is_empty() { None } else { Some(Value::Array(replies)) }
        }
        m => handle_one(&flux, &who, &headers, m).await,
    };
    match out {
        Some(v) => axum::Json(v).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// No server-initiated stream: clients only POST.
pub async fn handle_get() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_plans_with_minimal_arguments() {
        for t in TOOLS {
            let schema = (t.schema)();
            let mut args = Map::new();
            for r in schema["required"].as_array().unwrap() {
                let k = r.as_str().unwrap();
                let v = match schema["properties"][k]["type"].as_str() {
                    Some("integer") => json!(1),
                    Some("array") => json!([]),
                    _ => json!(if k == "action" { "approve" } else { "x" }),
                };
                args.insert(k.into(), v);
            }
            assert!(plan(t.name, &args).is_ok(), "{}", t.name);
        }
        assert!(plan("nope", &Map::new()).is_err());
    }
}
