//! `flux`: the command-line client (SPEC §13.1).

mod migrate;
#[cfg(target_os = "linux")]
mod vfs;
mod wc;

use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use flux_client::{Client, CsInfo};
use flux_core::{Changeset, ChangesetId, ContentId, FileChange, FileKind, RepoPath, Timestamp};
use flux_proto::v1::update_ref_request::Expect;

use crate::wc::{Change, Config, Entry, Kind, WorkingCopy, mtime_ns, repo_path_arg};

#[derive(Parser)]
#[command(name = "flux", version, about = "flux: fast source control for huge monorepos", max_term_width = 100)]
struct Cli {
    /// Server URL (defaults to the working copy's server).
    #[arg(long, global = true, env = "FLUX_SERVER")]
    server: Option<String>,
    /// Repository (defaults to the working copy's repository).
    #[arg(long, global = true, env = "FLUX_REPO")]
    repo: Option<String>,
    /// Print timing for the command.
    #[arg(long, global = true)]
    time: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a working copy attached to a server repository (alias: clone).
    #[command(alias = "clone")]
    Attach {
        server: String,
        repo: String,
        dir: Option<PathBuf>,
        /// Only materialize these directory prefixes (repeatable).
        #[arg(long)]
        sparse: Vec<String>,
        /// Revision to check out (default: the default branch).
        #[arg(long)]
        rev: Option<String>,
    },
    /// Update the working copy to a revision (alias: switch).
    #[command(alias = "switch")]
    Checkout {
        rev: String,
        /// Discard local modifications.
        #[arg(long)]
        force: bool,
    },
    /// Fetch the latest version of the followed branch.
    #[command(alias = "pull")]
    Sync,
    /// Show working-copy changes.
    #[command(alias = "st")]
    Status,
    /// Show working-copy changes as a diff, or a changeset's diff with --rev.
    Diff {
        /// Names only.
        #[arg(long)]
        stat: bool,
        /// Diff this changeset against its first parent instead.
        #[arg(long, short)]
        rev: Option<String>,
        paths: Vec<String>,
    },
    /// Create a changeset from working-copy changes.
    #[command(alias = "ci")]
    Commit {
        #[arg(short, long)]
        message: String,
        /// Also commit untracked files.
        #[arg(short = 'A', long)]
        all: bool,
        /// Only these paths.
        paths: Vec<String>,
    },
    /// Discard working-copy changes to paths (restores the parent's version).
    Revert { paths: Vec<String> },
    /// Show history (optionally of one file or directory).
    Log {
        /// Only changesets that modified this path.
        path: Option<String>,
        #[arg(short, long, default_value = "@")]
        rev: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        first_parent: bool,
        /// Hide changesets reachable from this revision (log BASE..REV).
        #[arg(long)]
        since: Option<String>,
    },
    /// Show a changeset and its diff.
    Show {
        #[arg(default_value = "@")]
        rev: String,
        /// File names only.
        #[arg(long)]
        stat: bool,
    },
    /// Print a file at a revision (no checkout needed).
    Cat { rev: String, path: String },
    /// List a directory at a revision (no checkout needed).
    Ls {
        #[arg(default_value = "@")]
        rev: String,
        #[arg(default_value = "")]
        path: String,
    },
    /// Show who last changed each line of a file.
    Blame {
        path: String,
        #[arg(long, default_value = "@")]
        rev: String,
    },
    /// Search file contents at a revision, on the server (no checkout needed).
    Grep {
        pattern: String,
        /// Treat the pattern as a regular expression.
        #[arg(short = 'E', long)]
        regex: bool,
        #[arg(short, long)]
        ignore_case: bool,
        #[arg(long, default_value = "@")]
        rev: String,
        /// Only under this path.
        path: Option<String>,
        #[arg(short = 'n', long, default_value_t = 200)]
        limit: u32,
    },
    /// Fuzzy-find file paths at a revision.
    Find {
        query: String,
        #[arg(long, default_value = "@")]
        rev: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
    },
    /// List refs (branches).
    #[command(alias = "branches")]
    Refs {
        #[arg(default_value = "")]
        prefix: String,
    },
    /// Create a branch at a revision (default: the working copy's parent)
    /// and follow it.
    Branch {
        name: String,
        #[arg(default_value = "@")]
        rev: String,
        /// Delete the branch instead.
        #[arg(short, long)]
        delete: bool,
    },
    /// Point a ref at a revision (fast-forward check unless --force).
    SetRef {
        name: String,
        rev: String,
        #[arg(long)]
        force: bool,
    },
    /// Land a stack onto a branch: fast-forward or server-side rebase with
    /// three-way merges, then compare-and-swap.
    Land {
        #[arg(default_value = "@")]
        rev: String,
        #[arg(long, default_value = "main")]
        onto: String,
    },
    /// Code review.
    Review {
        #[command(subcommand)]
        cmd: ReviewCmd,
    },
    /// Show or change the sparse profile.
    Sparse {
        #[command(subcommand)]
        cmd: SparseCmd,
    },
    /// Repository administration.
    Repo {
        #[command(subcommand)]
        cmd: RepoCmd,
    },
    /// Store a token for a server (read from --token or stdin).
    /// Sign in with your username and password (or paste a token with
    /// --token). Stores an API token for this server.
    Login {
        server: Option<String>,
        /// Use this access token instead of a password.
        #[arg(long)]
        token: Option<String>,
        /// Username or email (prompted if omitted).
        #[arg(long, short)]
        user: Option<String>,
        /// Read the password from standard input (for scripts).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Forget the stored token for a server.
    Logout { server: Option<String> },
    /// Show who the server thinks you are.
    Whoami,
    /// Stream live repository events.
    Watch {
        /// All repositories on the server.
        #[arg(long)]
        all: bool,
    },
    /// Open the web UI for the current repository, revision and path.
    Web { path: Option<String> },
    /// Mount a revision read-only (Linux, FUSE): directories and files load
    /// on first access. Runs until Ctrl-C.
    Mount {
        dir: PathBuf,
        /// Revision to mount (default: the working copy's parent, or main).
        #[arg(short, long)]
        rev: Option<String>,
    },
    /// Manage fluxd, the local cache daemon (commands route through it
    /// automatically while it runs).
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Migrate repositories from another forge.
    Migrate {
        #[command(subcommand)]
        cmd: migrate::MigrateCmd,
    },
    /// Keep a flux repository in sync with a GitHub repository (git and
    /// metadata, incremental) until stopped: for a gradual cutover.
    Mirror(migrate::MirrorArgs),
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start a daemon for the server (no-op if one is running).
    Start,
    /// Stop the server's daemon.
    Stop,
    /// Show the daemon's address and cache statistics.
    Status,
    /// Start daemons automatically when a command needs one.
    Enable,
    /// Stop starting daemons automatically.
    Disable,
}

#[derive(Subcommand)]
enum ReviewCmd {
    /// Open a review for a stack (default head: the working copy's parent).
    Create {
        #[arg(short, long)]
        title: Option<String>,
        #[arg(short, long, default_value = "")]
        message: String,
        #[arg(long, default_value = "@")]
        head: String,
        #[arg(long, default_value = "main")]
        target: String,
    },
    /// List reviews.
    List {
        #[arg(long, default_value = "open")]
        state: String,
    },
    /// Show a review with its timeline and stack.
    Show {
        number: i64,
    },
    /// Comment on a review.
    Comment {
        number: i64,
        #[arg(short, long)]
        message: String,
    },
    /// Approve a review.
    Approve {
        number: i64,
        #[arg(short, long, default_value = "")]
        message: String,
    },
    /// Point a review at a new head (default: the working copy's parent).
    Update {
        number: i64,
        #[arg(long, default_value = "@")]
        head: String,
    },
    /// Land a review.
    Land {
        number: i64,
    },
    Close {
        number: i64,
    },
    Reopen {
        number: i64,
    },
}

#[derive(Subcommand)]
enum SparseCmd {
    Show,
    /// Replace the profile (no prefixes = full checkout) and reconcile.
    Set {
        prefixes: Vec<String>,
    },
}

#[derive(Subcommand)]
enum RepoCmd {
    /// Create `owner/name` (yours or an organization you own), or a
    /// server-wide `name` (site admins).
    Create {
        name: String,
        /// public, internal or private (owned repositories default to private).
        #[arg(long)]
        visibility: Option<String>,
    },
    /// Repositories you can read.
    List,
}

// ───────────── terminal styling ─────────────

struct Style {
    on: bool,
}

impl Style {
    fn new() -> Self {
        Self { on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() }
    }
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let started = Instant::now();
    let timed = cli.time;
    let r = run(cli).await;
    if timed {
        eprintln!("({:.1} ms)", started.elapsed().as_secs_f64() * 1000.0);
    }
    if let Err(e) = r {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

// ───────────── credentials ─────────────

fn normalize_server(s: &str) -> String {
    let s = s.trim().trim_end_matches('/');
    if s.contains("://") { s.to_string() } else { format!("http://{s}") }
}

fn credentials_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("flux").join("credentials.json"))
}

fn load_credentials() -> HashMap<String, String> {
    credentials_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_credentials(c: &HashMap<String, String>) -> Result<()> {
    let p = credentials_path().context("no config directory")?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(&serde_json::to_vec_pretty(c)?)?;
    }
    std::fs::rename(tmp, p)?;
    Ok(())
}

/// `$FLUX_TOKEN`, or the stored token for a (normalized) server URL.
fn token_for(server: &str) -> Option<String> {
    std::env::var("FLUX_TOKEN").ok().or_else(|| load_credentials().get(server).cloned())
}

async fn connect(server: &str, repo: &str) -> Result<Client> {
    let server = normalize_server(server);
    let token = token_for(&server);
    // Route through a running fluxd for this server (objects from local
    // disk); start one in the background if the user asked for that.
    if std::env::var_os("FLUX_NO_DAEMON").is_none() {
        match flux_client::daemon::live_url(&server) {
            Some(url) => {
                if let Ok(c) = Client::connect_with_token(&url, repo, token.as_deref()).await {
                    return Ok(c);
                }
            }
            None if flux_client::daemon::auto_start_enabled() => {
                let _ = spawn_daemon(&server);
            }
            None => {}
        }
    }
    Client::connect_with_token(&server, repo, token.as_deref()).await
}

/// Start `fluxd` for `server` in the background (detached from the terminal).
fn spawn_daemon(server: &str) -> Result<()> {
    let exe = if cfg!(windows) { "fluxd.exe" } else { "fluxd" };
    let fluxd = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(exe));
    let dir = flux_client::daemon::dir_for(server).context("no cache directory")?;
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("fluxd.log"))?;
    let mut cmd = std::process::Command::new(&fluxd);
    cmd.arg("--upstream").arg(server).stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x8;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().with_context(|| format!("starting {}", fluxd.display()))?;
    Ok(())
}

/// A minimal HTTP/1.1 request to a loopback daemon: `(status, body)`.
fn daemon_http(url: &str, method: &str, path: &str, headers: &[(&str, &str)]) -> Result<(u16, String)> {
    use std::io::Read as _;
    let addr: std::net::SocketAddr = url.strip_prefix("http://").context("bad daemon url")?.parse()?;
    let mut sock = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2))?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: 0\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes())?;
    let mut resp = String::new();
    sock.read_to_string(&mut resp)?;
    let status = resp.split(' ').nth(1).and_then(|c| c.parse().ok()).context("bad response from daemon")?;
    let body = resp.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default();
    Ok((status, body))
}

async fn client_for(cli: &Cli, wc: Option<&WorkingCopy>) -> Result<Client> {
    let server = cli.server.clone().or_else(|| wc.map(|w| w.config.server.clone()));
    let repo = cli.repo.clone().or_else(|| wc.map(|w| w.config.repo.clone()));
    match (server, repo) {
        (Some(s), Some(r)) => connect(&s, &r).await,
        _ => bail!("no working copy here; pass --server and --repo"),
    }
}

/// Resolve `@` to the working-copy parent.
fn expand_rev(rev: &str, wc: Option<&WorkingCopy>) -> Result<String> {
    if let Some(rest) = rev.strip_prefix('@') {
        let parent = wc.and_then(|w| w.state.parent).context("`@` needs a working copy with a checked-out changeset")?;
        return Ok(format!("{}{rest}", parent.to_hex()));
    }
    Ok(rev.to_string())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn ago(ms: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let s = (now - ms).max(0) / 1000;
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        86_400..2_592_000 => format!("{}d ago", s / 86_400),
        2_592_000..31_536_000 => format!("{}mo ago", s / 2_592_000),
        _ => format!("{}y ago", s / 31_536_000),
    }
}

fn subject(msg: &[u8]) -> String {
    String::from_utf8_lossy(msg).lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

fn author_name(a: &[u8]) -> String {
    let s = String::from_utf8_lossy(a);
    s.split('<').next().unwrap_or(&s).trim().to_string()
}

async fn run(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let wc = WorkingCopy::find(&cwd).ok();
    let st = Style::new();
    match &cli.cmd {
        Cmd::Attach { server, repo, dir, sparse, rev } => {
            let server = normalize_server(server);
            let dir = dir.clone().unwrap_or_else(|| PathBuf::from(repo.rsplit('/').next().unwrap_or(repo)));
            std::fs::create_dir_all(&dir)?;
            let client = connect(&server, repo).await?;
            let mut wc = WorkingCopy::init(
                &dir,
                Config {
                    server: server.clone(),
                    repo: repo.clone(),
                    sparse: sparse.iter().map(|s| s.trim_matches('/').to_string()).collect(),
                },
            )?;
            let rev = match rev {
                Some(r) => Some(r.clone()),
                None => {
                    let refs = client.list_refs(b"").await?;
                    ["main", "master", "trunk"]
                        .iter()
                        .find(|b| refs.iter().any(|r| r.name == b.as_bytes()))
                        .map(|b| b.to_string())
                        .or_else(|| refs.first().map(|r| String::from_utf8_lossy(&r.name).into_owned()))
                }
            };
            match rev {
                Some(r) => checkout(&client, &mut wc, &r, false).await?,
                None => println!("attached to empty repository {repo}"),
            }
            println!("working copy at {}", wc.root.display());
        }
        Cmd::Checkout { rev, force } => {
            let mut wc = wc.context("not in a working copy")?;
            let client = client_for(&cli, Some(&wc)).await?;
            let rev = expand_rev(rev, Some(&wc))?;
            checkout(&client, &mut wc, &rev, *force).await?;
        }
        Cmd::Sync => {
            let mut wc = wc.context("not in a working copy")?;
            let client = client_for(&cli, Some(&wc)).await?;
            let bm = wc.state.bookmark.clone().context("the working copy doesn't follow a branch; use `flux checkout BRANCH`")?;
            let before = wc.state.parent;
            checkout(&client, &mut wc, &bm, false).await?;
            if before == wc.state.parent {
                println!("already up to date");
            }
        }
        Cmd::Status => {
            let wc = wc.context("not in a working copy")?;
            let entries = wc.status()?;
            if let Some(p) = wc.state.parent {
                let follow = wc.state.bookmark.as_ref().map(|b| format!(" on {}", st.cyan(b))).unwrap_or_default();
                println!("parent {}{follow}", st.yellow(&p.short()));
            }
            if entries.is_empty() {
                println!("{}", st.dim("clean"));
            }
            for e in entries {
                let line = match e.change {
                    Change::Modified => st.yellow(&format!("M {}", e.path)),
                    Change::Deleted => st.red(&format!("D {}", e.path)),
                    Change::Untracked => st.dim(&format!("? {}", e.path)),
                };
                println!("{line}");
            }
        }
        Cmd::Diff { stat, rev, paths } => match rev {
            Some(r) => {
                let client = client_for(&cli, wc.as_ref()).await?;
                let r = expand_rev(r, wc.as_ref())?;
                let cs = client.resolve(&r).await?;
                print_changeset_diff(&client, &cs, *stat, &st).await?;
            }
            None => {
                let wc = wc.context("not in a working copy")?;
                let client = client_for(&cli, Some(&wc)).await?;
                let filters: Vec<RepoPath> = paths.iter().filter_map(|p| repo_path_arg(&wc, p).ok().flatten()).collect();
                diff_wc(&client, &wc, *stat, &filters, &st).await?;
            }
        },
        Cmd::Commit { message, all, paths } => {
            let mut wc = wc.context("not in a working copy")?;
            let client = client_for(&cli, Some(&wc)).await?;
            let filters: Vec<RepoPath> = paths.iter().filter_map(|p| repo_path_arg(&wc, p).ok().flatten()).collect();
            commit(&client, &mut wc, message, *all, &filters).await?;
        }
        Cmd::Revert { paths } => {
            let mut wc = wc.context("not in a working copy")?;
            let client = client_for(&cli, Some(&wc)).await?;
            if paths.is_empty() {
                bail!("name the paths to revert (use `.` for everything)");
            }
            let filters: Vec<RepoPath> = paths.iter().filter_map(|p| repo_path_arg(&wc, p).ok().flatten()).collect();
            let everything = paths.iter().any(|p| repo_path_arg(&wc, p).ok().flatten().is_none());
            revert(&client, &mut wc, if everything { &[] } else { &filters }).await?;
        }
        Cmd::Log { rev, limit, first_parent, path, since } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let path = match (path, wc.as_ref()) {
                (Some(p), Some(w)) => repo_path_arg(w, p)?.map(|p| p.into_bytes()),
                (Some(p), None) => Some(p.trim_matches('/').as_bytes().to_vec()),
                _ => None,
            };
            let hide: Vec<String> = since.iter().map(|s| expand_rev(s, wc.as_ref())).collect::<Result<_>>()?;
            for cs in client.log_range(&rev, &hide, 0, *limit, *first_parent, path.as_deref()).await? {
                print_oneline(&cs, &st);
            }
        }
        Cmd::Show { rev, stat } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let cs = client.resolve(&rev).await?;
            print_header(&cs, &st);
            print_changeset_diff(&client, &cs, *stat, &st).await?;
        }
        Cmd::Cat { rev, path } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let path = match wc.as_ref() {
                Some(w) => repo_path_arg(w, path)?.map(|p| p.into_bytes()).unwrap_or_default(),
                None => path.trim_matches('/').as_bytes().to_vec(),
            };
            std::io::stdout().write_all(&client.read_file(&rev, &path).await?)?;
        }
        Cmd::Ls { rev, path } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let dir = client.list_dir(&rev, path.trim_matches('/').as_bytes()).await?;
            for e in dir.entries {
                let name = String::from_utf8_lossy(&e.name);
                if e.is_dir {
                    println!("{:>10}  {:>8} files  {}", human(e.size), e.descendant_files, st.cyan(&format!("{name}/")));
                } else {
                    println!("{:>10}  {:>14}  {name}", human(e.size), "");
                }
            }
        }
        Cmd::Blame { path, rev } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let p = match wc.as_ref() {
                Some(w) => repo_path_arg(w, path)?.context("blame needs a file")?.into_bytes(),
                None => path.trim_matches('/').as_bytes().to_vec(),
            };
            let (ranges, infos) = client.blame(&rev, &p).await?;
            let content = client.read_file(&rev, &p).await?;
            let by_id: HashMap<ChangesetId, &CsInfo> = infos.iter().map(|i| (i.id, i)).collect();
            let text = String::from_utf8_lossy(&content);
            let lines: Vec<&str> = text.lines().collect();
            let width = lines.len().to_string().len();
            for (start, count, id) in ranges {
                let info = by_id.get(&id);
                for k in 0..count {
                    let i = (start + k) as usize;
                    let Some(line) = lines.get(i) else { break };
                    let who = info.map(|c| author_name(&c.changeset.author)).unwrap_or_default();
                    let when = info.map(|c| ago(c.changeset.author_time.unix_ms)).unwrap_or_default();
                    let meta = if k == 0 {
                        format!("{} {:<16.16} {:>8}", st.yellow(&id.to_hex()[..10]), who, when)
                    } else {
                        format!("{} {:<16} {:>8}", st.dim(&"·".repeat(10)), "", "")
                    };
                    println!("{meta} {} {line}", st.dim(&format!("{:>width$}", i + 1)));
                }
            }
        }
        Cmd::Grep { pattern, regex, ignore_case, rev, path, limit } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let prefix = match (path, wc.as_ref()) {
                (Some(p), Some(w)) => repo_path_arg(w, p)?.map(|p| p.into_bytes()).unwrap_or_default(),
                (Some(p), None) => p.trim_matches('/').as_bytes().to_vec(),
                _ => vec![],
            };
            let hits = client
                .search(flux_proto::v1::SearchRequest {
                    repo: String::new(),
                    rev,
                    pattern: pattern.clone(),
                    regex: *regex,
                    case_insensitive: *ignore_case,
                    path_prefix: prefix,
                    limit: *limit,
                })
                .await?;
            for h in &hits {
                println!(
                    "{}{}{}{}{}",
                    st.cyan(&String::from_utf8_lossy(&h.path)),
                    st.dim(":"),
                    st.green(&h.line_number.to_string()),
                    st.dim(":"),
                    String::from_utf8_lossy(&h.line)
                );
            }
            if hits.is_empty() {
                std::process::exit(1);
            }
        }
        Cmd::Find { query, rev, limit } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            for p in client.find_files(&rev, query, *limit).await? {
                println!("{}", String::from_utf8_lossy(&p));
            }
        }
        Cmd::Refs { prefix } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let current = wc.as_ref().and_then(|w| w.state.bookmark.clone());
            for r in client.list_refs(prefix.as_bytes()).await? {
                let id = ChangesetId::from_slice(&r.target)?;
                let name = String::from_utf8_lossy(&r.name).into_owned();
                let mark = if current.as_deref() == Some(name.as_str()) { "*" } else { " " };
                println!(
                    "{mark} {}  {}",
                    st.yellow(&id.short()),
                    if mark == "*" { st.bold(&st.cyan(&name)) } else { st.cyan(&name) }
                );
            }
        }
        Cmd::Branch { name, rev, delete } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            if *delete {
                let cur = client
                    .list_refs(name.as_bytes())
                    .await?
                    .into_iter()
                    .find(|r| r.name == name.as_bytes())
                    .context("no such branch")?;
                if !client.update_ref(name, Expect::Version(cur.version), None, "flux branch -d").await? {
                    bail!("{name} moved concurrently; retry");
                }
                println!("deleted {name}");
                return Ok(());
            }
            let rev = expand_rev(rev, wc.as_ref())?;
            let target = client.resolve(&rev).await?;
            if !client.update_ref(name, Expect::Absent(true), Some(target.id), "flux branch").await? {
                bail!("{name} already exists");
            }
            if let Some(mut w) = wc
                && w.state.parent == Some(target.id)
            {
                w.state.bookmark = Some(name.clone());
                w.save_state()?;
                println!("created {} at {} (now following it)", st.cyan(name), st.yellow(&target.id.short()));
            } else {
                println!("created {} at {}", st.cyan(name), st.yellow(&target.id.short()));
            }
        }
        Cmd::SetRef { name, rev, force } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let target = client.resolve(&rev).await?;
            let expect = if *force {
                Expect::Any(true)
            } else {
                match client.list_refs(name.as_bytes()).await?.into_iter().find(|r| r.name == name.as_bytes()) {
                    None => Expect::Absent(true),
                    Some(cur) => {
                        let cur_id = ChangesetId::from_slice(&cur.target)?;
                        let a = client.ancestry(&cur_id.to_hex(), &target.id.to_hex()).await?;
                        if !a.a_is_ancestor_of_b {
                            bail!(
                                "{name} ({}) is not an ancestor of {}; use --force or `flux land`",
                                cur_id.short(),
                                target.id.short()
                            );
                        }
                        Expect::Version(cur.version)
                    }
                }
            };
            if !client.update_ref(name, expect, Some(target.id), "flux set-ref").await? {
                bail!("{name} moved concurrently; retry");
            }
            println!("{name} -> {}", target.id.short());
        }
        Cmd::Land { rev, onto } => {
            let mut wc = wc;
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let started = Instant::now();
            let r = client.land(&rev, onto, "flux land").await?;
            let head = ChangesetId::from_slice(&r.new_head)?;
            if r.fast_forward {
                println!(
                    "landed on {} by fast-forward: {} ({:.0} ms)",
                    st.cyan(onto),
                    st.yellow(&head.short()),
                    started.elapsed().as_secs_f64() * 1000.0
                );
            } else {
                println!(
                    "landed {} changeset(s) on {} by server-side rebase{}: {} ({:.0} ms)",
                    r.rebased.len(),
                    st.cyan(onto),
                    if r.merged_files > 0 { format!(", {} file(s) merged", r.merged_files) } else { String::new() },
                    st.yellow(&head.short()),
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
            // Move the working copy onto the landed result.
            if let Some(w) = wc.as_mut() {
                let clean = w.status()?.iter().all(|e| e.change == Change::Untracked);
                if clean {
                    checkout(&client, w, onto, false).await?;
                }
            }
        }
        Cmd::Review { cmd } => review(&cli, wc, cmd, &st).await?,
        Cmd::Sparse { cmd } => {
            let mut wc = wc.context("not in a working copy")?;
            match cmd {
                SparseCmd::Show => {
                    if wc.config.sparse.is_empty() {
                        println!("(full checkout)");
                    }
                    for p in &wc.config.sparse {
                        println!("{p}");
                    }
                }
                SparseCmd::Set { prefixes } => {
                    let client = client_for(&cli, Some(&wc)).await?;
                    if !wc.status()?.iter().all(|e| e.change == Change::Untracked) {
                        bail!("working copy has changes; commit them first");
                    }
                    wc.config.sparse = prefixes.iter().map(|s| s.trim_matches('/').to_string()).collect();
                    wc.save_config()?;
                    if let Some(p) = wc.state.parent {
                        let bm = wc.state.bookmark.clone();
                        checkout(&client, &mut wc, &p.to_hex(), true).await?;
                        wc.state.bookmark = bm;
                        wc.save_state()?;
                    }
                }
            }
        }
        Cmd::Repo { cmd } => {
            let server =
                cli.server.clone().or_else(|| wc.as_ref().map(|w| w.config.server.clone())).context("--server required")?;
            let client = connect(&server, "").await?;
            match cmd {
                RepoCmd::Create { name, visibility } => {
                    client.create_repo_with(name, visibility.as_deref().unwrap_or("")).await?;
                    println!("created {name}");
                }
                RepoCmd::List => {
                    for r in client.list_repos().await? {
                        println!("{r}");
                    }
                }
            }
        }
        Cmd::Login { server, token, user, password_stdin } => {
            let server = normalize_server(
                &server
                    .clone()
                    .or_else(|| cli.server.clone())
                    .or_else(|| wc.as_ref().map(|w| w.config.server.clone()))
                    .context("which server?")?,
            );
            let token = match token {
                Some(t) => t.clone(),
                None => {
                    let login = match user {
                        Some(u) => u.clone(),
                        None => {
                            eprint!("username or email for {server}: ");
                            let mut u = String::new();
                            std::io::stdin().read_line(&mut u)?;
                            u.trim().to_string()
                        }
                    };
                    let password = if *password_stdin {
                        let mut p = String::new();
                        std::io::stdin().read_line(&mut p)?;
                        p.trim_end_matches(['\r', '\n']).to_string()
                    } else {
                        rpassword::prompt_password("password: ")?
                    };
                    let host = std::env::var("HOSTNAME")
                        .ok()
                        .or_else(|| std::env::var("COMPUTERNAME").ok())
                        .unwrap_or_else(|| "this machine".into());
                    let (token, _) = Client::connect_with_token(&server, "", Some(""))
                        .await?
                        .login(&login, &password, &format!("flux CLI on {host}"))
                        .await?;
                    token
                }
            };
            let who = Client::connect_with_token(&server, "", Some(&token)).await?.whoami().await?;
            if who.user.is_empty() {
                bail!("the server did not accept that token");
            }
            let mut creds = load_credentials();
            creds.insert(server.clone(), token);
            save_credentials(&creds)?;
            println!("logged in to {server} as {}{}", st.bold(&who.user), if who.is_admin { " (admin)" } else { "" });
        }
        Cmd::Logout { server } => {
            let server = normalize_server(
                &server
                    .clone()
                    .or_else(|| cli.server.clone())
                    .or_else(|| wc.as_ref().map(|w| w.config.server.clone()))
                    .context("which server?")?,
            );
            let mut creds = load_credentials();
            creds.remove(&server);
            save_credentials(&creds)?;
            println!("logged out of {server}");
        }
        Cmd::Whoami => {
            let server = cli.server.clone().or_else(|| wc.as_ref().map(|w| w.config.server.clone())).context("pass --server")?;
            let client = connect(&server, "").await?;
            let w = client.whoami().await?;
            println!(
                "{} on a server in {} mode{}",
                if w.user.is_empty() { "anonymous".to_string() } else { st.bold(&w.user) },
                w.auth_mode,
                if w.can_write { "; you can write" } else { "; read-only" }
            );
        }
        Cmd::Watch { all } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let mut s = client.watch(*all).await?;
            eprintln!("{}", st.dim("watching for events (Ctrl-C to stop)…"));
            use futures::StreamExt;
            while let Some(e) = s.next().await {
                let e = e?;
                let short = |b: &[u8]| ChangesetId::from_slice(b).map(|c| c.short()).unwrap_or_else(|_| "∅".into());
                match e.kind.as_str() {
                    "ref" => println!(
                        "{} {} moved {} {} → {}",
                        st.dim(&e.repo),
                        st.bold(&e.actor),
                        st.cyan(&String::from_utf8_lossy(&e.ref_name)),
                        st.yellow(&short(&e.old)),
                        st.yellow(&short(&e.new))
                    ),
                    "changesets" => {
                        println!("{} {} new changeset(s), latest {}", st.dim(&e.repo), e.count, st.yellow(&short(&e.new)))
                    }
                    "review" => println!("{} review #{} {}", st.dim(&e.repo), e.review, e.detail),
                    other => println!("{} {other}", st.dim(&e.repo)),
                }
            }
        }
        Cmd::Mount { dir, rev } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = match rev {
                Some(r) => expand_rev(r, wc.as_ref())?,
                None => wc.as_ref().and_then(|w| w.state.parent).map(|p| p.to_hex()).unwrap_or_else(|| "main".into()),
            };
            let info = client.resolve(&rev).await?;
            #[cfg(target_os = "linux")]
            {
                let mtime = UNIX_EPOCH + std::time::Duration::from_millis(info.changeset.author_time.unix_ms.max(0) as u64);
                eprintln!("{} {} at {} (Ctrl-C to unmount)", st.dim("mounted"), info.id.short(), dir.display());
                vfs::mount(client, info.root_tree, mtime, dir).await?;
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (info, dir);
                bail!("flux mount needs Linux (FUSE); use a sparse working copy (flux attach --sparse) here");
            }
        }
        Cmd::Daemon { cmd } => {
            let server = normalize_server(
                &cli.server
                    .clone()
                    .or_else(|| wc.as_ref().map(|w| w.config.server.clone()))
                    .context("which server? pass --server")?,
            );
            match cmd {
                DaemonCmd::Start => {
                    if let Some(url) = flux_client::daemon::live_url(&server) {
                        println!("fluxd already running for {server} at {url}");
                        return Ok(());
                    }
                    spawn_daemon(&server)?;
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        if let Some(url) = flux_client::daemon::live_url(&server) {
                            println!("fluxd running for {server} at {url}");
                            break;
                        }
                        if std::time::Instant::now() > deadline {
                            let log = flux_client::daemon::dir_for(&server).map(|d| d.join("fluxd.log"));
                            bail!("fluxd did not start; see {}", log.map(|p| p.display().to_string()).unwrap_or_default());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                }
                DaemonCmd::Stop => {
                    let (Some(state), Some(url)) =
                        (flux_client::daemon::read_state(&server), flux_client::daemon::live_url(&server))
                    else {
                        println!("no fluxd running for {server}");
                        return Ok(());
                    };
                    let (status, _) = daemon_http(&url, "POST", "/-/fluxd/shutdown", &[("x-fluxd-secret", &state.secret)])?;
                    if status != 200 {
                        bail!("fluxd refused to stop (HTTP {status})");
                    }
                    for _ in 0..200 {
                        if flux_client::daemon::live_url(&server).is_none() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    println!("stopped fluxd for {server}");
                }
                DaemonCmd::Status => {
                    let auto = if flux_client::daemon::auto_start_enabled() { "on" } else { "off" };
                    match (flux_client::daemon::read_state(&server), flux_client::daemon::live_url(&server)) {
                        (Some(state), Some(url)) => {
                            let (_, metrics) = daemon_http(&url, "GET", "/-/cache/metrics", &[])?;
                            let metric = |name: &str| {
                                metrics.lines().find_map(|l| l.strip_prefix(name)?.trim().parse::<f64>().ok()).unwrap_or(0.0)
                            };
                            let (hits, misses) = (metric("flux_cache_hits_total"), metric("flux_cache_misses_total"));
                            println!("fluxd {} for {server}", st.green("running"));
                            println!("  address   {url} (pid {})", state.pid);
                            println!("  up        {}", ago(state.started_ms).trim_end_matches(" ago"));
                            println!(
                                "  cache     {} objects, {:.1} MiB; {:.0}% hits ({} of {})",
                                metric("flux_cache_objects") as u64,
                                metric("flux_cache_bytes") / (1 << 20) as f64,
                                100.0 * hits / (hits + misses).max(1.0),
                                hits as u64,
                                (hits + misses) as u64
                            );
                            println!("  auto-start {auto}");
                        }
                        _ => println!("fluxd {} for {server} (auto-start {auto})", st.dim("not running")),
                    }
                }
                DaemonCmd::Enable => {
                    flux_client::daemon::set_auto_start(true)?;
                    println!("fluxd will start automatically when needed");
                }
                DaemonCmd::Disable => {
                    flux_client::daemon::set_auto_start(false)?;
                    println!("fluxd will no longer start automatically");
                }
            }
        }
        Cmd::Web { path } => {
            let wc = wc.context("not in a working copy")?;
            let rev = wc.state.bookmark.clone().or_else(|| wc.state.parent.map(|p| p.to_hex())).unwrap_or_else(|| "main".into());
            let p = match path {
                Some(p) => repo_path_arg(&wc, p)?,
                None => repo_path_arg(&wc, ".")?,
            };
            let url = match p {
                Some(p) => {
                    let is_file = wc.fs_path(&p).is_file();
                    format!("{}/{}/-/{}/{}/{}", wc.config.server, wc.config.repo, if is_file { "blob" } else { "tree" }, rev, p)
                }
                None => format!("{}/{}/-/tree/{}", wc.config.server, wc.config.repo, rev),
            };
            println!("{url}");
            let _ = open::that(&url);
        }
        Cmd::Migrate { cmd } => {
            let server = migrate_server(&cli, wc.as_ref())?;
            migrate::migrate(&server, token_for(&server), cmd).await?;
        }
        Cmd::Mirror(args) => {
            let server = migrate_server(&cli, wc.as_ref())?;
            migrate::mirror(&server, token_for(&server), args).await?;
        }
    }
    Ok(())
}

fn migrate_server(cli: &Cli, wc: Option<&WorkingCopy>) -> Result<String> {
    let server = cli.server.clone().or_else(|| wc.map(|w| w.config.server.clone()));
    Ok(normalize_server(&server.context("which flux server? pass --server or set FLUX_SERVER")?))
}

fn human(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        _ => format!("{:.1} GB", n as f64 / 1_073_741_824.0),
    }
}

fn print_oneline(cs: &CsInfo, st: &Style) {
    let c = &cs.changeset;
    let merge = if c.parents.len() > 1 { st.dim(" (merge)") } else { String::new() };
    println!(
        "{} {} {} {}{merge}",
        st.yellow(&cs.id.to_hex()[..10]),
        subject(&c.message),
        st.dim(&format!("— {}", author_name(&c.author))),
        st.dim(&ago(c.author_time.unix_ms))
    );
}

fn print_header(cs: &CsInfo, st: &Style) {
    let c = &cs.changeset;
    println!("{} {}", st.yellow("changeset"), st.yellow(&cs.id.to_string()));
    if let Some(g) = cs.git_sha1 {
        println!("git        {}", hex(&g));
    }
    for p in &c.parents {
        println!("parent     {}", p.short());
    }
    println!("author     {}", String::from_utf8_lossy(&c.author));
    println!("date       {} ({})", ago(c.author_time.unix_ms), c.author_time.unix_ms);
    println!();
    for line in String::from_utf8_lossy(&c.message).lines() {
        println!("    {line}");
    }
    println!();
}

fn print_unified(path: &str, old: &[u8], new: &[u8], st: &Style) {
    let bin = |d: &[u8]| d[..d.len().min(8192)].contains(&0) || std::str::from_utf8(d).is_err();
    if bin(old) || bin(new) {
        println!("{}", st.bold(&format!("Binary files a/{path} and b/{path} differ")));
        return;
    }
    let (a, b) = (String::from_utf8_lossy(old), String::from_utf8_lossy(new));
    let d = similar::TextDiff::configure().algorithm(similar::Algorithm::Patience).diff_lines(a.as_ref(), b.as_ref());
    let text = d.unified_diff().header(&format!("a/{path}"), &format!("b/{path}")).to_string();
    for line in text.lines() {
        let out = if line.starts_with("+++") || line.starts_with("---") {
            st.bold(line)
        } else if line.starts_with('+') {
            st.green(line)
        } else if line.starts_with('-') {
            st.red(line)
        } else if line.starts_with("@@") {
            st.cyan(line)
        } else {
            line.to_string()
        };
        println!("{out}");
    }
}

async fn fetch_one(client: &Client, id: Option<ContentId>) -> Result<Vec<u8>> {
    let Some(id) = id else { return Ok(vec![]) };
    let mut buf = Vec::new();
    client
        .get_contents(&[id], |_, d| {
            buf = d;
            Ok(())
        })
        .await?;
    Ok(buf)
}

async fn print_changeset_diff(client: &Client, cs: &CsInfo, stat: bool, st: &Style) -> Result<()> {
    let entries = client.diff("", &cs.id.to_hex(), b"").await?;
    for d in &entries {
        let path = String::from_utf8_lossy(&d.path).into_owned();
        let (code, color): (&str, fn(&Style, &str) -> String) = match (&d.old, &d.new) {
            (None, Some(_)) => ("A", Style::green),
            (Some(_), None) => ("D", Style::red),
            _ => ("M", Style::yellow),
        };
        if stat {
            println!("{}", color(st, &format!("{code} {path}")));
            continue;
        }
        let cid = |f: &Option<flux_proto::v1::DiffFile>| f.as_ref().and_then(|f| ContentId::from_slice(&f.content_id).ok());
        let (old, new) = (fetch_one(client, cid(&d.old)).await?, fetch_one(client, cid(&d.new)).await?);
        print_unified(&path, &old, &new, st);
    }
    Ok(())
}

/// Target state for a checkout.
struct TargetFile {
    kind: Kind,
    id: [u8; 32],
    size: u64,
    content: Option<ContentId>,
}

async fn checkout(client: &Client, wc: &mut WorkingCopy, rev: &str, force: bool) -> Result<()> {
    let started = Instant::now();
    if !force {
        let dirty: Vec<_> = wc.status()?.into_iter().filter(|e| e.change != Change::Untracked).collect();
        if !dirty.is_empty() {
            bail!("working copy has {} uncommitted change(s) (e.g. {}); commit or use --force", dirty.len(), dirty[0].path);
        }
    }
    let info = client.resolve(rev).await?;
    let sparse = wc.sparse_prefixes();
    // One round trip: the server expands the whole (sparse) tree.
    let trees = client.get_trees(&[info.root_tree], 4096, &sparse, b"").await?;
    let mut target: BTreeMap<RepoPath, TargetFile> = BTreeMap::new();
    for (_, dir, tree) in &trees {
        for e in tree.entries() {
            let Some(fk) = e.file_kind() else { continue };
            let mut full = dir.clone();
            if !full.is_empty() {
                full.push(b'/');
            }
            full.extend_from_slice(e.name.as_bytes());
            if !wc.in_sparse(&full) {
                continue;
            }
            let path = RepoPath::new(full)?;
            let id = match fk {
                FileKind::GitLink(sha) => {
                    let mut id = [0u8; 32];
                    id[..20].copy_from_slice(&sha);
                    id
                }
                _ => *e.content_id().unwrap().as_bytes(),
            };
            target.insert(path, TargetFile { kind: Kind::from_core(&fk), id, size: e.size, content: e.content_id() });
        }
    }

    // Remove files that are no longer present.
    let mut removed = 0;
    for path in wc.state.files.keys().cloned().collect::<Vec<_>>() {
        if !target.contains_key(&path) {
            wc.remove_file(&path)?;
            wc.state.files.remove(&path);
            removed += 1;
        }
    }

    // Write new or changed files, fetching each needed content once.
    let mut wanted: HashMap<ContentId, Vec<RepoPath>> = HashMap::new();
    let mut gitlinks = Vec::new();
    for (path, t) in &target {
        let cur = wc.state.files.get(path);
        let unchanged = cur.is_some_and(|c| c.kind == t.kind && c.id == t.id);
        if !force && unchanged && wc.fs_path(path).symlink_metadata().is_ok() {
            continue;
        }
        match t.content {
            Some(c) => wanted.entry(c).or_default().push(path.clone()),
            None => gitlinks.push(path.clone()),
        }
    }
    let ids: Vec<ContentId> = wanted.keys().copied().collect();
    let mut written: Vec<(RepoPath, Entry)> = Vec::new();
    // Buffer fetched contents and write them in parallel batches.
    const BATCH_BYTES: usize = 64 << 20;
    let mut batch: Vec<(RepoPath, Kind, Vec<u8>)> = Vec::new();
    let mut batch_bytes = 0usize;
    let flush = |batch: &mut Vec<(RepoPath, Kind, Vec<u8>)>, written: &mut Vec<(RepoPath, Entry)>| -> Result<()> {
        for (path, md) in wc.write_files(std::mem::take(batch))? {
            let t = &target[&path];
            written.push((path, Entry { kind: t.kind, id: t.id, size: t.size, mtime_ns: mtime_ns(&md) }));
        }
        Ok(())
    };
    client
        .get_contents(&ids, |cid, data| {
            for path in &wanted[&cid] {
                batch_bytes += data.len();
                batch.push((path.clone(), target[path].kind, data.clone()));
            }
            if batch_bytes >= BATCH_BYTES {
                flush(&mut batch, &mut written)?;
                batch_bytes = 0;
            }
            Ok(())
        })
        .await?;
    flush(&mut batch, &mut written)?;
    for path in gitlinks {
        let t = &target[&path];
        let md = wc.write_file(&path, Kind::GitLink, &[])?;
        written.push((path.clone(), Entry { kind: Kind::GitLink, id: t.id, size: 0, mtime_ns: mtime_ns(&md) }));
    }
    let n_written = written.len();
    for (k, e) in written {
        wc.state.files.insert(k, e);
    }

    wc.state.parent = Some(info.id);
    wc.state.root_tree = Some(info.root_tree);
    let is_ref = client.list_refs(rev.as_bytes()).await?.iter().any(|r| r.name == rev.as_bytes());
    // Follow the branch when checking one out; a bare changeset detaches.
    wc.state.bookmark = is_ref.then(|| rev.to_string());
    wc.save_state()?;
    println!(
        "checked out {}{}: {} files ({} written, {} removed) in {:.0} ms",
        info.id.short(),
        wc.state.bookmark.as_ref().map(|b| format!(" ({b})")).unwrap_or_default(),
        target.len(),
        n_written,
        removed,
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

async fn diff_wc(client: &Client, wc: &WorkingCopy, stat: bool, filters: &[RepoPath], st: &Style) -> Result<()> {
    for mut e in wc.status()? {
        if e.change == Change::Untracked || !WorkingCopy::matches(&e.path, filters) {
            continue;
        }
        wc.load_change(&mut e)?;
        if stat {
            let line = if e.new.is_none() { st.red(&format!("D {}", e.path)) } else { st.yellow(&format!("M {}", e.path)) };
            println!("{line}");
            continue;
        }
        let old = fetch_one(client, wc.state.files.get(&e.path).and_then(|x| x.content_id())).await?;
        let new = e.new.as_ref().map(|(_, d)| d.clone()).unwrap_or_default();
        print_unified(&e.path.to_string(), &old, &new, st);
    }
    Ok(())
}

async fn revert(client: &Client, wc: &mut WorkingCopy, filters: &[RepoPath]) -> Result<()> {
    let mut n = 0;
    for e in wc.status()? {
        if !WorkingCopy::matches(&e.path, filters) {
            continue;
        }
        match e.change {
            Change::Untracked => continue,
            Change::Modified | Change::Deleted => {
                let entry = wc.state.files[&e.path];
                let data = fetch_one(client, entry.content_id()).await?;
                let md = wc.write_file(&e.path, entry.kind, &data)?;
                wc.state.files.insert(e.path.clone(), Entry { mtime_ns: mtime_ns(&md), ..entry });
                n += 1;
            }
        }
    }
    wc.save_state()?;
    println!("reverted {n} file(s)");
    Ok(())
}

fn author() -> Vec<u8> {
    if let Ok(a) = std::env::var("FLUX_AUTHOR") {
        return a.into_bytes();
    }
    let git = |k: &str| {
        std::process::Command::new("git")
            .args(["config", "--get", k])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    match (git("user.name"), git("user.email")) {
        (Some(n), Some(e)) => format!("{n} <{e}>").into_bytes(),
        _ => {
            let u = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
            format!("{u} <{u}@localhost>").into_bytes()
        }
    }
}

fn local_tz_offset_min() -> i16 {
    // `date +%z` is the portable way without a tz database dependency.
    std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.len() != 5 {
                return None;
            }
            let sign = if s.starts_with('-') { -1 } else { 1 };
            let h: i16 = s[1..3].parse().ok()?;
            let m: i16 = s[3..5].parse().ok()?;
            Some(sign * (h * 60 + m))
        })
        .unwrap_or(0)
}

async fn commit(client: &Client, wc: &mut WorkingCopy, message: &str, all: bool, filters: &[RepoPath]) -> Result<()> {
    let started = Instant::now();
    let mut changes: Vec<_> = wc
        .status()?
        .into_iter()
        .filter(|e| (all || e.change != Change::Untracked) && WorkingCopy::matches(&e.path, filters))
        .collect();
    if changes.is_empty() {
        println!("nothing to commit");
        return Ok(());
    }
    for e in changes.iter_mut() {
        wc.load_change(e)?;
    }
    // Upload with delta hints: the previous version of each path.
    let uploads: Vec<(ContentId, bytes::Bytes, Option<ContentId>)> = changes
        .iter()
        .filter_map(|e| {
            let (_, data) = e.new.as_ref()?;
            let base = wc.state.files.get(&e.path).and_then(|x| x.content_id());
            Some((ContentId::hash(data), bytes::Bytes::copy_from_slice(data), base))
        })
        .collect();
    client.upload_known(uploads).await?;

    let mut file_changes = BTreeMap::new();
    for e in &changes {
        let fc = match &e.new {
            None => FileChange::Deletion,
            Some((kind, data)) => FileChange::Change {
                content_id: ContentId::hash(data),
                size: data.len() as u64,
                kind: match kind {
                    Kind::Executable => FileKind::Executable,
                    Kind::Symlink => FileKind::Symlink,
                    _ => FileKind::Regular,
                },
                copy_from: None,
            },
        };
        file_changes.insert(e.path.clone(), fc);
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    let tz = local_tz_offset_min();
    let mut msg = message.as_bytes().to_vec();
    if !msg.ends_with(b"\n") {
        msg.push(b'\n');
    }
    let who = author();
    let cs = Changeset {
        parents: wc.state.parent.into_iter().collect(),
        author: who.clone(),
        author_time: Timestamp { unix_ms: now, tz_offset_min: tz },
        committer: Some(who),
        committer_time: Some(Timestamp { unix_ms: now, tz_offset_min: tz }),
        message: msg,
        file_changes,
        extra: BTreeMap::new(),
    };
    let info = client.create_changeset(&cs).await?;

    // Advance the followed branch with a compare-and-swap.
    let mut moved = None;
    if let Some(bm) = wc.state.bookmark.clone() {
        let expect = match wc.state.parent {
            Some(p) => Expect::Target(p.as_bytes().to_vec()),
            None => Expect::Absent(true),
        };
        if client.update_ref(&bm, expect, Some(info.id), "flux commit").await? {
            moved = Some(bm);
        } else {
            eprintln!(
                "note: {bm} moved since you last synced; {} was created but {bm} was not updated.\n      Run `flux land --onto {bm}` to rebase and land it, or `flux review create`.",
                info.id.short()
            );
            wc.state.bookmark = None;
        }
    }

    for e in &changes {
        match &e.new {
            None => {
                wc.state.files.remove(&e.path);
            }
            Some((kind, data)) => {
                let md = std::fs::symlink_metadata(wc.fs_path(&e.path))?;
                wc.state.files.insert(
                    e.path.clone(),
                    Entry {
                        kind: *kind,
                        id: *ContentId::hash(data).as_bytes(),
                        size: data.len() as u64,
                        mtime_ns: mtime_ns(&md),
                    },
                );
            }
        }
    }
    wc.state.parent = Some(info.id);
    wc.state.root_tree = Some(info.root_tree);
    wc.save_state()?;
    println!(
        "committed {} ({} file{}){} in {:.0} ms",
        info.id.short(),
        changes.len(),
        if changes.len() == 1 { "" } else { "s" },
        moved.map(|b| format!(" on {b}")).unwrap_or_default(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

async fn review(cli: &Cli, wc: Option<WorkingCopy>, cmd: &ReviewCmd, st: &Style) -> Result<()> {
    let client = client_for(cli, wc.as_ref()).await?;
    let web = |n: i64| {
        let server = cli.server.clone().or_else(|| wc.as_ref().map(|w| w.config.server.clone())).unwrap_or_default();
        let repo = cli.repo.clone().or_else(|| wc.as_ref().map(|w| w.config.repo.clone())).unwrap_or_default();
        format!("{}/{repo}/-/reviews/{n}", normalize_server(&server))
    };
    match cmd {
        ReviewCmd::Create { title, message, head, target } => {
            let head = expand_rev(head, wc.as_ref())?;
            let title = match title {
                Some(t) => t.clone(),
                None => subject(&client.resolve(&head).await?.changeset.message),
            };
            let r = client.create_review(&title, message, &head, target).await?;
            println!("opened review #{} {}", r.number, st.bold(&r.title));
            println!("{}", st.dim(&web(r.number)));
        }
        ReviewCmd::List { state } => {
            let rs = client.list_reviews(if state == "all" { "" } else { state }).await?;
            if rs.is_empty() {
                println!("{}", st.dim(&format!("no {state} reviews")));
            }
            for r in rs {
                let badge = match r.state.as_str() {
                    "open" => st.green("open  "),
                    "landed" => st.paint("35", "landed"),
                    _ => st.red("closed"),
                };
                println!(
                    "{} {badge} {}  {}",
                    st.yellow(&format!("#{:<4}", r.number)),
                    r.title,
                    st.dim(&format!("{} · {}", r.author, ago(r.updated_ms)))
                );
            }
        }
        ReviewCmd::Show { number } => {
            let d = client.get_review(*number).await?;
            let r = d.review.context("missing review")?;
            println!("{} {}  [{}]", st.yellow(&format!("#{}", r.number)), st.bold(&r.title), r.state);
            println!("{}", st.dim(&format!("{} → {} · opened {}", r.author, r.target, ago(r.created_ms))));
            if !r.body.is_empty() {
                println!("\n{}\n", r.body);
            }
            println!("{}", st.bold("changesets"));
            for i in d.stack {
                let info: CsInfo = i.try_into()?;
                print_oneline(&info, st);
            }
            println!("{}", st.bold("timeline"));
            for e in d.events {
                println!(
                    "  {} {} {}{}",
                    st.dim(&ago(e.ts_ms)),
                    st.bold(&e.author),
                    e.kind,
                    if e.body.is_empty() { String::new() } else { format!(": {}", e.body) }
                );
            }
            println!("{}", st.dim(&web(r.number)));
        }
        ReviewCmd::Comment { number, message } => {
            client.update_review(*number, "comment", message, "").await?;
            println!("commented on #{number}");
        }
        ReviewCmd::Approve { number, message } => {
            client.update_review(*number, "approve", message, "").await?;
            println!("approved #{number}");
        }
        ReviewCmd::Update { number, head } => {
            let head = expand_rev(head, wc.as_ref())?;
            let r = client.update_review(*number, "update", "", &head).await?;
            println!("#{number} now points at {}", ChangesetId::from_slice(&r.head)?.short());
        }
        ReviewCmd::Land { number } => {
            let started = Instant::now();
            let r = client.land_review(*number).await?;
            let head = ChangesetId::from_slice(&r.new_head)?;
            println!(
                "landed #{number}{}: {} ({:.0} ms)",
                if r.fast_forward {
                    " (fast-forward)".to_string()
                } else {
                    format!(" (rebased {}, merged {} file(s))", r.rebased.len(), r.merged_files)
                },
                st.yellow(&head.short()),
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        ReviewCmd::Close { number } => {
            client.update_review(*number, "close", "", "").await?;
            println!("closed #{number}");
        }
        ReviewCmd::Reopen { number } => {
            client.update_review(*number, "reopen", "", "").await?;
            println!("reopened #{number}");
        }
    }
    Ok(())
}
