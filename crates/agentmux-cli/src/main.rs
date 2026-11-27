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
use agentmux::run::{AutoResumePolicy, RunStore, StartRequest};
use clap::Parser as _;
use color_eyre::eyre::{Result, WrapErr as _, bail};

use crate::cli::{Cli, CodexLaunchArg, Command, DelegateArgs, McpArgs, QuestionArgs};

/// musl's allocator serialises enough under the multi-threaded runtime to show up in wall-clock
/// time, and the Linux release is built against musl so that it runs on any host's glibc.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The consultation `ask` and `start` begin.
///
/// `--codex-launch exec` needs no field here: a store that drives nothing records `exec` on every
/// consultation it starts.
fn start_request(delegate: &DelegateArgs, question: &QuestionArgs) -> Result<StartRequest> {
    Ok(StartRequest {
        delegate: delegate.build()?,
        question: question.text()?,
        cwd: question.working_dir()?,
        retention: question.retention(),
        env: question.environment()?,
        billing: delegate.billing,
        force_codex_exec: false,
        label: question.label.clone(),
    })
}

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

    // The driver is a delegate, not a front end: it opens no store and serves nobody, and its
    // exit status is what the store reads when the turn's stream says nothing.
    if let Command::CodexAppServer(args) = &cli.command {
        std::process::exit(agentmux::appserver::drive(&args.plan_path));
    }
    // So is the supervisor: the store it opens is the one its plan names, and the environment it
    // was started with is its delegate's, which it hands on.
    if let Command::Supervise(args) = &cli.command {
        std::process::exit(agentmux::run::supervise(&args.plan_path, agentmux::host_env()).await);
    }

    let store = open_store(&cli)?;
    match cli.command {
        Command::Accounts => {
            // Resolved from the current directory, so the answer is the one a consultation started
            // here would actually get.
            let cwd = std::env::current_dir().wrap_err("locating the working directory")?;
            let config = agentmux::config::Config::load(store.host_env(), &cwd)?;
            report::accounts(&config, store.host_env(), cli.json)
        }
        Command::Quota(args) => report::quota(&store.quota(args.delegate)?, cli.json),
        Command::Mcp(args) => serve(store, &args).await,
        Command::Ask(args) => {
            let status = store.start(&start_request(&args.delegate, &args.question)?)?;
            store
                .wait_until_terminal(&status.run_id, Duration::from_secs(args.wait))
                .await?;
            report::answer(&store, &status.run_id, cli.json)
        }
        Command::Start(args) => {
            let status = store.start(&start_request(&args.delegate, &args.question)?)?;
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
            let status = store.follow_up_with(
                args.run_id.id(),
                &args.text()?,
                args.permissions.options()?,
                args.label.as_ref(),
            )?;
            store
                .wait_until_terminal(&status.run_id, Duration::from_secs(args.wait))
                .await?;
            report::answer(&store, &status.run_id, cli.json)
        }
        Command::ContinueFresh(args) => {
            let status = store.continue_fresh(args.run_id.id(), args.text()?.as_deref())?;
            report::status(&status, cli.json)
        }
        Command::Steer(args) => {
            let status = store.steer(args.run_id.id(), &args.text()?).await?;
            if !cli.json {
                println!(
                    "steered: the delegate accepted the message into its running turn; the \
                     transcript records it as `### user (steer)`"
                );
            }
            report::status(&status, cli.json)
        }
        Command::Wait(args) => {
            let waited = report::wait(&store, &args, cli.json).await?;
            if waited != report::WaitStatus::Completed {
                // Exiting skips the flush a return would make.
                std::io::Write::flush(&mut std::io::stdout())?;
                std::process::exit(waited.code());
            }
            Ok(())
        }
        Command::Cancel(args) => report::status(&store.cancel(args.run_id.id()).await?, cli.json),
        Command::List(args) => {
            let filter = args.filter();
            let runs = store.list(args.limit, &filter)?;
            report::list(&runs, filter.filters(), cli.json)
        }
        // Dispatched before the store was opened.
        Command::CodexAppServer(_) | Command::Supervise(_) => Ok(()),
        Command::Prune(args) => {
            if let Some(run_id) = args.run_id {
                store.remove(run_id.id())?;
                println!("removed {}", run_id.id());
            } else {
                println!("removed {} expired consultation(s)", sweep(&store)?);
            }
            Ok(())
        }
    }
}

/// The store every front-end command works on, as the global arguments configure it.
///
/// # Errors
///
/// Returns an error when the state directory or this executable cannot be located, or the store
/// cannot be opened.
fn open_store(cli: &Cli) -> Result<RunStore> {
    let host_env = agentmux::host_env();
    let root = match &cli.state_dir {
        Some(dir) => dir.clone(),
        None => RunStore::default_root(&host_env).wrap_err("locating the state directory")?,
    };
    // This executable is the supervisor of every turn and the driver of a driven one, so a turn
    // it starts is run and settled by the same build that will read it back.
    // The store refuses to launch once an upgrade replaces the file under a running server.
    let executable = std::env::current_exe().wrap_err("locating the agentmux executable")?;
    let mut store = RunStore::open(root, Arc::new(ProcessLauncher::new()), host_env)?
        .with_quota_probe(Arc::new(agentmux::quota::SystemProbe))
        .with_supervisor(&executable);
    if let Some(first_delay) = cli.auto_resume_delay {
        store = store.with_auto_resume(Some(AutoResumePolicy {
            first_delay,
            ..AutoResumePolicy::DEFAULT
        }));
    }
    if cli.codex_launch == CodexLaunchArg::AppServer {
        store = store.with_codex_app_server(executable);
    }
    Ok(store)
}

/// Delete the consultations past the machine's retention window.
///
/// Only the machine file is read, because it alone may set the window and the store is shared by
/// every project.
/// A machine file that does not load fails the sweep rather than falling back to the default
/// window, which could delete what a longer configured one keeps.
///
/// # Errors
///
/// Returns an error when the machine configuration cannot be read, or the run directory cannot be
/// listed.
fn sweep(store: &RunStore) -> Result<usize> {
    let config = agentmux::config::Config::load_machine(store.host_env())?;
    Ok(store.sweep(config.sweep_after())?)
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
    let denied: BTreeSet<Vendor> = args.deny.iter().copied().collect();
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
             serve consultations to it. A delegate that loads its account's configuration also \
             loads the MCP servers it names, agentmux among them; leave agentmux out of that \
             configuration, or launch the delegate with ignore_user_config set to true."
        );
    }
    // The sweep runs once at server start.
    // No timer, no daemon.
    match sweep(&store) {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "swept expired consultations"),
        Err(error) => tracing::warn!(%error, "sweep failed; serving anyway"),
    }
    agentmux_mcp::serve_stdio(store).await?;
    Ok(())
}
