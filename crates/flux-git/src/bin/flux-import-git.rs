use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use flux_client::Client;
use flux_git::{ImportOptions, git};

/// Import git history into a flux repository, or verify a previous import.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// flux server URL.
    #[arg(long, env = "FLUX_SERVER", default_value = "http://127.0.0.1:7878")]
    server: String,
    /// flux repository name.
    #[arg(long, env = "FLUX_REPO")]
    repo: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import (or incrementally update) from a local git repository.
    Import {
        /// Path to the git repository (working tree or bare).
        git_dir: PathBuf,
        /// Ref patterns to import (default: refs/heads/).
        #[arg(long = "ref")]
        refs: Vec<String>,
        /// Create the flux repository if it doesn't exist.
        #[arg(long)]
        create: bool,
        #[arg(long, default_value_t = 512)]
        batch: usize,
    },
    /// Check flux's file set against `git ls-tree` for sampled commits.
    Verify {
        git_dir: PathBuf,
        /// Number of commits to check, evenly spaced (0 = all).
        #[arg(long, default_value_t = 200)]
        sample: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let client = Client::connect(&args.server, &args.repo).await?;
    match args.cmd {
        Cmd::Import { git_dir, refs, create, batch } => {
            if create && !client.list_repos().await?.contains(&args.repo) {
                client.create_repo(&args.repo).await?;
            }
            let mut opts = ImportOptions::new(git_dir);
            opts.ref_patterns = refs;
            opts.batch = batch;
            let stats = flux_git::import(&client, &opts).await?;
            let secs = stats.elapsed_ms as f64 / 1000.0;
            println!(
                "imported {} commits ({} already present) in {secs:.1}s ({:.0} commits/s); {} blobs, {:.1} MiB; {} refs",
                stats.commits_imported,
                stats.commits_skipped,
                stats.commits_imported as f64 / secs.max(0.001),
                stats.blobs_uploaded,
                stats.blob_bytes as f64 / (1 << 20) as f64,
                stats.refs_updated,
            );
        }
        Cmd::Verify { git_dir, sample } => {
            let tips: Vec<_> = git::list_refs(&git_dir, &[])?.into_iter().map(|r| r.1).collect();
            let all: Vec<_> = git::rev_list(&git_dir, &tips)?.into_iter().map(|c| c.0).collect();
            let picked: Vec<_> = if sample == 0 || sample >= all.len() {
                all
            } else {
                let step = all.len() as f64 / sample as f64;
                (0..sample).map(|i| all[(i as f64 * step) as usize]).collect()
            };
            let bad = flux_git::verify(&client, &git_dir, &picked).await?;
            if bad.is_empty() {
                println!("verified {} commits: all file sets match git", picked.len());
            } else {
                for m in &bad {
                    eprintln!("MISMATCH {}: {}", m.commit, m.detail);
                }
                anyhow::bail!("{} of {} commits mismatched", bad.len(), picked.len());
            }
        }
    }
    Ok(())
}
