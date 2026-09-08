//! The command-line surface.
//!
//! Changes when the CLI's arguments change.
//!
//! The same nine verbs the MCP server exposes, plus `mcp` to serve them, `accounts` to show what
//! this machine defines, and `prune` to remove consultations by hand.
//! A human debugging a delegation and an agent calling the tool are exercising exactly the same
//! code path, which is the point: if `agentmux ask` works from a terminal, the tool works.

use std::path::PathBuf;

use std::collections::BTreeMap;

use agentmux::delegate::{CodexSandbox, Delegate, Isolation, Vendor};
use agentmux::run::{Retention, RunId};
use clap::{Args, Parser, Subcommand, ValueEnum};
use color_eyre::eyre::{Result, WrapErr as _, bail};

/// Delegate a question to another vendor's coding agent and keep the whole transcript.
#[derive(Debug, Parser)]
#[command(name = "agentmux", version, about, long_about = None)]
pub struct Cli {
    /// Where consultations are stored.
    /// Defaults to the platform state directory.
    #[arg(long, global = true, env = agentmux::run::RunStore::STATE_DIR_ENV)]
    pub state_dir: Option<PathBuf>,

    /// Print machine-readable JSON instead of prose.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// What to do.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Ask a delegate a question and wait for the answer.
    ///
    /// Starts the consultation and blocks until it finishes or `--wait` elapses, whichever comes
    /// first.
    /// Waiting too long is never fatal: the consultation keeps running and its id is printed so it
    /// can be collected later.
    Ask(AskArgs),

    /// Begin a consultation and return immediately.
    Start(StartArgs),

    /// Report how a consultation is doing.
    Status(RunArgs),

    /// Print the transcript written since a cursor, then the next cursor.
    Tail(TailArgs),

    /// Print the whole transcript.
    Result(ResultArgs),

    /// Ask a finished consultation one more question, in the delegate's own session.
    #[command(name = "follow-up", alias = "followup")]
    FollowUp(FollowUpArgs),

    /// Stop a running consultation, keeping everything collected so far.
    Cancel(RunArgs),

    /// List recent consultations.
    List(ListArgs),

    /// Delete consultations past their retention, or one by id.
    Prune(PruneArgs),

    /// List the account aliases and model rewrites this machine defines, and where they came
    /// from.
    Accounts,

    /// Report what each configured account has left of its usage windows.
    Quota(QuotaArgs),

    /// Serve the tools over stdio, for an MCP host to launch.
    Mcp(McpArgs),
}

/// How the server is served.
#[derive(Debug, Args)]
pub struct McpArgs {
    /// Refuse to launch this vendor's delegates.
    ///
    /// Repeatable.
    /// Name the vendor of the harness this server is registered in — `--deny claude` under Claude
    /// Code, `--deny codex` under Codex.
    /// That harness spawns its own same-vendor subagents natively and supervises them itself, so
    /// a consultation agentmux runs for it is a slower, blinder copy of something it already has;
    /// agentmux is worth paying for across the vendor line, not along it.
    /// Denying both leaves nothing to consult and is refused.
    #[arg(long = "deny", value_enum, value_name = "VENDOR")]
    pub deny: Vec<VendorArg>,
}

/// Which delegate to consult, and on what terms.
#[derive(Debug, Args)]
pub struct DelegateArgs {
    /// Which CLI to consult: `claude` reaches Anthropic models, `codex` reaches GPT models.
    #[arg(long, value_enum)]
    pub delegate: VendorArg,

    /// Model identifier, passed to the delegate CLI verbatim, for example `claude-opus-5` or
    /// `gpt-6-astra`.
    /// agentmux keeps no roster; the delegate CLI decides what is valid.
    /// A `[models]` rule in `agentmux.toml` may rewrite one identifier to another, which
    /// `agentmux accounts` lists and every report of the consultation names.
    #[arg(long)]
    pub model: String,

    /// Reasoning effort, passed verbatim, for example `xhigh`.
    /// Always pinned explicitly rather than inherited from the delegate's configured default.
    #[arg(long)]
    pub effort: String,

    /// Which configured account to authenticate as, for either vendor.
    ///
    /// Aliases come from `agentmux.toml`; run `agentmux accounts` to see the ones this machine
    /// defines.
    /// Omit it to use the account `agentmux.toml` names as the default, or the CLI's own login
    /// when it names none.
    #[arg(long)]
    pub account: Option<String>,

    /// Run the delegate with the account's own settings, hooks and MCP servers.
    ///
    /// Off by default: a consultation is a second opinion, not a second copy of your setup.
    /// Use it when the point is to exercise the tooling that configuration sets up, and accept
    /// that a hook can then reopen the delegate's finished turn.
    #[arg(long, conflicts_with = "isolated")]
    pub inherit_settings: bool,

    /// Load none of the account's settings, hooks or MCP servers.
    ///
    /// This is what happens with neither flag, unless the account sets `inherit_settings = true`.
    /// Pass this to override that for one run, when the answer must not depend on this machine.
    /// `agentmux accounts` marks which accounts inherit.
    #[arg(long, conflicts_with = "inherit_settings")]
    pub isolated: bool,

    /// How much of the filesystem the delegate may write, for `--delegate codex`.
    /// Defaults to `read-only`.
    #[arg(long, value_enum)]
    pub sandbox: Option<SandboxArg>,
}

/// Which CLI to consult.
///
/// A copy of [`Vendor`] that clap can list in `--help`; the core enum stays free of clap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum VendorArg {
    /// Anthropic's `claude`.
    ///
    /// Aliased to `claude-code` because the CLI and the harness that runs it are spelled
    /// differently, and `--deny` is written while thinking of the harness.
    #[value(alias = "claude-code")]
    Claude,
    /// The `codex` CLI from `OpenAI`.
    Codex,
}

impl From<VendorArg> for Vendor {
    fn from(vendor: VendorArg) -> Self {
        match vendor {
            VendorArg::Claude => Self::Claude,
            VendorArg::Codex => Self::Codex,
        }
    }
}

/// Which vendor's accounts to ask about.
#[derive(Debug, Args)]
pub struct QuotaArgs {
    /// Ask only this vendor.
    /// Both are asked when omitted.
    #[arg(long, value_enum)]
    pub delegate: Option<VendorArg>,
}

/// How much of the filesystem a Codex delegate may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SandboxArg {
    /// Read the working directory, write nothing.
    ReadOnly,
    /// Also write inside the working directory.
    WorkspaceWrite,
}

impl From<SandboxArg> for CodexSandbox {
    fn from(sandbox: SandboxArg) -> Self {
        match sandbox {
            SandboxArg::ReadOnly => Self::ReadOnly,
            SandboxArg::WorkspaceWrite => Self::WorkspaceWrite,
        }
    }
}

impl DelegateArgs {
    /// The isolation asked for on the command line, or `None` to let the account decide.
    fn isolation(&self) -> Option<Isolation> {
        match (self.inherit_settings, self.isolated) {
            (true, _) => Some(Isolation::Inherit),
            (_, true) => Some(Isolation::Isolated),
            _ => None,
        }
    }

    /// Build the delegate, rejecting an option that belongs to the other vendor.
    ///
    /// Existence of the alias is not checked here: which aliases are defined depends on the
    /// working directory the consultation will run in, which `build` does not know.
    ///
    /// # Errors
    ///
    /// Returns an error when the model, effort or alias is not argv-safe, or when a
    /// vendor-specific option was set for the wrong vendor.
    pub fn build(&self) -> Result<Delegate> {
        Delegate::from_parts(
            self.delegate.into(),
            &self.model,
            &self.effort,
            self.account.as_deref(),
            self.isolation(),
            self.sandbox.map(CodexSandbox::from),
        )
        .wrap_err("invalid delegate arguments")
    }
}

/// Where the question comes from, and where the delegate reads the project.
#[derive(Debug, Args)]
pub struct QuestionArgs {
    /// The question.
    /// Omit it to read the question from stdin, which is what a long brief wants.
    pub question: Option<String>,

    /// Read the question from a file instead.
    #[arg(long, short = 'f', conflicts_with = "question")]
    pub file: Option<PathBuf>,

    /// The directory the delegate reads the project from.
    /// Defaults to the current directory.
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,

    /// Extra environment for the delegate, as a repeatable `KEY=VALUE` pair.
    ///
    /// Only useful with `--inherit-settings`: an isolated delegate loads no settings, hooks or
    /// MCP servers, so nothing in it reads these.
    /// Standing settings belong in `agentmux.toml`, which applies them to every launch.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Keep the consultation indefinitely so a follow-up can arrive at any time.
    ///
    /// Without this it is swept 24 hours after it started.
    #[arg(long)]
    pub keep: bool,
}

impl QuestionArgs {
    /// Read the question from wherever it was supplied.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, stdin cannot be read, or the question is
    /// empty — a delegate given an empty brief burns money to say nothing.
    pub fn text(&self) -> Result<String> {
        let text = match (&self.question, &self.file) {
            (Some(text), _) => text.clone(),
            (None, Some(path)) => std::fs::read_to_string(path)
                .wrap_err_with(|| format!("reading the question from {}", path.display()))?,
            (None, None) => {
                use std::io::Read as _;
                let mut buffer = String::new();
                std::io::stdin()
                    .read_to_string(&mut buffer)
                    .wrap_err("reading the question from stdin")?;
                buffer
            }
        };
        if text.trim().is_empty() {
            bail!(
                "the question is empty. Pass it as an argument, with --file, or on stdin. The \
                 delegate starts with no context, so the question has to be self-contained."
            );
        }
        Ok(text)
    }

    /// The directory the delegate reads from, made absolute.
    ///
    /// Absolute because the consultation outlives this process: a follow-up from another
    /// directory must read the same checkout, and a path recorded relative to this one would
    /// silently resolve elsewhere.
    ///
    /// # Errors
    ///
    /// Returns an error when the current directory cannot be determined, or `--cwd` is not a
    /// directory.
    pub fn working_dir(&self) -> Result<PathBuf> {
        let dir = match &self.cwd {
            Some(dir) => std::path::absolute(dir)
                .wrap_err_with(|| format!("resolving --cwd {}", dir.display()))?,
            None => std::env::current_dir().wrap_err("determining the working directory")?,
        };
        if !dir.is_dir() {
            bail!(
                "--cwd {} is not a directory. Pass the path of the checkout the delegate should \
                 read.",
                dir.display()
            );
        }
        Ok(dir)
    }

    /// The extra environment, parsed from `KEY=VALUE` pairs.
    ///
    /// Split on the first `=` only, because a value may legitimately contain one.
    ///
    /// # Errors
    ///
    /// Returns an error when a pair has no `=`, or an empty name.
    pub fn environment(&self) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        for pair in &self.env {
            let Some((name, value)) = pair.split_once('=') else {
                bail!("--env expects KEY=VALUE, got {pair:?}");
            };
            if name.is_empty() {
                bail!("--env has an empty variable name in {pair:?}");
            }
            env.insert(name.to_owned(), value.to_owned());
        }
        Ok(env)
    }

    /// How long the consultation is kept.
    #[must_use]
    pub fn retention(&self) -> Retention {
        if self.keep {
            Retention::UntilReleased
        } else {
            Retention::Ttl
        }
    }
}

/// Arguments for `agentmux ask`: who to consult, what to ask, and how long to wait.
#[derive(Debug, Args)]
pub struct AskArgs {
    #[command(flatten)]
    pub delegate: DelegateArgs,
    #[command(flatten)]
    pub question: QuestionArgs,

    /// How long to wait for the answer, in seconds.
    /// `0` returns as soon as the child is running.
    #[arg(long, default_value_t = 600)]
    pub wait: u64,
}

/// Arguments for `agentmux start`: who to consult and what to ask.
#[derive(Debug, Args)]
pub struct StartArgs {
    #[command(flatten)]
    pub delegate: DelegateArgs,
    #[command(flatten)]
    pub question: QuestionArgs,
}

/// A command that names one consultation.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The consultation id, as printed by `start` or `list`.
    pub run_id: RunIdArg,
}

/// Arguments for `agentmux tail`: which consultation to follow, and from where.
#[derive(Debug, Args)]
pub struct TailArgs {
    /// The consultation id.
    pub run_id: RunIdArg,

    /// Start from this byte offset.
    /// Use the `next_cursor` a previous `tail` printed.
    #[arg(long, default_value_t = 0)]
    pub cursor: u64,

    /// Most bytes to print.
    #[arg(long, default_value_t = 64_000)]
    pub max_bytes: usize,

    /// Keep printing until the consultation finishes.
    #[arg(long, short = 'F')]
    pub follow: bool,
}

/// Arguments for `agentmux result`: which consultation to collect, and which page.
#[derive(Debug, Args)]
pub struct ResultArgs {
    /// The consultation id.
    pub run_id: RunIdArg,

    /// Wait up to this many seconds for the consultation to finish first.
    #[arg(long, default_value_t = 0)]
    pub wait: u64,

    /// Start from this byte offset.
    #[arg(long, default_value_t = 0)]
    pub offset: u64,

    /// Most bytes to print.
    #[arg(long, default_value_t = 2_000_000)]
    pub max_bytes: usize,
}

/// Arguments for `agentmux follow-up`: which consultation to continue, and with what.
///
/// Deliberately does not take `--cwd` or `--keep`.
/// A follow-up continues the delegate's own session, so the working directory and the retention
/// were both fixed when the consultation started.
#[derive(Debug, Args)]
pub struct FollowUpArgs {
    /// The consultation id.
    pub run_id: RunIdArg,

    /// The follow-up question.
    /// Omit it to read from stdin.
    pub question: Option<String>,

    /// Read the follow-up question from a file instead.
    #[arg(long, short = 'f', conflicts_with = "question")]
    pub file: Option<PathBuf>,

    /// How long to wait for the answer, in seconds.
    #[arg(long, default_value_t = 600)]
    pub wait: u64,
}

impl FollowUpArgs {
    /// Read the follow-up question from wherever it was supplied.
    ///
    /// # Errors
    ///
    /// Returns an error when the file or stdin cannot be read, or the question is empty.
    pub fn text(&self) -> Result<String> {
        QuestionArgs {
            env: Vec::new(),
            question: self.question.clone(),
            file: self.file.clone(),
            cwd: None,
            keep: false,
        }
        .text()
    }
}

/// Arguments for `agentmux list`: how many consultations to show.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Most consultations to list.
    #[arg(long, short = 'n', default_value_t = 20)]
    pub limit: usize,
}

/// Arguments for `agentmux prune`: one consultation to delete, or none to sweep.
#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Delete this consultation outright, whatever its retention.
    pub run_id: Option<RunIdArg>,
}

/// A consultation id parsed at the boundary, so nothing downstream can be handed a path.
#[derive(Debug, Clone)]
pub struct RunIdArg(RunId);

impl RunIdArg {
    /// The parsed id.
    #[must_use]
    pub fn id(&self) -> &RunId {
        &self.0
    }
}

impl std::str::FromStr for RunIdArg {
    type Err = agentmux::run::RunIdError;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        RunId::parse(value).map(Self)
    }
}
