//! Composition root: parse arguments, sweep, and either run one command or serve stdio.
//!
//! Changes when wiring changes.

mod cli;
mod report;

use std::sync::Arc;
use std::time::Duration;

use agentmux::launch::ProcessLauncher;
use agentmux::run::{RunStore, StartRequest};
use clap::Parser as _;
use color_eyre::eyre::{Result, WrapErr as _};

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    // stdout is the JSON-RPC channel under `agentmux mcp`; anything else on it corrupts the
    // protocol.
    // Logging goes to stderr for every subcommand, so the two never diverge.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AGENTMUX_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let host_env = agentmux::host_env();
    let root = match &cli.state_dir {
        Some(dir) => dir.clone(),
        None => RunStore::default_root(&host_env).wrap_err("locating the state directory")?,
    };
    let store = RunStore::open(root, Arc::new(ProcessLauncher), host_env)?;

    match cli.command {
        Command::Mcp => {
            // The sweep runs once at server start.
            // No timer, no daemon.
            match store.sweep() {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "swept expired consultations"),
                Err(error) => tracing::warn!(%error, "sweep failed; serving anyway"),
            }
            agentmux_mcp::serve_stdio(store).await?;
            Ok(())
        }
        Command::Ask(args) => {
            let status = store.start(&StartRequest {
                delegate: args.delegate.build()?,
                question: args.question.text()?,
                cwd: args.question.working_dir()?,
                retention: args.question.retention(),
            })?;
            let status = store
                .wait_until_terminal(&status.run_id, Duration::from_secs(args.wait))
                .await?;
            report::answer(&store, &status, cli.json)
        }
        Command::Start(args) => {
            let status = store.start(&StartRequest {
                delegate: args.delegate.build()?,
                question: args.question.text()?,
                cwd: args.question.working_dir()?,
                retention: args.question.retention(),
            })?;
            report::status(&status, cli.json)
        }
        Command::Status(args) => report::status(&store.status(&args.run_id.0)?, cli.json),
        Command::Tail(args) => report::tail(&store, &args, cli.json).await,
        Command::Result(args) => {
            let status = if args.wait > 0 {
                store
                    .wait_until_terminal(&args.run_id.0, Duration::from_secs(args.wait))
                    .await?
            } else {
                store.status(&args.run_id.0)?
            };
            let page = store.read_transcript(&args.run_id.0, args.offset, args.max_bytes)?;
            report::transcript(&status, &page, cli.json)
        }
        Command::FollowUp(args) => {
            let status = store.follow_up(&args.run_id.0, &args.text()?)?;
            let status = store
                .wait_until_terminal(&status.run_id, Duration::from_secs(args.wait))
                .await?;
            report::answer(&store, &status, cli.json)
        }
        Command::Cancel(args) => report::status(&store.cancel(&args.run_id.0)?, cli.json),
        Command::List(args) => report::list(&store.list(args.limit)?, cli.json),
        Command::Prune(args) => {
            if let Some(id) = args.run_id {
                store.remove(&id.0)?;
                println!("removed {}", id.0);
            } else {
                println!("removed {} expired consultation(s)", store.sweep()?);
            }
            Ok(())
        }
    }
}
