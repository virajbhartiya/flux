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
    /// Directory for the all-in-one SQLite + segment-file layout.
    #[arg(long, env = "FLUX_DATA_DIR", global = true, conflicts_with_all = ["meta_url", "blob_url"])]
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
    /// Disable the web UI (gRPC only).
    #[arg(long)]
    no_web: bool,
    /// Create these repositories at startup if they don't exist.
    #[arg(long = "create-repo")]
    create_repos: Vec<String>,
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let cfg = match (cli.storage.data_dir, cli.storage.meta_url, cli.storage.blob_url) {
        (Some(dir), _, _) => StorageConfig::data_dir(&dir)?,
        (None, Some(meta_url), Some(blob_url)) => StorageConfig { meta_url, blob_url },
        _ => StorageConfig::data_dir(&PathBuf::from(".flux-data"))?,
    };
    let flux = flux_server::open_with(&cfg, cli.serve.auth).await?;
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
    }
    for name in &cli.serve.create_repos {
        if flux.meta.get_repo(name).await?.is_none() {
            flux.create_repo(name).await?;
            tracing::info!(repo = %name, "created repository");
        }
    }
    flux_server::warm(&flux);
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
