//! The command-line surface.
//!
//! Changes when the CLI's arguments change.
//!
//! The same eleven verbs the MCP server exposes, plus `mcp` to serve them, `accounts` to show what
//! this machine defines, and `prune` to remove consultations by hand.
//! Two more are hidden: `supervise`, the process the run store launches for every turn to start
//! its delegate and settle it, and `codex-app-server`, the driver it launches as the delegate of a
//! Codex turn it runs over `codex app-server`.
//! A human debugging a delegation and an agent calling the tool are exercising exactly the same
//! code path, which is the point: if `agentmux ask` works from a terminal, the tool works.

use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use agentmux::delegate::{
    Billing, ClaudePermissionMode, ClaudeTool, CodexSandbox, Delegate, Isolation,
    PermissionOptions, Vendor,
};
use agentmux::run::{Retention, RunId, RunLabel};
use agentmux::transcript::TurnState;
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

    /// How a Codex turn that can be steered is launched.
    ///
    /// `app-server` drives a Codex consultation that inherits its account's settings over
    /// `codex app-server`, which is what lets `steer` reach its running turn; an isolated one runs
    /// over `codex exec` either way.
    /// `exec` launches every Codex turn with `codex exec`, for a codex CLI whose app-server
    /// cannot run these turns; no Codex turn can then be steered.
    #[arg(
        long,
        global = true,
        value_enum,
        env = "AGENTMUX_CODEX_LAUNCH",
        default_value_t = CodexLaunchArg::AppServer
    )]
    pub codex_launch: CodexLaunchArg,

    /// How long a turn's supervisor waits before it first resumes a consultation whose model
    /// was overloaded; later resumes in a row double it, up to fifteen minutes.
    ///
    /// Hidden, because thirty seconds is the delay to use; the tests shorten it.
    #[arg(long, global = true, hide = true, value_name = "DURATION", value_parser = humantime::parse_duration)]
    pub auto_resume_delay: Option<std::time::Duration>,

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

    /// Add an instruction to a consultation's running turn, which the delegate sees before its
    /// next model call.
    ///
    /// Only a turn launched over `codex app-server` takes one; `status` says whether the running
    /// turn is steerable.
    Steer(SteerArgs),

    /// Block until consultations' turns have settled, then print them.
    ///
    /// Returns only once each turn's transcript is published and final, never on a transcript
    /// that merely looks finished, and as long as that takes unless `--timeout` says otherwise.
    /// The exit status says how it ended: 0 completed, 10 failed, 11 cancelled, 124 when the
    /// timeout passed first, 1 when agentmux itself failed and 2 for arguments it cannot act on.
    /// Run it in a background shell and its exit is the completion notice.
    Wait(WaitArgs),

    /// Stop a running consultation, keeping everything collected so far.
    Cancel(RunArgs),

    /// List recent consultations.
    List(ListArgs),

    /// Delete consultations past their retention, or one by id.
    Prune(PruneArgs),

    /// List the account aliases, model rewrites and default efforts this machine defines, and
    /// where they came from.
    Accounts,

    /// Report what each configured account has left of its usage windows.
    Quota(QuotaArgs),

    /// Serve the tools over stdio, for an MCP host to launch.
    Mcp(McpArgs),

    /// Drive one Codex turn over `codex app-server`.
    ///
    /// Launched by the run store as a consultation's delegate, never by hand.
    #[command(name = "codex-app-server", hide = true)]
    CodexAppServer(CodexAppServerArgs),

    /// Start one turn's delegate, wait for it, and settle the turn.
    ///
    /// Launched by the run store for every turn, never by hand.
    #[command(name = "supervise", hide = true)]
    Supervise(SuperviseArgs),
}

/// How a Codex turn that can be steered is launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CodexLaunchArg {
    /// Over `codex app-server`, driven by this executable, so the turn can be steered.
    AppServer,
    /// With `codex exec`, which reads its question once.
    Exec,
}

/// The plan a driven Codex turn runs under.
#[derive(Debug, Args)]
pub struct CodexAppServerArgs {
    /// The plan the run store wrote into the turn's directory.
    #[arg(value_name = "PLAN")]
    pub plan_path: PathBuf,
}

/// The plan a supervised turn runs under.
#[derive(Debug, Args)]
pub struct SuperviseArgs {
    /// The plan the run store wrote into the turn's directory.
    #[arg(value_name = "PLAN")]
    pub plan_path: PathBuf,
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
    pub deny: Vec<Vendor>,
}

/// Which delegate to consult, and on what terms.
#[derive(Debug, Args)]
pub struct DelegateArgs {
    /// Which CLI to consult: `claude` reaches Anthropic models, `codex` reaches GPT models.
    #[arg(long, value_enum)]
    pub delegate: Vendor,

    /// Model identifier: a family alias this machine's `agentmux.toml` defines, such as `opus`,
    /// which `agentmux accounts` lists, or the vendor's own identifier.
    /// agentmux keeps no built-in roster; this machine's rewrites and blocks apply before launch,
    /// and the delegate CLI decides what else is valid.
    /// A `[models]` rule in `agentmux.toml` may rewrite one identifier to another, which
    /// `agentmux accounts` lists and every report of the consultation names.
    /// A `[blocked]` entry refuses a model and names the one to ask for instead.
    #[arg(long)]
    pub model: String,

    /// Reasoning effort, passed verbatim, for example `xhigh`.
    /// Omitted, an `[efforts]` rule in `agentmux.toml` for the model that runs decides, and
    /// failing that the delegate CLI's own default.
    #[arg(long)]
    pub effort: Option<String>,

    /// Which configured account to authenticate as, for either vendor.
    ///
    /// Aliases come from `agentmux.toml`; run `agentmux accounts` to see the ones this machine
    /// defines.
    /// Omit it to use the account `agentmux.toml` names as the default, or the CLI's own login
    /// when it names none.
    #[arg(long)]
    pub account: Option<String>,

    /// Whether the delegate loads the user's configuration, and what it may do.
    #[command(flatten)]
    pub permissions: PermissionArgs,

    /// Whether the delegate's environment may carry an API key; omitted, the account decides.
    ///
    /// No key from the environment reaches a delegate unless `agentmux.toml` marks its account
    /// `billing = "api"`, which `agentmux accounts` shows.
    /// `api` is refused for any other account: it insists on the permission, it cannot grant it.
    /// `subscription` withholds the key even from an account that allows one.
    /// Follow-ups keep whatever the first turn was allowed.
    #[arg(long, value_enum)]
    pub billing: Option<Billing>,
}

/// Which vendor's accounts to ask about.
#[derive(Debug, Args)]
pub struct QuotaArgs {
    /// Ask only this vendor.
    /// Both are asked when omitted.
    #[arg(long, value_enum)]
    pub delegate: Option<Vendor>,
}

/// Whether the delegate loads the user's configuration, and what it may do.
///
/// On `start` and `ask` an omitted flag takes the machine's default, which `agentmux accounts`
/// lists; on `follow-up` it keeps what the newest turn ran with.
/// A value beyond the machine's limits is refused, never lowered to fit.
#[derive(Debug, Args)]
pub struct PermissionArgs {
    /// Skip the user's own CLI configuration: settings, hooks and MCP servers.
    ///
    /// `--ignore-user-config` alone means `true`; `--ignore-user-config=false` loads them.
    /// Omitted, the machine's default for the vendor decides, which `agentmux accounts` shows.
    /// With `false`, a hook can reopen the delegate's finished turn.
    #[arg(
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL"
    )]
    pub ignore_user_config: Option<bool>,

    /// Load the account's own settings, hooks and MCP servers: `--ignore-user-config=false`.
    #[arg(long, conflicts_with = "isolated")]
    pub inherit_settings: bool,

    /// Load none of the account's settings, hooks or MCP servers: `--ignore-user-config`.
    #[arg(long, conflicts_with = "inherit_settings")]
    pub isolated: bool,

    /// How much the delegate's commands may write, for `--delegate codex`.
    /// Omitted, the machine's default, or `read-only`.
    /// `danger-full-access` is refused unless the machine allows it.
    #[arg(long, value_enum)]
    pub sandbox: Option<CodexSandbox>,

    /// Whether the delegate's commands may reach the network, for `--delegate codex`.
    /// `--network-access` alone means `true`.
    /// Omitted, the machine's default, or Codex's own configuration.
    #[arg(
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL"
    )]
    pub network_access: Option<bool>,

    /// The permission mode, for `--delegate claude`; omitted, the machine's default, or `plan`.
    #[arg(long, value_enum)]
    pub permission_mode: Option<ClaudePermissionMode>,

    /// The built-in tools offered, comma-separated, for `--delegate claude`; `""` offers none.
    #[arg(long, value_delimiter = ',', value_name = "TOOLS")]
    pub tools: Option<Vec<String>>,

    /// Tool calls to preapprove, comma-separated, such as `WebFetch,Bash(cargo check *)`, for
    /// `--delegate claude`.
    #[arg(long, value_delimiter = ',', value_name = "RULES")]
    pub allowed_tools: Option<Vec<String>>,

    /// Tool calls to deny, comma-separated, such as `Bash(git push *)`, for `--delegate claude`.
    #[arg(long, value_delimiter = ',', value_name = "RULES")]
    pub disallowed_tools: Option<Vec<String>>,
}

impl PermissionArgs {
    /// The options asked for on the command line.
    ///
    /// # Errors
    ///
    /// Returns an error when the isolation is spelled twice and the spellings disagree, or a
    /// tool rule is not one the CLI can take.
    pub fn options(&self) -> Result<PermissionOptions> {
        let inherit_settings = match (self.inherit_settings, self.isolated) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        };
        // An empty value, as `--tools ""` gives, is no entry rather than an empty rule.
        let tools = |list: &Option<Vec<String>>| {
            list.as_ref()
                .map(|list| {
                    let entries: Vec<&String> =
                        list.iter().filter(|entry| !entry.is_empty()).collect();
                    ClaudeTool::parse_all(&entries)
                })
                .transpose()
        };
        Ok(PermissionOptions {
            isolation: Isolation::from_spellings(self.ignore_user_config, inherit_settings)?,
            sandbox: self.sandbox,
            network_access: self.network_access,
            permission_mode: self.permission_mode,
            tools: tools(&self.tools)?,
            allowed_tools: tools(&self.allowed_tools)?,
            disallowed_tools: tools(&self.disallowed_tools)?,
        })
    }
}

impl DelegateArgs {
    /// Build the delegate, rejecting an option that belongs to the other vendor.
    ///
    /// Existence of the alias is not checked here: which aliases are defined depends on the
    /// working directory the consultation will run in, which `build` does not know.
    ///
    /// # Errors
    ///
    /// Returns an error when the model, effort, alias or a tool rule is not argv-safe, when the
    /// isolation is spelled twice and the spellings disagree, or when a vendor-specific option
    /// was set for the wrong vendor.
    pub fn build(&self) -> Result<Delegate> {
        Delegate::from_parts(
            self.delegate,
            &self.model,
            self.effort.as_deref(),
            self.account.as_deref(),
            self.permissions.options()?,
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
    /// Only useful for a delegate that loads the user's configuration: an isolated one
    /// (`--ignore-user-config`) loads no settings, hooks or MCP servers, so nothing in it reads
    /// these.
    /// Standing settings belong in `agentmux.toml`, which applies them to every launch.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Keep the consultation indefinitely so a follow-up can arrive at any time.
    ///
    /// Without this it is swept once its delegate has written nothing for a week, or for the
    /// `retention` the machine's `agentmux.toml` sets.
    #[arg(long)]
    pub keep: bool,

    /// A short purpose for the consultation, such as `"REVIEW CRD G1"`, which `status`, `list`
    /// and `wait` show and `list --label` finds it by.
    #[arg(long, value_parser = RunLabel::parse)]
    pub label: Option<RunLabel>,
}

impl QuestionArgs {
    /// Read the question from wherever it was supplied.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, stdin cannot be read, or the question is
    /// empty — a delegate given an empty brief burns money to say nothing.
    pub fn text(&self) -> Result<String> {
        supplied_text(self.question.as_deref(), self.file.as_deref())
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

/// The text given as an argument, else read from `file`, else from stdin, refused when empty.
fn supplied_text(text: Option<&str>, file: Option<&Path>) -> Result<String> {
    let text = match (text, file) {
        (Some(text), _) => text.to_owned(),
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

    /// Start with the last `--max-bytes` of the transcript, what the delegate wrote most recently,
    /// rather than from a cursor.
    #[arg(long, conflicts_with = "cursor")]
    pub from_end: bool,

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
/// It does take the isolation and permission flags, as changes to what was recorded.
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

    /// A new short purpose for this turn and the ones after it; omitted, the turn keeps the
    /// consultation's current label.
    #[arg(long, value_parser = RunLabel::parse)]
    pub label: Option<RunLabel>,

    /// Changes to the consultation's isolation or permissions for this turn and the ones after
    /// it; whatever is omitted keeps what the newest turn ran with.
    #[command(flatten)]
    pub permissions: PermissionArgs,
}

impl FollowUpArgs {
    /// Read the follow-up question from wherever it was supplied.
    ///
    /// # Errors
    ///
    /// Returns an error when the file or stdin cannot be read, or the question is empty.
    pub fn text(&self) -> Result<String> {
        supplied_text(self.question.as_deref(), self.file.as_deref())
    }
}

/// Arguments for `agentmux steer`: which consultation, and what to tell its running turn.
#[derive(Debug, Args)]
pub struct SteerArgs {
    /// The consultation id.
    pub run_id: RunIdArg,

    /// The instruction.
    /// Omit it to read from stdin.
    pub message: Option<String>,

    /// Read the instruction from a file instead.
    #[arg(long, short = 'f', conflicts_with = "message")]
    pub file: Option<PathBuf>,
}

impl SteerArgs {
    /// Read the instruction from wherever it was supplied.
    ///
    /// # Errors
    ///
    /// Returns an error when the file or stdin cannot be read, or the instruction is empty.
    pub fn text(&self) -> Result<String> {
        supplied_text(self.message.as_deref(), self.file.as_deref())
    }
}

/// Arguments for `agentmux wait`: which turns to wait for, and for how many of them.
#[derive(Debug, Args)]
pub struct WaitArgs {
    /// The consultation ids.
    #[arg(required = true, value_name = "RUN")]
    pub run_ids: Vec<RunIdArg>,

    /// The turn to wait for, counted from 1 as the transcript's headings count, in every RUN.
    /// Defaults to each consultation's newest turn as the wait begins; a follow-up started
    /// afterwards is not waited for.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub turn: Option<u32>,

    /// Return once any one has settled; among several that have, the first named.
    #[arg(long, conflicts_with = "all")]
    pub any: bool,

    /// Return once every one has settled, which is the default.
    #[arg(long)]
    pub all: bool,

    /// Give up after this long, such as `90s` or `2h`, and exit 124; the consultations carry on.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    pub timeout: Option<std::time::Duration>,
}

/// Arguments for `agentmux list`: how many consultations to show, and which.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Most consultations to list.
    #[arg(long, short = 'n', default_value_t = 20)]
    pub limit: usize,

    /// Only consultations whose label holds this text, ignoring case: the label `start` gave it,
    /// or the one its newest turn has.
    #[arg(long, value_name = "TEXT")]
    pub label: Option<String>,

    /// Only consultations whose newest turn stands so.
    #[arg(long, value_enum)]
    pub state: Option<TurnState>,
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

#[cfg(test)]
mod tests {
    use agentmux::delegate::{ClaudePermissionMode, CodexSandbox, Isolation, PermissionOptions};
    use clap::Parser as _;
    use googletest::prelude::*;

    use super::{Cli, Command};

    /// The permission flags of `agentmux follow-up <id> q <flags>`.
    fn follow_up_permissions(flags: &[&str]) -> Result<super::PermissionArgs> {
        let argv = ["agentmux", "follow-up", "20261020T005204-388ea0d8", "q"]
            .into_iter()
            .chain(flags.iter().copied());
        let cli = Cli::try_parse_from(argv).or_fail()?;
        let follow_up = match cli.command {
            Command::FollowUp(follow_up) => Some(follow_up),
            _ => None,
        }
        .or_fail()?;
        Ok(follow_up.permissions)
    }

    /// The permission options `agentmux follow-up <id> q <flags>` parses to.
    fn follow_up_options(flags: &[&str]) -> Result<PermissionOptions> {
        follow_up_permissions(flags)?.options().or_fail()
    }

    /// Every spelling of the isolation reaches the same choice, and a bare boolean flag means
    /// `true`.
    #[gtest]
    fn each_isolation_spelling_parses_to_one_choice() -> Result<()> {
        for (flags, expected) in [
            (&["--ignore-user-config"][..], Some(Isolation::Isolated)),
            (
                &["--ignore-user-config=false"][..],
                Some(Isolation::Inherit),
            ),
            (&["--isolated"][..], Some(Isolation::Isolated)),
            (&["--inherit-settings"][..], Some(Isolation::Inherit)),
            (
                &["--ignore-user-config=false", "--inherit-settings"][..],
                Some(Isolation::Inherit),
            ),
            (&[][..], None),
        ] {
            assert_that!(
                follow_up_options(flags)?.isolation,
                eq(expected),
                "{flags:?}"
            );
        }
        // Two spellings that disagree are refused, not resolved one way.
        let args = follow_up_permissions(&["--ignore-user-config", "--inherit-settings"])?;
        assert_that!(
            args.options().map_err(|err| err.to_string()),
            err(contains_substring("Conflicting settings"))
        );
        Ok(())
    }

    /// The permission flags parse in the CLI's own spellings, and an empty tool list offers none.
    #[gtest]
    fn the_permission_flags_parse() -> Result<()> {
        let codex = follow_up_options(&["--sandbox", "workspace-write", "--network-access"])?;
        assert_that!(codex.sandbox, some(eq(CodexSandbox::WorkspaceWrite)));
        assert_that!(codex.network_access, some(eq(true)));

        let claude = follow_up_options(&[
            "--permission-mode",
            "acceptEdits",
            "--tools",
            "",
            "--disallowed-tools",
            "Bash(git push *),WebSearch",
        ])?;
        assert_that!(
            claude.permission_mode,
            some(eq(ClaudePermissionMode::AcceptEdits))
        );
        assert_that!(claude.tools, some(is_empty()));
        assert_that!(claude.disallowed_tools.map(|list| list.len()), some(eq(2)));
        Ok(())
    }

    /// `--from-end` reads the latest output with `--follow`, but a cursor names another start,
    /// so the two together are refused, an explicit `--cursor 0` included.
    #[gtest]
    fn from_end_parses_and_conflicts_with_a_cursor() -> Result<()> {
        let parse = |flags: &[&str]| {
            Cli::try_parse_from(
                ["agentmux", "tail", "20261020T005204-388ea0d8"]
                    .into_iter()
                    .chain(flags.iter().copied()),
            )
        };
        let cli = parse(&["--from-end", "--max-bytes", "2000", "--follow"]).or_fail()?;
        let tail = match cli.command {
            Command::Tail(tail) => Some(tail),
            _ => None,
        }
        .or_fail()?;
        assert_that!(tail.from_end, eq(true));
        assert_that!(tail.max_bytes, eq(2000));
        assert_that!(tail.follow, eq(true));

        assert_that!(
            parse(&["--from-end", "--cursor", "0"]).map(|_| ()),
            err(anything())
        );
        Ok(())
    }
}
