//! Composition root: parse arguments, sweep, and either run one command or serve stdio.
//!
//! Changes when wiring changes.

mod cli;
mod report;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use agentmux::delegate::Vendor;
use agentmux::launch::ProcessLauncher;
use agentmux::run::{RunStore, StartRequest};
use clap::Parser as _;
use color_eyre::eyre::{Result, WrapErr as _, bail};

use crate::cli::{Cli, Command, McpArgs};

/// musl's allocator serialises enough under the multi-threaded runtime to show up in wall-clock
/// time, and the Linux release is built against musl so that it runs on any host's glibc.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
    let store = RunStore::open(root, Arc::new(ProcessLauncher::new()), host_env)?
        .with_quota_probe(Arc::new(agentmux::quota::SystemProbe));

    match cli.command {
        Command::Accounts => {
            // Resolved from the current directory, so the answer is the one a consultation started
            // here would actually get.
            let cwd = std::env::current_dir().wrap_err("locating the working directory")?;
            let config = agentmux::config::Config::load(store.host_env(), &cwd)?;
            report::accounts(&config, store.host_env(), cli.json)
        }
        Command::Quota(args) => {
            report::quota(&store.quota(args.delegate.map(Into::into))?, cli.json)
        }
        Command::Mcp(args) => serve(store, &args).await,
        Command::Ask(args) => {
            let status = store.start(&StartRequest {
                delegate: args.delegate.build()?,
                question: args.question.text()?,
                cwd: args.question.working_dir()?,
                retention: args.question.retention(),
                env: args.question.environment()?,
            })?;
            store
                .wait_until_terminal(&status.run_id, Duration::from_secs(args.wait))
                .await?;
            report::answer(&store, &status.run_id, cli.json)
        }
        Command::Start(args) => {
            let status = store.start(&StartRequest {
                delegate: args.delegate.build()?,
                question: args.question.text()?,
                cwd: args.question.working_dir()?,
                retention: args.question.retention(),
                env: args.question.environment()?,
            })?;
            report::status(&status, cli.json)
        }
        Command::Status(args) => report::status(&store.status(args.run_id.id())?, cli.json),
        Command::Tail(args) => report::tail(&store, &args, cli.json).await,
        Command::Result(args) => {
            if args.wait > 0 {
                store
                    .wait_until_terminal(args.run_id.id(), Duration::from_secs(args.wait))
                    .await?;
            }
            let (status, page) = store.view(args.run_id.id(), args.offset, args.max_bytes)?;
            report::transcript(&status, &page, cli.json)
        }
        Command::FollowUp(args) => {
            let status = store.follow_up(args.run_id.id(), &args.text()?)?;
            store
                .wait_until_terminal(&status.run_id, Duration::from_secs(args.wait))
                .await?;
            report::answer(&store, &status.run_id, cli.json)
        }
        Command::Cancel(args) => report::status(&store.cancel(args.run_id.id()).await?, cli.json),
        Command::List(args) => report::list(&store.list(args.limit)?, cli.json),
        Command::Prune(args) => {
            if let Some(run_id) = args.run_id {
                store.remove(run_id.id())?;
                println!("removed {}", run_id.id());
            } else {
                println!("removed {} expired consultation(s)", store.sweep()?);
            }
            Ok(())
        }
    }
}

/// Serve the tools over stdio until the host disconnects.
///
/// Everything decided here is decided once, before the first tool call: which vendors this server
/// will launch, whether it may serve at all, and what expired while it was not running.
///
/// # Errors
///
/// Returns an error when `--deny` leaves no vendor to consult, when this process is itself a
/// delegate, or when the host does not complete the handshake.
async fn serve(store: RunStore, args: &McpArgs) -> Result<()> {
    let denied: BTreeSet<Vendor> = args.deny.iter().copied().map(Vendor::from).collect();
    // Serving with every vendor denied would refuse every call it accepted.
    // A host surfaces a server that will not start, where it would quietly swallow one that
    // answers nothing.
    if Vendor::ALL.iter().all(|vendor| denied.contains(vendor)) {
        bail!(
            "--deny names every vendor, which leaves nothing to consult. Deny the vendor of the \
             harness this server is registered in — `--deny claude` under Claude Code, `--deny \
             codex` under Codex — and leave the other."
        );
    }
    let store = store.with_denied_vendors(denied);
    // A server started by a delegate would hand that delegate the means to start another.
    // Refusing to serve at all is simpler than refusing tool by tool, and the host reports a
    // server that would not start.
    if store.running_inside_a_delegate()? {
        bail!(
            "this agentmux was started inside a delegate that agentmux launched, so it will not \
             serve consultations to it. A delegate that inherits its account's MCP servers also \
             inherits agentmux; leave it out of that account's configuration, or run the account \
             isolated."
        );
    }
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
