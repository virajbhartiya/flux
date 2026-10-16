use std::path::PathBuf;

use clap::{Args as ClapArgs, Parser, Subcommand};
use flux_server::StorageConfig;
use flux_server::auth::AuthMode;

/// flux server: gRPC API and web UI on one port.
///
/// All-in-one mode: `flux-server --data-dir ./data` (SQLite + local segments).
/// Production: `--meta-url postgres://... --blob-url s3://bucket/prefix`.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(flatten)]
    storage: Storage,
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    serve: Serve,
}

#[derive(ClapArgs)]
struct Storage {
    /// Directory for the all-in-one SQLite + segment-file layout. With
    /// --meta-url and --blob-url it only holds the server's keys (secrets
    /// key, SSH host key).
    #[arg(long, env = "FLUX_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,
    #[arg(long, env = "FLUX_META_URL", global = true, requires = "blob_url")]
    meta_url: Option<String>,
    #[arg(long, env = "FLUX_BLOB_URL", global = true, requires = "meta_url")]
    blob_url: Option<String>,
}

#[derive(ClapArgs)]
struct Serve {
    #[arg(long, env = "FLUX_LISTEN", default_value = "127.0.0.1:7878")]
    listen: std::net::SocketAddr,
    /// open: no auth (single user / trusted network); token: anonymous
    /// reads, writes need a token; private: everything needs a token.
    #[arg(long, env = "FLUX_AUTH", default_value = "open")]
    auth: AuthMode,
    /// Who may create accounts: closed (site admins create them) or open.
    /// The first account is always allowed and becomes a site admin.
    #[arg(long, env = "FLUX_SIGNUP", default_value = "closed")]
    signup: flux_server::identity::SignupPolicy,
    /// Disable the web UI (gRPC only).
    #[arg(long)]
    no_web: bool,
    /// Create these repositories at startup if they don't exist.
    #[arg(long = "create-repo")]
    create_repos: Vec<String>,
    /// The URL people and webhooks use to reach this server
    /// (https://flux.example.com); used in links, payloads and sign-in redirects.
    #[arg(long, env = "FLUX_PUBLIC_URL")]
    public_url: Option<String>,
    /// Serve git over SSH on this address (e.g. 0.0.0.0:2222).
    #[arg(long, env = "FLUX_SSH_LISTEN")]
    ssh_listen: Option<std::net::SocketAddr>,
    /// SSH host key (OpenSSH format); created on first start if missing.
    /// Default: <data-dir>/ssh_host_ed25519_key.
    #[arg(long, env = "FLUX_SSH_HOST_KEY")]
    ssh_host_key: Option<PathBuf>,
    /// host[:port] shown in SSH clone URLs (default: from --public-url and --ssh-listen).
    #[arg(long, env = "FLUX_SSH_HOST")]
    ssh_host: Option<String>,
    /// 32-byte key (64 hex chars) for encrypting CI secrets and two-factor
    /// secrets. Default: <data-dir>/secret.key, created if missing. Every
    /// instance of a deployment must use the same key.
    #[arg(long, env = "FLUX_SECRET_KEY_FILE")]
    secret_key_file: Option<PathBuf>,
    /// Let webhooks reach loopback and private networks (off: SSRF guard).
    #[arg(long, env = "FLUX_WEBHOOK_ALLOW_PRIVATE")]
    webhook_allow_private: bool,
    /// Require two-factor authentication for password sign-ins.
    #[arg(long, env = "FLUX_REQUIRE_2FA")]
    require_2fa: bool,
    /// Email notifications: smtps://user:pass@host:465 or smtp://user:pass@host:587 (STARTTLS).
    #[arg(long, env = "FLUX_SMTP_URL", requires = "smtp_from", hide_env_values = true)]
    smtp_url: Option<String>,
    /// Sender address, e.g. "flux <flux@example.com>".
    #[arg(long, env = "FLUX_SMTP_FROM")]
    smtp_from: Option<String>,
    /// REST and MCP requests per minute per signed-in caller (anonymous
    /// callers: a tenth, per address). 0 turns limiting off.
    #[arg(long, env = "FLUX_API_RATE", default_value_t = 3000)]
    api_rate_per_minute: u32,
    #[arg(long, env = "FLUX_GITHUB_CLIENT_ID", requires = "github_client_secret")]
    github_client_id: Option<String>,
    #[arg(long, env = "FLUX_GITHUB_CLIENT_SECRET", hide_env_values = true)]
    github_client_secret: Option<String>,
    #[arg(long, env = "FLUX_GOOGLE_CLIENT_ID", requires = "google_client_secret")]
    google_client_id: Option<String>,
    #[arg(long, env = "FLUX_GOOGLE_CLIENT_SECRET", hide_env_values = true)]
    google_client_secret: Option<String>,
    /// OpenID Connect issuer URL for single sign-on (Okta, Auth0, Entra ID, Keycloak, ...).
    #[arg(long, env = "FLUX_OIDC_ISSUER", requires_all = ["oidc_client_id", "oidc_client_secret"])]
    oidc_issuer: Option<String>,
    #[arg(long, env = "FLUX_OIDC_CLIENT_ID")]
    oidc_client_id: Option<String>,
    #[arg(long, env = "FLUX_OIDC_CLIENT_SECRET", hide_env_values = true)]
    oidc_client_secret: Option<String>,
    /// Sign-in button label for the OIDC provider.
    #[arg(long, env = "FLUX_OIDC_LABEL", default_value = "SSO")]
    oidc_label: String,
    /// Create accounts for first-time external sign-ins (otherwise only
    /// linked accounts and imported placeholders may sign in).
    #[arg(long, env = "FLUX_OAUTH_SIGNUP", default_value_t = true, action = clap::ArgAction::Set)]
    oauth_signup: bool,
}

/// Read or create the secrets key.
fn secret_key(explicit: Option<&PathBuf>, state_dir: Option<&std::path::Path>) -> anyhow::Result<Option<[u8; 32]>> {
    let path = match (explicit, state_dir) {
        (Some(p), _) => p.clone(),
        (None, Some(d)) => d.join("secret.key"),
        (None, None) => return Ok(None),
    };
    if !path.exists() {
        if explicit.is_some() {
            anyhow::bail!("secret key file {} does not exist", path.display());
        }
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).map_err(|e| anyhow::anyhow!("random: {e}"))?;
        std::fs::write(&path, hex::encode(k))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        tracing::info!(path = %path.display(), "generated secrets key");
    }
    let raw = std::fs::read_to_string(&path)?;
    let bytes = hex::decode(raw.trim()).map_err(|_| anyhow::anyhow!("{} must hold 64 hex characters", path.display()))?;
    Ok(Some(bytes.try_into().map_err(|_| anyhow::anyhow!("{} must hold 64 hex characters", path.display()))?))
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (the default).
    Serve,
    /// Issue an access token (creates the user if needed). Printed once.
    Token {
        #[arg(long)]
        user: String,
        /// Make a new user an admin (can create repositories).
        #[arg(long)]
        admin: bool,
        #[arg(long, default_value = "cli")]
        label: String,
    },
    /// Revoke every token of a user.
    Revoke {
        #[arg(long)]
        user: String,
    },
    /// List users.
    Users,
    /// Verify integrity (and repair derived data). Safe to run online.
    Fsck {
        /// Also re-hash every tree and file reachable from refs.
        #[arg(long)]
        full: bool,
    },
    /// Write a consistent online backup to DIR; restore with --data-dir DIR.
    Backup { dir: PathBuf },
    /// Create a repository.
    CreateRepo { name: String },
    /// Show how stored objects are encoded (all-in-one layout only).
    StorageStats,
    /// Train zstd dictionaries for small trees and files from a repository
    /// and use them for new writes (existing objects are unchanged).
    TrainDict {
        repo: String,
        /// Samples per kind.
        #[arg(long, default_value_t = 20_000)]
        samples: usize,
        /// Dictionary size in bytes.
        #[arg(long, default_value_t = flux_store::dict::DICT_SIZE)]
        size: usize,
    },
    /// Run a regional cache tier in front of an upstream flux server: serves
    /// trees and contents from local disk, proxies everything else.
    Cache {
        /// Upstream flux server URL.
        #[arg(long, env = "FLUX_UPSTREAM")]
        upstream: String,
        /// Directory for cached objects.
        #[arg(long, env = "FLUX_CACHE_DIR", default_value = ".flux-cache")]
        dir: PathBuf,
        #[arg(long, env = "FLUX_CACHE_LISTEN", default_value = "127.0.0.1:7879")]
        listen: std::net::SocketAddr,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs go to stderr: stdout carries command output (tokens, reports).
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if let Some(Cmd::Cache { upstream, dir, listen }) = &cli.cmd {
        std::fs::create_dir_all(dir)?;
        let url = url::Url::from_directory_path(dir.canonicalize()?).map_err(|_| anyhow::anyhow!("bad cache dir"))?;
        let cache = flux_server::cache::Cache::new(upstream, url.as_str()).await?;
        let (listener, addr) = flux_server::bind(*listen).await?;
        tracing::info!(%addr, %upstream, dir = %dir.display(), "flux-cache listening");
        return flux_server::cache::serve(cache, listener, shutdown_signal()).await;
    }
    let (cfg, state_dir) = match (cli.storage.data_dir, cli.storage.meta_url, cli.storage.blob_url) {
        (dir, Some(meta_url), Some(blob_url)) => {
            if let Some(d) = &dir {
                std::fs::create_dir_all(d)?;
            }
            (StorageConfig { meta_url, blob_url }, dir)
        }
        (Some(dir), _, _) => (StorageConfig::data_dir(&dir)?, Some(dir)),
        _ => {
            let dir = PathBuf::from(".flux-data");
            (StorageConfig::data_dir(&dir)?, Some(dir))
        }
    };
    let s = &cli.serve;
    let mut oauth = Vec::new();
    if let (Some(id), Some(secret)) = (&s.github_client_id, &s.github_client_secret) {
        oauth.push(flux_server::settings::OAuthProvider::github(id.clone(), secret.clone()));
    }
    if let (Some(id), Some(secret)) = (&s.google_client_id, &s.google_client_secret) {
        oauth.push(flux_server::settings::OAuthProvider::google(id.clone(), secret.clone()));
    }
    if let (Some(iss), Some(id), Some(secret)) = (&s.oidc_issuer, &s.oidc_client_id, &s.oidc_client_secret) {
        let mut p = flux_server::settings::OAuthProvider::oidc(iss, id.clone(), secret.clone());
        p.label = s.oidc_label.clone();
        // Endpoints from the issuer's discovery document.
        let url = format!("{}/.well-known/openid-configuration", iss.trim_end_matches('/'));
        match reqwest::Client::new().get(&url).timeout(std::time::Duration::from_secs(10)).send().await {
            Ok(r) if r.status().is_success() => {
                let d: serde_json::Value = r.json().await?;
                let field = |k: &str| d.get(k).and_then(|v| v.as_str()).map(str::to_string);
                if let (Some(a), Some(t), Some(u)) =
                    (field("authorization_endpoint"), field("token_endpoint"), field("userinfo_endpoint"))
                {
                    (p.auth_url, p.token_url, p.userinfo_url) = (a, t, u);
                }
            }
            other => {
                tracing::warn!(%url, status = ?other.map(|r| r.status()), "OIDC discovery failed; using default endpoint paths")
            }
        }
        oauth.push(p);
    }
    for p in &mut oauth {
        p.allow_signup = s.oauth_signup;
    }
    let ssh_host = s.ssh_host.clone().or_else(|| {
        let port = s.ssh_listen?.port();
        let host = s.public_url.as_deref().and_then(|u| url::Url::parse(u).ok()?.host_str().map(str::to_string))?;
        Some(if port == 22 { host } else { format!("{host}:{port}") })
    });
    let settings = flux_server::settings::Settings {
        public_url: s.public_url.clone(),
        webhook_allow_private: s.webhook_allow_private,
        secret_key: secret_key(s.secret_key_file.as_ref(), state_dir.as_deref())?,
        smtp: s
            .smtp_url
            .clone()
            .map(|url| flux_server::settings::SmtpConfig { url, from: s.smtp_from.clone().unwrap_or_default() }),
        oauth,
        ssh_host,
        require_2fa: s.require_2fa,
        listen_url: Some(format!("http://{}", s.listen)),
        api_rate_per_minute: s.api_rate_per_minute,
    };
    let flux = flux_server::open_with_settings(&cfg, s.auth, s.signup, settings).await?;
    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => {}
        Cmd::Token { user, admin, label } => {
            let (u, token) = flux.issue_token(&user, admin, &label).await?;
            eprintln!("token for {}{} (shown once; store it safely):", u.name, if u.is_admin { " (admin)" } else { "" });
            println!("{token}");
            return Ok(());
        }
        Cmd::Revoke { user } => {
            let n = flux.revoke_tokens(&user).await?;
            println!("revoked {n} token(s) for {user}");
            return Ok(());
        }
        Cmd::Users => {
            for u in flux.meta.list_users().await? {
                println!("{}{}", u.name, if u.is_admin { "  (admin)" } else { "" });
            }
            return Ok(());
        }
        Cmd::Fsck { full } => {
            let started = std::time::Instant::now();
            let r = flux.fsck(full, |m| eprintln!("{m}")).await?;
            println!(
                "checked {} repos, {} changesets, {} refs, {} trees, {} files in {:.1}s; repaired {}",
                r.repos,
                r.changesets,
                r.refs,
                r.trees,
                r.contents,
                started.elapsed().as_secs_f64(),
                r.repaired
            );
            for p in &r.problems {
                eprintln!("PROBLEM: {p}");
            }
            if !r.ok() {
                anyhow::bail!("{} problem(s) found", r.problems.len());
            }
            println!("ok");
            return Ok(());
        }
        Cmd::StorageStats => {
            let stats = flux.blobs.codec_stats().await?;
            let mib = |b: u64| b as f64 / (1 << 20) as f64;
            println!("{:<10} {:<10} {:>10} {:>12} {:>12} {:>7}", "kind", "codec", "objects", "stored MiB", "raw MiB", "ratio");
            let (mut n, mut st, mut raw) = (0, 0, 0);
            for ((kind, codec), (objects, stored, decoded)) in &stats {
                println!(
                    "{kind:<10} {codec:<10} {objects:>10} {:>12.1} {:>12.1} {:>6.1}x",
                    mib(*stored),
                    mib(*decoded),
                    *decoded as f64 / (*stored).max(1) as f64
                );
                (n, st, raw) = (n + objects, st + stored, raw + decoded);
            }
            println!(
                "{:<10} {:<10} {n:>10} {:>12.1} {:>12.1} {:>6.1}x",
                "total",
                "",
                mib(st),
                mib(raw),
                raw as f64 / st.max(1) as f64
            );
            return Ok(());
        }
        Cmd::TrainDict { repo, samples, size } => {
            let id = flux.repo(&repo).await?;
            let reports = flux.train_dicts(id, samples, size).await?;
            if reports.is_empty() {
                println!("not enough small objects in {repo} to train a dictionary");
            }
            for r in reports {
                println!(
                    "{:?} dictionary #{} from {} samples: held-out objects {:.1} KiB -> {:.1} KiB ({:.0}% smaller than plain zstd)",
                    r.kind,
                    r.id,
                    r.samples,
                    r.plain_bytes as f64 / 1024.0,
                    r.dict_bytes as f64 / 1024.0,
                    100.0 * (1.0 - r.dict_bytes as f64 / r.plain_bytes.max(1) as f64)
                );
            }
            return Ok(());
        }
        Cmd::Backup { dir } => {
            let started = std::time::Instant::now();
            let (objects, bytes) = flux.backup(&dir).await?;
            println!(
                "backed up metadata and {objects} objects ({:.1} MiB) to {} in {:.1}s",
                bytes as f64 / (1 << 20) as f64,
                dir.display(),
                started.elapsed().as_secs_f64()
            );
            return Ok(());
        }
        Cmd::CreateRepo { name } => {
            flux.create_repo(&name).await?;
            println!("created {name}");
            return Ok(());
        }
        Cmd::Cache { .. } => unreachable!("handled above"),
    }
    for name in &cli.serve.create_repos {
        if flux.meta.get_repo(name).await?.is_none() {
            flux.create_repo(name).await?;
            tracing::info!(repo = %name, "created repository");
        }
    }
    flux_server::warm(&flux);
    if let Some(ssh) = cli.serve.ssh_listen {
        let key_path = cli
            .serve
            .ssh_host_key
            .clone()
            .or_else(|| state_dir.as_ref().map(|d| d.join("ssh_host_ed25519_key")))
            .ok_or_else(|| anyhow::anyhow!("--ssh-host-key is required with --meta-url/--blob-url"))?;
        let key = flux_server::git::ssh::host_key(&key_path)?;
        let listener = tokio::net::TcpListener::bind(ssh).await?;
        tracing::info!(addr = %ssh, "git over SSH listening");
        let flux = flux.clone();
        tokio::spawn(async move {
            if let Err(e) = flux_server::git::ssh::serve(flux, listener, key, shutdown_signal()).await {
                tracing::error!(error = %e, "SSH server stopped");
            }
        });
    }
    let (listener, addr) = flux_server::bind(cli.serve.listen).await?;
    tracing::info!(
        %addr,
        auth = cli.serve.auth.as_str(),
        web = !cli.serve.no_web,
        meta = %cfg.meta_url,
        blobs = %cfg.blob_url,
        "flux-server listening (gRPC{} on http://{addr})",
        if cli.serve.no_web { "" } else { " + web UI" }
    );
    flux_server::serve_with(flux, listener, flux_server::ServeOptions { web: !cli.serve.no_web }, shutdown_signal()).await
}

/// Graceful shutdown on Ctrl-C or SIGTERM: stop accepting, drain requests.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutting down: draining in-flight requests");
}
