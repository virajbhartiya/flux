//! Commands over the REST API: `flux api`, `flux mcp`, `flux issue`,
//! `flux ci`, `flux key` and `flux token`.

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

pub struct Api {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Api {
    pub fn new(server: &str, token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent(concat!("flux-cli/", env!("CARGO_PKG_VERSION"))).build()?;
        Ok(Self { base: server.trim_end_matches('/').to_string(), token, http })
    }

    fn req(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, url);
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    /// Call `/api/v1/<path>`; errors carry the server's message.
    pub async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{}/api/v1/{}", self.base, path.trim_start_matches('/'));
        let mut r = self.req(method, &url);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.with_context(|| format!("contacting {}", self.base))?;
        let status = resp.status();
        let text = resp.text().await?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        if !status.is_success() {
            let msg = v.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| v.to_string());
            bail!("{msg} ({status})");
        }
        Ok(v)
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> Result<Value> {
        self.call(reqwest::Method::PATCH, path, Some(body)).await
    }

    async fn text(&self, path: &str) -> Result<(String, reqwest::header::HeaderMap)> {
        let resp = self.req(reqwest::Method::GET, &format!("{}/api/v1/{path}", self.base)).send().await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("{text} ({status})");
        }
        Ok((text, headers))
    }
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn ago(ms: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let s = ((now - ms) / 1000).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

// ───────────── flux api ─────────────

#[derive(Args)]
pub struct ApiArgs {
    /// Path under /api/v1, e.g. `repos/acme/app/issues`.
    pub path: String,
    /// HTTP method (default: GET, or POST with --data).
    #[arg(short = 'X', long)]
    pub method: Option<String>,
    /// JSON body, or @file, or @- for standard input.
    #[arg(short, long)]
    pub data: Option<String>,
}

pub async fn api(api: &Api, a: &ApiArgs) -> Result<()> {
    let body = match a.data.as_deref() {
        None => None,
        Some("@-") => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            Some(serde_json::from_str(&s).context("the body is not JSON")?)
        }
        Some(d) if d.starts_with('@') => Some(serde_json::from_slice(&std::fs::read(&d[1..])?).context("the body is not JSON")?),
        Some(d) => Some(serde_json::from_str(d).context("the body is not JSON")?),
    };
    let method = match &a.method {
        Some(m) => m.to_ascii_uppercase().parse().context("unknown HTTP method")?,
        None if body.is_some() => reqwest::Method::POST,
        None => reqwest::Method::GET,
    };
    let v = api.call(method, &a.path, body).await?;
    match v {
        Value::String(s) => println!("{s}"),
        v => print_json(&v)?,
    }
    Ok(())
}

// ───────────── flux mcp ─────────────

/// Bridge an MCP client on stdio (Claude Code, Codex, ...) to the server's
/// MCP endpoint: one JSON-RPC message per line in, one per line out.
pub async fn mcp(api: &Api) -> Result<()> {
    let url = format!("{}/api/mcp", api.base);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp =
            api.req(reqwest::Method::POST, &url).header("content-type", "application/json").body(line.clone()).send().await;
        let reply = match resp {
            Ok(r) if r.status() == reqwest::StatusCode::ACCEPTED => continue,
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(e) => {
                // Answer requests (not notifications) with a JSON-RPC error.
                let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("id").cloned());
                match id {
                    Some(id) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": format!("flux server unreachable: {e}") } }).to_string(),
                    None => continue,
                }
            }
        };
        if reply.trim().is_empty() {
            continue;
        }
        writeln!(out, "{}", reply.trim())?;
        out.flush()?;
    }
    Ok(())
}

// ───────────── flux issue ─────────────

#[derive(Subcommand)]
pub enum IssueCmd {
    /// List issues.
    List {
        #[arg(long, default_value = "open")]
        state: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        /// Text search.
        #[arg(short, long)]
        query: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show an issue and its timeline.
    View {
        number: i64,
        #[arg(long)]
        json: bool,
    },
    /// Open an issue.
    Create {
        #[arg(short, long)]
        title: String,
        #[arg(short, long, default_value = "")]
        body: String,
        #[arg(short, long)]
        label: Vec<String>,
        #[arg(short, long)]
        assignee: Vec<String>,
    },
    /// Comment on an issue.
    Comment {
        number: i64,
        #[arg(short, long)]
        body: String,
    },
    /// Close an issue.
    Close {
        number: i64,
        /// completed, not_planned or duplicate.
        #[arg(long, default_value = "completed")]
        reason: String,
    },
    Reopen {
        number: i64,
    },
    /// Change title, labels or assignees.
    Edit {
        number: i64,
        #[arg(long)]
        title: Option<String>,
        /// Replace the labels (repeatable).
        #[arg(long)]
        label: Option<Vec<String>>,
        /// Replace the assignees (repeatable).
        #[arg(long)]
        assignee: Option<Vec<String>>,
    },
}

pub async fn issue(api: &Api, repo: &str, cmd: &IssueCmd) -> Result<()> {
    let base = format!("repos/{repo}/issues");
    match cmd {
        IssueCmd::List { state, label, assignee, query, json } => {
            let mut q = format!("{base}?state={}&per_page=100", enc(state));
            for (k, v) in [("label", label), ("assignee", assignee), ("q", query)] {
                if let Some(v) = v {
                    q.push_str(&format!("&{k}={}", enc(v)));
                }
            }
            let v = api.get(&q).await?;
            if *json {
                return print_json(&v);
            }
            let items = v.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("no {state} issues");
            }
            for i in items {
                let labels: Vec<&str> = i["labels"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                println!(
                    "#{:<5} {:<7} {}{}  {}",
                    i["number"],
                    i["state"].as_str().unwrap_or(""),
                    i["title"].as_str().unwrap_or(""),
                    if labels.is_empty() { String::new() } else { format!("  [{}]", labels.join(", ")) },
                    ago(i["updated_at"].as_i64().unwrap_or(0))
                );
            }
        }
        IssueCmd::View { number, json } => {
            let v = api.get(&format!("{base}/{number}")).await?;
            if *json {
                return print_json(&v);
            }
            println!("#{} {} ({})", v["number"], v["title"].as_str().unwrap_or(""), v["state"].as_str().unwrap_or(""));
            println!("opened by {} {}\n", v["author"].as_str().unwrap_or(""), ago(v["created_at"].as_i64().unwrap_or(0)));
            if let Some(b) = v["body"].as_str().filter(|b| !b.is_empty()) {
                println!("{b}\n");
            }
            for e in v["timeline"].as_array().into_iter().flatten() {
                let who = e["author"].as_str().unwrap_or("");
                match e["kind"].as_str().unwrap_or("") {
                    "comment" => println!(
                        "── {who} commented {}\n{}\n",
                        ago(e["created_at"].as_i64().unwrap_or(0)),
                        e["body"].as_str().unwrap_or("")
                    ),
                    k => println!("· {who} {k} {}", e["body"].as_str().unwrap_or("")),
                }
            }
        }
        IssueCmd::Create { title, body, label, assignee } => {
            let v = api.post(&base, json!({ "title": title, "body": body, "labels": label, "assignees": assignee })).await?;
            println!("opened issue #{} {}", v["number"], v["title"].as_str().unwrap_or(""));
        }
        IssueCmd::Comment { number, body } => {
            api.post(&format!("{base}/{number}/comments"), json!({ "body": body })).await?;
            println!("commented on #{number}");
        }
        IssueCmd::Close { number, reason } => {
            api.patch(&format!("{base}/{number}"), json!({ "state": "closed", "state_reason": reason })).await?;
            println!("closed #{number}");
        }
        IssueCmd::Reopen { number } => {
            api.patch(&format!("{base}/{number}"), json!({ "state": "open" })).await?;
            println!("reopened #{number}");
        }
        IssueCmd::Edit { number, title, label, assignee } => {
            let mut body = serde_json::Map::new();
            if let Some(t) = title {
                body.insert("title".into(), json!(t));
            }
            if let Some(l) = label {
                body.insert("labels".into(), json!(l));
            }
            if let Some(a) = assignee {
                body.insert("assignees".into(), json!(a));
            }
            api.patch(&format!("{base}/{number}"), Value::Object(body)).await?;
            println!("updated #{number}");
        }
    }
    Ok(())
}

// ───────────── flux ci ─────────────

#[derive(Subcommand)]
pub enum CiCmd {
    /// Recent runs (optionally for one revision).
    Runs {
        #[arg(long)]
        rev: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// A run with its jobs.
    Show {
        run: i64,
    },
    /// A job's log; --follow streams it until the job ends.
    Logs {
        job: i64,
        #[arg(short, long)]
        follow: bool,
    },
    /// Wait for a revision's checks to settle; exits non-zero on failure.
    Wait {
        rev: String,
        /// Seconds.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Start a workflow that has a workflow_dispatch trigger.
    Run {
        workflow: String,
        #[arg(long = "ref")]
        rev: Option<String>,
    },
    Cancel {
        run: i64,
    },
    Rerun {
        run: i64,
    },
}

pub async fn ci(api: &Api, repo: &str, cmd: &CiCmd) -> Result<()> {
    let base = format!("repos/{repo}");
    match cmd {
        CiCmd::Runs { rev, json } => {
            let q = match rev {
                Some(r) => format!("{base}/runs?rev={}", enc(r)),
                None => format!("{base}/runs"),
            };
            let v = api.get(&q).await?;
            if *json {
                return print_json(&v);
            }
            for r in v.as_array().into_iter().flatten() {
                println!(
                    "{:<6} {:<10} {:<20} {:<18} {} {}",
                    r["id"],
                    r["state"].as_str().unwrap_or(""),
                    r["workflow"].as_str().unwrap_or(""),
                    format!("{} {}", r["event"].as_str().unwrap_or(""), r["ref"].as_str().unwrap_or("")),
                    &r["changeset"].as_str().unwrap_or("")[..12.min(r["changeset"].as_str().unwrap_or("").len())],
                    ago(r["created_at"].as_i64().unwrap_or(0))
                );
            }
        }
        CiCmd::Show { run } => {
            let v = api.get(&format!("{base}/runs/{run}")).await?;
            println!(
                "run {} {} ({}) on {}",
                v["id"],
                v["workflow"].as_str().unwrap_or(""),
                v["state"].as_str().unwrap_or(""),
                v["ref"].as_str().unwrap_or("")
            );
            for j in v["jobs"].as_array().into_iter().flatten() {
                println!("  job {:<6} {:<10} {}", j["id"], j["state"].as_str().unwrap_or(""), j["name"].as_str().unwrap_or(""));
            }
        }
        CiCmd::Logs { job, follow } => {
            let mut after = 0i64;
            let mut out = std::io::stdout();
            loop {
                let (text, headers) = api.text(&format!("{base}/jobs/{job}/log?after={after}")).await?;
                out.write_all(text.as_bytes())?;
                out.flush()?;
                after = headers.get("x-flux-log-seq").and_then(|v| v.to_str().ok()?.parse().ok()).unwrap_or(after);
                let state = headers.get("x-flux-job-state").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                if !*follow || matches!(state.as_str(), "success" | "failure" | "cancelled" | "skipped") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            }
        }
        CiCmd::Wait { rev, timeout } => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(*timeout);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now()).as_secs().min(300);
                let v = api.get(&format!("{base}/commits/{}/status?wait={left}", enc(rev))).await?;
                let state = v["state"].as_str().unwrap_or("pending").to_string();
                let settled = v["total_count"].as_u64().unwrap_or(0) > 0
                    && v["statuses"].as_array().into_iter().flatten().all(|s| s["state"] != "pending");
                if settled || left == 0 {
                    for s in v["statuses"].as_array().into_iter().flatten() {
                        println!("{:<8} {}", s["state"].as_str().unwrap_or(""), s["context"].as_str().unwrap_or(""));
                    }
                    match state.as_str() {
                        "success" => return Ok(()),
                        "pending" => bail!("timed out waiting for checks"),
                        _ => bail!("checks failed"),
                    }
                }
            }
        }
        CiCmd::Run { workflow, rev } => {
            let v = api.post(&format!("{base}/runs"), json!({ "workflow": workflow, "ref": rev })).await?;
            println!("started run(s) {}", v["runs"]);
        }
        CiCmd::Cancel { run } => {
            api.post(&format!("{base}/runs/{run}/cancel"), json!({})).await?;
            println!("cancelled run {run}");
        }
        CiCmd::Rerun { run } => {
            let v = api.post(&format!("{base}/runs/{run}/rerun"), json!({})).await?;
            println!("started run(s) {}", v["runs"]);
        }
    }
    Ok(())
}

// ───────────── flux key / flux token ─────────────

#[derive(Subcommand)]
pub enum KeyCmd {
    /// Add an SSH public key (a .pub file, or - for standard input; default
    /// ~/.ssh/id_ed25519.pub).
    Add {
        file: Option<String>,
        #[arg(short, long, default_value = "")]
        title: String,
    },
    List,
    Remove {
        id: i64,
    },
}

pub async fn key(api: &Api, cmd: &KeyCmd) -> Result<()> {
    match cmd {
        KeyCmd::Add { file, title } => {
            let key = match file.as_deref() {
                Some("-") => {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                    s
                }
                Some(f) => std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?,
                None => {
                    let home = dirs::home_dir().context("no home directory")?;
                    let path = ["id_ed25519.pub", "id_ecdsa.pub", "id_rsa.pub"]
                        .iter()
                        .map(|n| home.join(".ssh").join(n))
                        .find(|p| p.exists());
                    std::fs::read_to_string(path.context("no SSH public key found in ~/.ssh; pass a file")?)?
                }
            };
            let v = api.post("user/keys", json!({ "title": title, "key": key.trim() })).await?;
            println!("added key {} {}", v["fingerprint"].as_str().unwrap_or(""), v["title"].as_str().unwrap_or(""));
        }
        KeyCmd::List => {
            for k in api.get("user/keys").await?.as_array().into_iter().flatten() {
                println!("{:<5} {} {}", k["id"], k["fingerprint"].as_str().unwrap_or(""), k["title"].as_str().unwrap_or(""));
            }
        }
        KeyCmd::Remove { id } => {
            api.call(reqwest::Method::DELETE, &format!("user/keys/{id}"), None).await?;
            println!("removed key {id}");
        }
    }
    Ok(())
}

#[derive(Subcommand)]
pub enum TokenCmd {
    /// Mint a token, optionally bound to repositories and ref patterns —
    /// e.g. for an agent: `--scope write --for-repo acme/app --ref 'agent/*'`.
    Create {
        #[arg(long, default_value = "token")]
        label: String,
        /// read, write or all.
        #[arg(long, default_value = "read")]
        scope: String,
        #[arg(long = "for-repo")]
        repos: Vec<String>,
        #[arg(long = "ref")]
        refs: Vec<String>,
        #[arg(long)]
        expires_days: Option<i64>,
    },
}

pub async fn token(api: &Api, cmd: &TokenCmd) -> Result<()> {
    match cmd {
        TokenCmd::Create { label, scope, repos, refs, expires_days } => {
            let v = api
                .post(
                    "user/tokens",
                    json!({ "label": label, "scope": scope, "repos": repos, "refs": refs, "expires_in_days": expires_days }),
                )
                .await?;
            eprintln!("token with scopes \"{}\" (shown once):", v["scopes"].as_str().unwrap_or(""));
            println!("{}", v["token"].as_str().unwrap_or(""));
        }
    }
    Ok(())
}
