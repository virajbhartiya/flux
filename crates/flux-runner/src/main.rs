//! `flux-runner`: the CI job runner for flux.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "flux-runner", version, about = "Runs jobs for flux's built-in CI")]
struct Cli {
    /// The flux server's URL.
    #[arg(long, env = "FLUX_SERVER")]
    server: String,
    /// Runner token (`flxr_…`), from `POST /api/v1/runners`.
    #[arg(long, env = "FLUX_RUNNER_TOKEN", hide_env_values = true)]
    token: String,
    /// Extra labels (comma separated), added to the defaults.
    #[arg(long, value_delimiter = ',')]
    labels: Vec<String>,
    /// Use only `--labels`, without `self-hosted`, OS and architecture.
    #[arg(long)]
    no_default_labels: bool,
    /// Where job directories are created (default: a temp directory).
    #[arg(long, env = "FLUX_RUNNER_WORK_DIR")]
    work_dir: Option<PathBuf>,
    /// Jobs to run in parallel.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=256))]
    concurrency: u16,
    /// Exit when no job is waiting instead of polling forever.
    #[arg(long)]
    once: bool,
    /// Seconds to long-poll for a job (the server caps this at 55).
    #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u64).range(1..=55))]
    wait: u64,
    /// Name shown in job logs (default: the host name).
    #[arg(long)]
    name: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let cli = Cli::parse();
    let token = cli.token.trim().to_string();
    if !token.starts_with("flxr_") {
        anyhow::bail!("--token must be a runner token (flxr_…); create one with POST /api/v1/runners");
    }
    let mut labels = if cli.no_default_labels { Vec::new() } else { flux_runner::default_labels() };
    for l in cli.labels.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        if !labels.iter().any(|x| x == l) {
            labels.push(l.to_string());
        }
    }
    if labels.is_empty() {
        anyhow::bail!("the runner needs at least one label");
    }
    let cfg = flux_runner::Config {
        server: cli.server,
        token,
        labels,
        work_dir: cli.work_dir.unwrap_or_else(|| std::env::temp_dir().join("flux-runner")),
        concurrency: usize::from(cli.concurrency),
        once: cli.once,
        wait: cli.wait,
        name: cli.name.unwrap_or_else(flux_runner::default_name),
    };
    flux_runner::run(cfg).await
}
