//! `flux migrate github` and `flux mirror`: move repositories from GitHub
//! (docs/migrating-from-github.md).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use flux_migrate::{GitHub, Options, Report};

#[derive(Subcommand)]
pub enum MigrateCmd {
    /// Migrate a GitHub repository, or every repository of an organization:
    /// git history, issues, pull requests (as reviews), releases and branch
    /// protection. Re-running syncs incrementally.
    Github(GithubArgs),
}

#[derive(Args)]
pub struct GithubArgs {
    /// The GitHub repository.
    #[arg(value_name = "OWNER/REPO", required_unless_present = "org", conflicts_with = "org")]
    github_repo: Option<String>,
    /// Migrate every repository of this organization (or user).
    #[arg(long)]
    org: Option<String>,
    /// flux repository name (default: the GitHub owner/repo).
    #[arg(long, conflicts_with = "org")]
    to: Option<String>,
    /// flux owner for an organization's repositories (default: the
    /// organization's name).
    #[arg(long, conflicts_with = "github_repo")]
    to_org: Option<String>,
    /// With --org: also migrate archived repositories.
    #[arg(long, conflicts_with = "github_repo")]
    include_archived: bool,
    #[command(flatten)]
    common: Common,
}

#[derive(Args)]
pub struct MirrorArgs {
    /// The GitHub repository.
    #[arg(value_name = "OWNER/REPO")]
    github_repo: String,
    /// flux repository name (default: the GitHub owner/repo).
    #[arg(long)]
    to: Option<String>,
    /// Time between syncs (`30s`, `5m`, `1h`).
    #[arg(long, default_value = "60s")]
    interval: String,
    /// Sync once and exit.
    #[arg(long)]
    once: bool,
    /// Exit after this many consecutive failed syncs.
    #[arg(long, default_value_t = 5)]
    max_failures: u32,
    #[command(flatten)]
    common: Common,
}

#[derive(Args)]
pub struct Common {
    /// GitHub token (default: $GITHUB_TOKEN, $GH_TOKEN, or `gh auth token`).
    #[arg(long, value_name = "TOKEN")]
    github_token: Option<String>,
    /// GitHub API URL; for GitHub Enterprise Server, `https://HOST/api/v3`.
    #[arg(long, value_name = "URL", default_value = flux_migrate::github::DEFAULT_API)]
    github_api: String,
    /// Visibility of new flux repositories: public, internal or private
    /// (default: as on GitHub).
    #[arg(long)]
    visibility: Option<String>,
    /// Skip issues.
    #[arg(long)]
    no_issues: bool,
    /// Skip pull requests.
    #[arg(long)]
    no_reviews: bool,
    /// Skip releases.
    #[arg(long)]
    no_releases: bool,
    /// Skip branch protection.
    #[arg(long)]
    no_protection: bool,
    /// Create placeholder accounts for GitHub users (needs a site admin
    /// token); signing in with GitHub claims them.
    #[arg(long)]
    create_users: bool,
    /// Keep links to GitHub issues and pull requests unchanged.
    #[arg(long)]
    no_rewrite_links: bool,
    /// Where git mirrors and sync state live (default: ~/.cache/flux/migrate).
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
    /// Fetch git from this URL or path instead of GitHub.
    #[arg(long, value_name = "URL")]
    clone_url: Option<String>,
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
}

impl Common {
    fn options(&self, github_repo: &str, server: &str, token: Option<String>, flux_repo: String) -> Options {
        let mut o = Options::new(github_repo, server, &flux_repo);
        o.token = token;
        o.visibility = self.visibility.clone();
        o.issues = !self.no_issues;
        o.reviews = !self.no_reviews;
        o.releases = !self.no_releases;
        o.protection = !self.no_protection;
        o.create_users = self.create_users;
        o.rewrite_links = !self.no_rewrite_links;
        o.cache_dir = self.cache_dir.clone();
        o.clone_url = self.clone_url.clone();
        o
    }

    fn github(&self) -> Result<GitHub> {
        let host = flux_migrate::github::web_host(&self.github_api);
        let token = flux_migrate::github::resolve_token(self.github_token.as_deref(), &host);
        if token.is_none() {
            eprintln!(
                "warning: no GitHub token (pass --github-token, set GITHUB_TOKEN, or run `gh auth login`); \
                 only public data is readable, at 60 requests an hour"
            );
        }
        GitHub::new(&self.github_api, token)
    }
}

fn init_logging() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "flux_migrate=info,flux_git=info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_target(false)
        .without_time()
        .try_init();
}

fn default_name(github_repo: &str) -> Result<String> {
    let (owner, name) = flux_migrate::split_repo(github_repo)?;
    Ok(format!("{owner}/{name}"))
}

fn secs(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

fn print_report(r: &Report) {
    println!("migrated {} → {}{}", r.github, r.flux_repo, if r.created { " (new repository)" } else { "" });
    println!("  {}", r.summary());
    let phases: Vec<String> = r.phases.iter().map(|p| format!("{} {}", p.name, secs(p.ms))).collect();
    println!("  {} · {} GitHub requests · {} total", phases.join(" · "), r.github_requests, secs(r.elapsed_ms));
    if !r.warnings.is_empty() {
        println!("warnings:");
        for w in &r.warnings {
            println!("  - {w}");
        }
    }
    let steps = flux_migrate::next_steps(r);
    if !steps.is_empty() {
        println!("next steps:");
        for s in steps {
            println!("  {s}");
        }
    }
}

pub async fn migrate(server: &str, token: Option<String>, cmd: &MigrateCmd) -> Result<()> {
    let MigrateCmd::Github(a) = cmd;
    if !a.common.json {
        init_logging();
    }
    let gh = a.common.github()?;
    let Some(org) = &a.org else {
        let repo = a.github_repo.as_deref().context("pass OWNER/REPO or --org")?;
        let to = match &a.to {
            Some(t) => t.clone(),
            None => default_name(repo)?,
        };
        let report = flux_migrate::migrate(&gh, &a.common.options(repo, server, token, to)).await?;
        if a.common.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_report(&report);
        }
        return Ok(());
    };
    let all = flux_migrate::org_repos(&gh, org).await.with_context(|| format!("listing the repositories of {org}"))?;
    let (repos, archived): (Vec<_>, Vec<_>) = all.into_iter().partition(|r| a.include_archived || !r.archived);
    if !archived.is_empty() {
        eprintln!("skipping {} archived repositories (--include-archived migrates them)", archived.len());
    }
    let to_org = a.to_org.clone().unwrap_or_else(|| org.clone());
    let (total, mut reports, mut failed) = (repos.len(), Vec::new(), Vec::new());
    for (i, repo) in repos.iter().enumerate() {
        let to = format!("{to_org}/{}", repo.name);
        eprintln!("[{}/{total}] {} → {to}", i + 1, repo.full_name);
        let started = Instant::now();
        match flux_migrate::migrate(&gh, &a.common.options(&repo.full_name, server, token.clone(), to)).await {
            Ok(r) => {
                eprintln!(
                    "[{}/{total}] {} done in {}: {}{}",
                    i + 1,
                    repo.full_name,
                    secs(started.elapsed().as_millis() as u64),
                    r.summary(),
                    if r.warnings.is_empty() { String::new() } else { format!(" ({} warnings)", r.warnings.len()) }
                );
                reports.push(serde_json::to_value(&r)?);
                if !a.common.json {
                    print_report(&r);
                }
            }
            Err(e) => {
                eprintln!("[{}/{total}] {} failed: {e:#}", i + 1, repo.full_name);
                reports.push(serde_json::json!({ "github": repo.full_name, "error": format!("{e:#}") }));
                failed.push(repo.full_name.clone());
            }
        }
    }
    if a.common.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    }
    if !failed.is_empty() {
        bail!("{} of {total} repositories failed: {}", failed.len(), failed.join(", "));
    }
    Ok(())
}

/// `90`, `30s`, `5m`, `1h`, `1h30m`.
pub fn parse_interval(s: &str) -> Result<Duration> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(Duration::from_secs(n.max(1)));
    }
    let (mut total, mut num) = (0u64, String::new());
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().with_context(|| format!("invalid interval {s:?} (try 30s, 5m or 1h)"))?;
        num.clear();
        total += n * match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => bail!("invalid interval {s:?} (try 30s, 5m or 1h)"),
        };
    }
    if !num.is_empty() || total == 0 {
        bail!("invalid interval {s:?} (try 30s, 5m or 1h)");
    }
    Ok(Duration::from_secs(total))
}

pub async fn mirror(server: &str, token: Option<String>, a: &MirrorArgs) -> Result<()> {
    let interval = parse_interval(&a.interval)?;
    if !a.common.json {
        init_logging();
    }
    let gh = a.common.github()?;
    let to = match &a.to {
        Some(t) => t.clone(),
        None => default_name(&a.github_repo)?,
    };
    let mut opts = a.common.options(&a.github_repo, server, token, to.clone());
    opts.mirror = true;
    if !a.once {
        eprintln!("mirroring {} → {to} every {}s (Ctrl-C to stop)", a.github_repo, interval.as_secs());
    }
    let stamp = || flux_migrate::time::format_rfc3339(flux_migrate::time::now_ms());
    let mut failures = 0u32;
    loop {
        let result = tokio::select! {
            r = flux_migrate::migrate(&gh, &opts) => r,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("stopped");
                return Ok(());
            }
        };
        match result {
            Ok(r) => {
                failures = 0;
                if a.common.json {
                    println!("{}", serde_json::to_string(&r)?);
                } else {
                    eprintln!("{} synced {} → {}: {} in {}", stamp(), r.github, r.flux_repo, r.summary(), secs(r.elapsed_ms));
                    for w in &r.warnings {
                        eprintln!("  warning: {w}");
                    }
                }
            }
            Err(e) if a.once => return Err(e),
            Err(e) => {
                failures += 1;
                eprintln!("{} sync failed ({failures}/{}): {e:#}", stamp(), a.max_failures);
                if failures >= a.max_failures {
                    bail!("giving up after {failures} failed syncs in a row");
                }
            }
        }
        if a.once {
            return Ok(());
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = tokio::signal::ctrl_c() => {
                eprintln!("stopped");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals() {
        assert_eq!(parse_interval("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_interval("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_interval("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_interval("1h30m").unwrap(), Duration::from_secs(5400));
        for bad in ["", "m", "5x", "10m5", "0s"] {
            assert!(parse_interval(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cli_parses() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(subcommand)]
            cmd: MigrateCmd,
        }
        let t = T::try_parse_from(["t", "github", "acme/widget", "--to", "widget", "--no-issues", "--create-users"]).unwrap();
        let MigrateCmd::Github(a) = t.cmd;
        let o = a.common.options(a.github_repo.as_deref().unwrap(), "http://f", None, a.to.clone().unwrap());
        assert!(!o.issues && o.reviews && o.create_users && o.rewrite_links);
        assert!(T::try_parse_from(["t", "github", "--org", "acme", "--to-org", "eng"]).is_ok());
        assert!(T::try_parse_from(["t", "github", "acme/widget", "--org", "acme"]).is_err());
        assert!(T::try_parse_from(["t", "github"]).is_err());
        assert!(T::try_parse_from(["t", "github", "acme/widget", "--to-org", "eng"]).is_err());
    }
}
