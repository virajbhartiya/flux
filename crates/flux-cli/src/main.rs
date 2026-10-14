//! `flux`: the command-line client (SPEC §13.1).

mod wc;

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use flux_client::{Client, CsInfo};
use flux_core::{Changeset, ContentId, FileChange, FileKind, RepoPath, Timestamp};
use flux_proto::v1::update_ref_request::Expect;

use crate::wc::{Change, Config, Entry, Kind, WorkingCopy, key_path, mtime_ns, path_key};

#[derive(Parser)]
#[command(name = "flux", version, about = "flux source control client")]
struct Cli {
    /// Server URL (defaults to the working copy's server).
    #[arg(long, global = true, env = "FLUX_SERVER")]
    server: Option<String>,
    /// Repository (defaults to the working copy's repository).
    #[arg(long, global = true, env = "FLUX_REPO")]
    repo: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a working copy attached to a server repository.
    Attach {
        server: String,
        repo: String,
        dir: Option<PathBuf>,
        /// Only materialize these directory prefixes (repeatable).
        #[arg(long)]
        sparse: Vec<String>,
        /// Revision to check out (default: main, if it exists).
        #[arg(long)]
        rev: Option<String>,
    },
    /// Update the working copy to a revision.
    Checkout {
        rev: String,
        /// Discard local modifications.
        #[arg(long)]
        force: bool,
    },
    /// Show working-copy changes.
    Status,
    /// Show working-copy changes as a diff.
    Diff {
        /// Names only.
        #[arg(long)]
        stat: bool,
    },
    /// Create a changeset from working-copy changes.
    Commit {
        #[arg(short, long)]
        message: String,
        /// Also commit untracked files.
        #[arg(short = 'A', long)]
        all: bool,
    },
    /// Show history.
    Log {
        #[arg(default_value = "@")]
        rev: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        first_parent: bool,
        /// Only changesets touching this path.
        #[arg(long)]
        path: Option<String>,
    },
    /// Show a changeset and the files it changes.
    Show {
        #[arg(default_value = "@")]
        rev: String,
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
    /// List refs.
    Refs {
        #[arg(default_value = "")]
        prefix: String,
    },
    /// Point a ref at a revision (fast-forward check unless --force).
    SetRef {
        name: String,
        rev: String,
        #[arg(long)]
        force: bool,
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
}

#[derive(Subcommand)]
enum SparseCmd {
    Show,
    /// Replace the profile (no prefixes = full checkout) and reconcile.
    Set { prefixes: Vec<String> },
}

#[derive(Subcommand)]
enum RepoCmd {
    Create { name: String },
    List,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn client_for(cli: &Cli, wc: Option<&WorkingCopy>) -> Result<Client> {
    let server = cli.server.clone().or_else(|| wc.map(|w| w.config.server.clone()));
    let repo = cli.repo.clone().or_else(|| wc.map(|w| w.config.repo.clone()));
    match (server, repo) {
        (Some(s), Some(r)) => Client::connect(&s, &r).await,
        _ => bail!("no working copy here; pass --server and --repo"),
    }
}

/// Resolve `@` to the working-copy parent.
fn expand_rev(rev: &str, wc: Option<&WorkingCopy>) -> Result<String> {
    if let Some(rest) = rev.strip_prefix('@') {
        let parent = wc
            .and_then(|w| w.state.parent)
            .context("`@` needs a working copy with a checked-out changeset")?;
        return Ok(format!("{}{rest}", parent.to_hex()));
    }
    Ok(rev.to_string())
}

async fn run(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let wc = WorkingCopy::find(&cwd).ok();
    match &cli.cmd {
        Cmd::Attach { server, repo, dir, sparse, rev } => {
            let dir = dir.clone().unwrap_or_else(|| PathBuf::from(repo.rsplit('/').next().unwrap_or(repo)));
            std::fs::create_dir_all(&dir)?;
            let client = Client::connect(server, repo).await?;
            let mut wc = WorkingCopy::init(
                &dir,
                Config { server: server.clone(), repo: repo.clone(), sparse: sparse.iter().map(|s| s.trim_matches('/').to_string()).collect() },
            )?;
            let rev = match rev {
                Some(r) => Some(r.clone()),
                None => client.list_refs(b"main").await?.iter().any(|r| r.name == b"main").then(|| "main".to_string()),
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
        Cmd::Status => {
            let wc = wc.context("not in a working copy")?;
            let st = wc.status()?;
            if let Some(p) = wc.state.parent {
                println!("parent: {}{}", p.short(), wc.state.bookmark.as_ref().map(|b| format!(" ({b})")).unwrap_or_default());
            }
            if st.is_empty() {
                println!("clean");
            }
            for e in st {
                let c = match e.change {
                    Change::Modified => "M",
                    Change::Deleted => "D",
                    Change::Untracked => "?",
                };
                println!("{c} {}", e.path);
            }
        }
        Cmd::Diff { stat } => {
            let wc = wc.context("not in a working copy")?;
            let client = client_for(&cli, Some(&wc)).await?;
            diff_wc(&client, &wc, *stat).await?;
        }
        Cmd::Commit { message, all } => {
            let mut wc = wc.context("not in a working copy")?;
            let client = client_for(&cli, Some(&wc)).await?;
            commit(&client, &mut wc, message, *all).await?;
        }
        Cmd::Log { rev, limit, first_parent, path } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            for cs in client.log(&rev, *limit, *first_parent, path.as_deref().map(str::as_bytes)).await? {
                print_header(&cs, true);
            }
        }
        Cmd::Show { rev } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let cs = client.resolve(&rev).await?;
            print_header(&cs, false);
            for d in client.diff("", &cs.id.to_hex(), b"").await? {
                let c = match (&d.old, &d.new) {
                    (None, Some(_)) => "A",
                    (Some(_), None) => "D",
                    _ => "M",
                };
                println!("{c} {}", String::from_utf8_lossy(&d.path));
            }
        }
        Cmd::Cat { rev, path } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            std::io::stdout().write_all(&client.read_file(&rev, path.as_bytes()).await?)?;
        }
        Cmd::Ls { rev, path } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            let rev = expand_rev(rev, wc.as_ref())?;
            let dir = client.list_dir(&rev, path.trim_matches('/').as_bytes()).await?;
            for e in dir.entries {
                let name = String::from_utf8_lossy(&e.name);
                if e.is_dir {
                    println!("{:>12}  {:>8} files  {name}/", e.size, e.descendant_files);
                } else {
                    println!("{:>12}  {:>14}  {name}", e.size, "");
                }
            }
        }
        Cmd::Refs { prefix } => {
            let client = client_for(&cli, wc.as_ref()).await?;
            for r in client.list_refs(prefix.as_bytes()).await? {
                let id = flux_core::ChangesetId::from_slice(&r.target)?;
                println!("{}  {}  (v{})", id.short(), String::from_utf8_lossy(&r.name), r.version);
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
                        let cur_id = flux_core::ChangesetId::from_slice(&cur.target)?;
                        // Fast-forward check: current must be an ancestor of target.
                        let anc = client.log(&target.id.to_hex(), 0, false, None).await?;
                        if !anc.iter().any(|c| c.id == cur_id) {
                            bail!("{name} ({}) is not an ancestor of {}; use --force", cur_id.short(), target.id.short());
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
                        checkout(&client, &mut wc, &p.to_hex(), true).await?;
                    }
                }
            }
        }
        Cmd::Repo { cmd } => {
            let server = cli.server.clone().or_else(|| wc.as_ref().map(|w| w.config.server.clone())).context("--server required")?;
            let client = Client::connect(&server, "").await?;
            match cmd {
                RepoCmd::Create { name } => {
                    client.create_repo(name).await?;
                    println!("created {name}");
                }
                RepoCmd::List => {
                    for r in client.list_repos().await? {
                        println!("{r}");
                    }
                }
            }
        }
    }
    Ok(())
}

fn print_header(cs: &CsInfo, oneline: bool) {
    let c = &cs.changeset;
    let subject = String::from_utf8_lossy(c.message.split(|b| *b == b'\n').next().unwrap_or_default()).into_owned();
    if oneline {
        let git = cs.git_sha1.map(|g| format!(" git:{}", &hex(&g)[..10])).unwrap_or_default();
        println!("{}{git}  {}  {subject}", cs.id.short(), String::from_utf8_lossy(&c.author));
        return;
    }
    println!("changeset  {}", cs.id);
    if let Some(g) = cs.git_sha1 {
        println!("git        {}", hex(&g));
    }
    for p in &c.parents {
        println!("parent     {}", p.short());
    }
    println!("generation {}", cs.generation);
    println!("author     {}", String::from_utf8_lossy(&c.author));
    println!("date       {} ms (tz {:+} min)", c.author_time.unix_ms, c.author_time.tz_offset_min);
    println!();
    for line in String::from_utf8_lossy(&c.message).lines() {
        println!("    {line}");
    }
    println!();
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Target state for a checkout: key → (path, kind, id string, size, content).
struct TargetFile {
    path: RepoPath,
    kind: Kind,
    id: String,
    size: u64,
    content: Option<ContentId>,
}

async fn checkout(client: &Client, wc: &mut WorkingCopy, rev: &str, force: bool) -> Result<()> {
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
    let mut target: BTreeMap<String, TargetFile> = BTreeMap::new();
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
                FileKind::GitLink(sha) => hex(&sha),
                _ => e.content_id().unwrap().to_string(),
            };
            target.insert(path_key(&path), TargetFile { path, kind: Kind::from_core(&fk), id, size: e.size, content: e.content_id() });
        }
    }

    // Remove files that are no longer present.
    let mut removed = 0;
    for key in wc.state.files.keys().cloned().collect::<Vec<_>>() {
        if !target.contains_key(&key) {
            wc.remove_file(&key_path(&key)?)?;
            wc.state.files.remove(&key);
            removed += 1;
        }
    }

    // Write new or changed files, fetching each needed content once.
    let mut wanted: HashMap<ContentId, Vec<String>> = HashMap::new();
    let mut gitlinks = Vec::new();
    for (key, t) in &target {
        let cur = wc.state.files.get(key);
        let on_disk = wc.fs_path(&t.path).symlink_metadata().is_ok();
        // Unchanged and present: keep it (unless --force rewrites everything).
        if !force && on_disk && cur.is_some_and(|c| c.kind == t.kind && c.id == t.id) {
            continue;
        }
        match t.content {
            Some(c) => wanted.entry(c).or_default().push(key.clone()),
            None => gitlinks.push(key.clone()),
        }
    }
    let ids: Vec<ContentId> = wanted.keys().copied().collect();
    let mut written: Vec<(String, Entry)> = Vec::new();
    client
        .get_contents(&ids, |cid, data| {
            for key in &wanted[&cid] {
                let t = &target[key];
                let md = wc.write_file(&t.path, t.kind, &data)?;
                written.push((key.clone(), Entry { kind: t.kind, id: t.id.clone(), size: t.size, mtime_ns: mtime_ns(&md) }));
            }
            Ok(())
        })
        .await?;
    for key in gitlinks {
        let t = &target[&key];
        let md = wc.write_file(&t.path, Kind::GitLink, &[])?;
        written.push((key.clone(), Entry { kind: Kind::GitLink, id: t.id.clone(), size: 0, mtime_ns: mtime_ns(&md) }));
    }
    let n_written = written.len();
    for (k, e) in written {
        wc.state.files.insert(k, e);
    }

    wc.state.parent = Some(info.id);
    wc.state.root_tree = Some(info.root_tree);
    wc.state.bookmark = client.list_refs(rev.as_bytes()).await?.iter().any(|r| r.name == rev.as_bytes()).then(|| rev.to_string());
    wc.save_state()?;
    println!(
        "checked out {}{}: {} files ({} written, {} removed)",
        info.id.short(),
        wc.state.bookmark.as_ref().map(|b| format!(" ({b})")).unwrap_or_default(),
        target.len(),
        n_written,
        removed
    );
    Ok(())
}

async fn diff_wc(client: &Client, wc: &WorkingCopy, stat: bool) -> Result<()> {
    let st = wc.status()?;
    for e in st {
        if e.change == Change::Untracked {
            continue;
        }
        let key = path_key(&e.path);
        let old = match wc.state.files.get(&key) {
            Some(entry) if entry.kind != Kind::GitLink => {
                let id = ContentId::parse(&entry.id)?;
                let mut buf = None;
                client.get_contents(&[id], |_, d| {
                    buf = Some(d);
                    Ok(())
                })
                .await?;
                buf
            }
            _ => None,
        };
        let new = e.new.as_ref().map(|(_, d)| d.clone());
        if stat {
            println!("{} {}", if new.is_none() { "D" } else { "M" }, e.path);
            continue;
        }
        let (a, b) = (old.unwrap_or_default(), new.unwrap_or_default());
        match (std::str::from_utf8(&a), std::str::from_utf8(&b)) {
            (Ok(a), Ok(b)) if !a.contains('\0') && !b.contains('\0') => {
                let d = similar::TextDiff::from_lines(a, b);
                print!("{}", d.unified_diff().header(&format!("a/{}", e.path), &format!("b/{}", e.path)));
            }
            _ => println!("Binary files a/{0} and b/{0} differ", e.path),
        }
    }
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

async fn commit(client: &Client, wc: &mut WorkingCopy, message: &str, all: bool) -> Result<()> {
    let changes: Vec<_> = wc.status()?.into_iter().filter(|e| all || e.change != Change::Untracked).collect();
    if changes.is_empty() {
        println!("nothing to commit");
        return Ok(());
    }
    let datas: Vec<&[u8]> = changes.iter().filter_map(|e| e.new.as_ref().map(|(_, d)| d.as_slice())).collect();
    client.upload(&datas).await?;

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
    let mut msg = message.as_bytes().to_vec();
    if !msg.ends_with(b"\n") {
        msg.push(b'\n');
    }
    let who = author();
    let cs = Changeset {
        parents: wc.state.parent.into_iter().collect(),
        author: who.clone(),
        author_time: Timestamp { unix_ms: now, tz_offset_min: 0 },
        committer: Some(who),
        committer_time: Some(Timestamp { unix_ms: now, tz_offset_min: 0 }),
        message: msg,
        file_changes,
        extra: BTreeMap::new(),
    };
    let info = client.create_changeset(&cs).await?;

    // Advance the followed bookmark with a compare-and-swap.
    if let Some(bm) = wc.state.bookmark.clone() {
        let expect = match wc.state.parent {
            Some(p) => Expect::Target(p.as_bytes().to_vec()),
            None => Expect::Absent(true),
        };
        if !client.update_ref(&bm, expect, Some(info.id), "flux commit").await? {
            eprintln!("warning: {bm} moved since checkout; {} was created but {bm} was not updated", info.id.short());
            wc.state.bookmark = None;
        }
    }

    for e in &changes {
        let key = path_key(&e.path);
        match &e.new {
            None => {
                wc.state.files.remove(&key);
            }
            Some((kind, data)) => {
                let md = std::fs::symlink_metadata(wc.fs_path(&e.path))?;
                wc.state.files.insert(
                    key,
                    Entry { kind: *kind, id: ContentId::hash(data).to_string(), size: data.len() as u64, mtime_ns: mtime_ns(&md) },
                );
            }
        }
    }
    wc.state.parent = Some(info.id);
    wc.state.root_tree = Some(info.root_tree);
    wc.save_state()?;
    println!(
        "committed {} ({} file{}){}",
        info.id.short(),
        changes.len(),
        if changes.len() == 1 { "" } else { "s" },
        wc.state.bookmark.as_ref().map(|b| format!(" on {b}")).unwrap_or_default()
    );
    Ok(())
}
