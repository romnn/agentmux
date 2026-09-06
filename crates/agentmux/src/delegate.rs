//! The closed set of delegates, and the exact argument vector and environment each one needs.
//!
//! This module is the enforcement the whole crate exists for.
//! A caller does not compose a command line; it fills in a [`Delegate`] and the argument vector
//! follows as a total function over the enum.
//! There is no path through [`Delegate::invocation`] that omits the streaming flags, the hook
//! suppression, or the empty MCP config, because none of them is conditional.
//!
//! Two variants, not a trait: the set is closed at two, and an exhaustive `match` makes the
//! compiler enumerate every place a third vendor would need handling.
//!
//! # Model identifiers are opaque
//!
//! agentmux keeps no roster of models or reasoning efforts.
//! Both are pass-through strings, validated only for argv safety, so a new model identifier never
//! requires an agentmux release.
//! The delegate CLI is the sole authority on which values it accepts, and its rejection — which
//! names the valid set — reaches the caller verbatim.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A delegate argument that could not be used.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DelegateError {
    /// A model identifier or reasoning effort was empty, over-long, or not argv-safe.
    #[error("{field} {reason}: {value:?}")]
    Argument {
        /// Which argument was rejected.
        field: &'static str,
        /// Why it was rejected.
        reason: &'static str,
        /// What was supplied, so the caller can see its own mistake.
        value: String,
    },

    /// The child environment cannot be built because the host environment lacks something.
    #[error("cannot build the delegate environment: {0}")]
    Environment(String),
}

fn parse_token(
    field: &'static str,
    value: &str,
    max_len: usize,
    allowed: fn(char) -> bool,
) -> Result<String, DelegateError> {
    let reject = |reason| DelegateError::Argument {
        field,
        reason,
        value: value.to_owned(),
    };
    if value.is_empty() {
        return Err(reject("must not be empty"));
    }
    if value.chars().count() > max_len {
        return Err(reject("is too long"));
    }
    // A leading dash would land the value in flag position in the child's argv.
    if value.starts_with('-') {
        return Err(reject("must not start with '-'"));
    }
    if !value.chars().all(allowed) {
        return Err(reject("contains characters a delegate CLI will not accept"));
    }
    Ok(value.to_owned())
}

/// A model identifier, passed to the delegate CLI verbatim.
///
/// Deliberately not an enum.
/// See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelId(String);

impl ModelId {
    /// Longest identifier accepted.
    /// Well past anything either vendor ships.
    pub const MAX_LEN: usize = 128;

    /// Validate a model identifier for argv safety.
    ///
    /// Accepts what real identifiers contain, including the bracketed context-window suffix in
    /// `claude-opus-5[1m]`.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::Argument`] when the identifier is empty, over
    /// [`ModelId::MAX_LEN`], starts with `-`, or contains a character outside
    /// `[A-Za-z0-9._:/@\[\]-]`.
    pub fn parse(value: &str) -> Result<Self, DelegateError> {
        parse_token("model", value, Self::MAX_LEN, |c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '@' | '[' | ']' | '-')
        })
        .map(Self)
    }

    /// The identifier as the CLI will see it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A reasoning-effort level, passed to the delegate CLI verbatim.
///
/// Deliberately not an enum, for the same reason as [`ModelId`]: the vocabularies differ between
/// vendors and grow without notice.
/// agentmux forwards the string and lets the CLI judge it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Effort(String);

impl Effort {
    /// Longest effort name accepted.
    pub const MAX_LEN: usize = 32;

    /// Validate a reasoning-effort name for argv safety.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::Argument`] when the name is empty, over [`Effort::MAX_LEN`],
    /// starts with `-`, or contains a character outside `[A-Za-z0-9_-]`.
    pub fn parse(value: &str) -> Result<Self, DelegateError> {
        parse_token("effort", value, Self::MAX_LEN, |c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
        })
        .map(Self)
    }

    /// The name as the CLI will see it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! string_newtype_conversions {
    ($($ty:ident),+ $(,)?) => {$(
        impl TryFrom<String> for $ty {
            type Error = DelegateError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(&value)
            }
        }
        impl From<$ty> for String {
            fn from(value: $ty) -> Self {
                value.0
            }
        }
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    )+};
}
string_newtype_conversions!(ModelId, Effort);

/// Which Claude account a consultation authenticates as.
///
/// Two accounts live on one machine, separated by config directory.
/// `Personal` reproduces in process what a `CLAUDE_CONFIG_DIR` wrapper script does: point the CLI
/// at the other config directory and drop the API-key variables, so the session authenticates as
/// that account rather than silently spending an API key that happens to be exported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeAccount {
    /// The default account, using the CLI's own default config directory.
    #[default]
    Work,
    /// The secondary account, authenticated out of a separate config directory.
    Personal,
}

impl ClaudeAccount {
    /// Environment variable that overrides where the personal account's config directory lives.
    ///
    /// Unset, the personal account is `$HOME/.claude-personal`.
    pub const CONFIG_DIR_ENV: &'static str = "AGENTMUX_CLAUDE_PERSONAL_CONFIG_DIR";

    /// Directory name appended to `$HOME` when [`Self::CONFIG_DIR_ENV`] is unset.
    pub const DEFAULT_PERSONAL_DIR: &'static str = ".claude-personal";
}

/// How much of the filesystem a Codex delegate may write.
///
/// A consultation is a second opinion, so `ReadOnly` is the default.
/// `WorkspaceWrite` exists because a delegate asked to produce a large artefact cannot deliver it
/// through a read-only sandbox — a read-only delegate told to write its report to a path completes
/// the work, fails the write, and reports the failure instead of the findings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexSandbox {
    /// The delegate may read the working directory and nothing else.
    #[default]
    ReadOnly,
    /// The delegate may also write inside its working directory.
    WorkspaceWrite,
}

impl CodexSandbox {
    /// The value Codex uses for this mode, both as `--sandbox` and as `sandbox_mode`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }
}

/// Which CLI a consultation is talking to, without the options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vendor {
    /// Anthropic's `claude` CLI.
    Claude,
    /// The `codex` CLI from `OpenAI`.
    Codex,
}

impl Vendor {
    /// The executable name, as found on `PATH`.
    #[must_use]
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        })
    }
}

/// Who is being consulted, and on what terms.
///
/// Each variant carries only what that vendor accepts.
/// A Codex consultation with a Claude account, or a Claude consultation with a sandbox mode, does
/// not compile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "vendor", rename_all = "snake_case")]
pub enum Delegate {
    /// Anthropic's CLI.
    Claude {
        /// Model identifier, forwarded verbatim.
        model: ModelId,
        /// Reasoning effort, forwarded verbatim.
        effort: Effort,
        /// Which of the machine's two Claude accounts to authenticate as.
        #[serde(default)]
        account: ClaudeAccount,
    },
    /// The `OpenAI` CLI.
    Codex {
        /// Model identifier, forwarded verbatim.
        model: ModelId,
        /// Reasoning effort, forwarded verbatim.
        effort: Effort,
        /// How much the delegate may write.
        #[serde(default)]
        sandbox: CodexSandbox,
    },
}

impl Delegate {
    /// Which CLI this delegate runs.
    #[must_use]
    pub fn vendor(&self) -> Vendor {
        match self {
            Self::Claude { .. } => Vendor::Claude,
            Self::Codex { .. } => Vendor::Codex,
        }
    }

    /// The model identifier being consulted.
    #[must_use]
    pub fn model(&self) -> &ModelId {
        match self {
            Self::Claude { model, .. } | Self::Codex { model, .. } => model,
        }
    }

    /// The reasoning effort being requested.
    #[must_use]
    pub fn effort(&self) -> &Effort {
        match self {
            Self::Claude { effort, .. } | Self::Codex { effort, .. } => effort,
        }
    }

    /// A one-line description, for `list` output and log lines.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Claude {
                model,
                effort,
                account,
            } => {
                let account = match account {
                    ClaudeAccount::Work => "work",
                    ClaudeAccount::Personal => "personal",
                };
                format!("claude {model} effort={effort} account={account}")
            }
            Self::Codex {
                model,
                effort,
                sandbox,
            } => {
                format!("codex {model} effort={effort} sandbox={}", sandbox.as_str())
            }
        }
    }
}

/// Everything that varies between the turns of one consultation.
#[derive(Debug, Clone)]
pub struct TurnPlan<'a> {
    /// File holding the question.
    /// It becomes the child's stdin, so no pipe needs draining.
    pub question_path: &'a Path,
    /// Where Codex should drop its closing message.
    /// Ignored by the Claude delegate.
    pub last_message_path: &'a Path,
    /// The delegate's own session identifier, when continuing a consultation.
    pub resume: Option<&'a SessionRef>,
}

/// The delegate CLI's own handle on a conversation, so a later turn can continue it.
///
/// Claude calls this a session id and Codex calls it a thread id; both are opaque to agentmux and
/// both are recovered by parsing the delegate's event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionRef(String);

impl SessionRef {
    /// Longest identifier accepted.
    /// Both vendors use UUIDs; this leaves room for a thread name.
    pub const MAX_LEN: usize = 200;

    /// Wrap an identifier the delegate emitted.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::Argument`] when the identifier is not argv-safe, which would mean
    /// the delegate emitted something agentmux cannot hand back to it.
    pub fn parse(value: &str) -> Result<Self, DelegateError> {
        parse_token("session id", value, Self::MAX_LEN, |c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
        })
        .map(Self)
    }

    /// The identifier as the CLI will see it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A fully prepared child process: what to run, with which arguments, in which environment.
///
/// Produced only by [`Delegate::invocation`], so the flags that make capture correct cannot be
/// forgotten at a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Executable name, resolved against the child's `PATH`.
    pub program: &'static str,
    /// The complete argument vector, excluding `argv[0]`.
    pub args: Vec<String>,
    /// The child's entire environment, built as an allowlist from empty.
    pub env: BTreeMap<String, String>,
}

/// Environment variables every delegate needs regardless of vendor.
///
/// The child environment is built as an allowlist from empty rather than by filtering a denylist.
/// A denylist forgets the variable that was added last week; an allowlist cannot leak a variable
/// nobody thought about.
/// The host that launches agentmux is itself a coding-agent session, so its environment is full of
/// session identity — inheriting it would make the delegate a nested session wearing its parent's
/// identity.
const BASE_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TMPDIR",
    // Without proxy and CA settings a delegate cannot reach its provider from a managed network.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// Credential variables the Claude CLI reads.
///
/// Forwarded for the work account, withheld from the personal account so it authenticates as
/// itself rather than as whatever key is exported.
const CLAUDE_CREDENTIAL_ALLOWLIST: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
];

/// Credential variables the Codex CLI reads.
const CODEX_CREDENTIAL_ALLOWLIST: &[&str] = &["OPENAI_API_KEY", "OPENAI_BASE_URL", "CODEX_HOME"];

/// The empty MCP configuration handed to the Claude delegate.
///
/// Without it the delegate loads this machine's MCP servers, including agentmux itself, and a
/// consultation can recurse.
const EMPTY_MCP_CONFIG: &str = r#"{"mcpServers":{}}"#;

/// The built-in tools a Claude delegate may use.
///
/// A consultation is a second opinion, not an edit, so `Edit`, `Write` and `NotebookEdit` are not
/// offered.
/// That is a real reduction but not a sandbox: `Bash` is on the list, and a delegate that wanted
/// to write a file could.
/// What actually holds the line is `--permission-mode plan`, which the vendor enforces and reports
/// in prose rather than structurally.
/// Treat a Claude delegate as read-only by cooperation, and a Codex delegate under
/// `CodexSandbox::ReadOnly` as read-only by enforcement.
const CLAUDE_READ_ONLY_TOOLS: &str = "Bash,Read,Grep,Glob,WebFetch,WebSearch";

impl Delegate {
    /// Build the child process for one turn.
    ///
    /// `host_env` is the environment agentmux itself was launched with; only the variables named
    /// in the allowlists above are carried into the child.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError`] only for a personal-account consultation whose config directory
    /// cannot be located, which means `HOME` is absent from the host environment.
    pub fn invocation(
        &self,
        plan: &TurnPlan<'_>,
        host_env: &BTreeMap<String, String>,
    ) -> Result<Invocation, DelegateError> {
        let mut env: BTreeMap<String, String> = BASE_ALLOWLIST
            .iter()
            .filter_map(|key| {
                host_env
                    .get(*key)
                    .map(|value| ((*key).to_owned(), value.clone()))
            })
            .collect();
        // A delegate is a non-interactive child with no terminal.
        // Saying so keeps CLIs from emitting cursor control sequences and colour into the file
        // that captures their event stream, and `CI` is the conventional way to ask a tool for
        // its non-interactive behaviour.
        env.insert("TERM".to_owned(), "dumb".to_owned());
        env.insert("NO_COLOR".to_owned(), "1".to_owned());
        env.insert("CI".to_owned(), "1".to_owned());

        let args = match self {
            Self::Claude {
                model,
                effort,
                account,
            } => {
                for key in CLAUDE_CREDENTIAL_ALLOWLIST {
                    // The personal account is exactly the work account minus these variables plus
                    // its own config directory; that is the whole difference between the two.
                    if *account == ClaudeAccount::Work
                        && let Some(value) = host_env.get(*key)
                    {
                        env.insert((*key).to_owned(), value.clone());
                    }
                }
                if *account == ClaudeAccount::Personal {
                    env.insert(
                        "CLAUDE_CONFIG_DIR".to_owned(),
                        personal_config_dir(host_env)?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                claude_args(model, effort, plan)
            }
            Self::Codex {
                model,
                effort,
                sandbox,
            } => {
                for key in CODEX_CREDENTIAL_ALLOWLIST {
                    if let Some(value) = host_env.get(*key) {
                        env.insert((*key).to_owned(), value.clone());
                    }
                }
                codex_args(model, effort, *sandbox, plan)
            }
        };

        Ok(Invocation {
            program: self.vendor().program(),
            args,
            env,
        })
    }
}

fn personal_config_dir(host_env: &BTreeMap<String, String>) -> Result<PathBuf, DelegateError> {
    if let Some(explicit) = host_env.get(ClaudeAccount::CONFIG_DIR_ENV) {
        return Ok(PathBuf::from(explicit));
    }
    let home = host_env.get("HOME").ok_or_else(|| {
        DelegateError::Environment(
            "the personal Claude account lives under $HOME, which is not set. Set HOME, or point \
             AGENTMUX_CLAUDE_PERSONAL_CONFIG_DIR at that account's config directory."
                .to_owned(),
        )
    })?;
    Ok(PathBuf::from(home).join(ClaudeAccount::DEFAULT_PERSONAL_DIR))
}

/// Argument vector for the Claude CLI.
///
/// Every flag here is load-bearing:
///
/// - `--output-format stream-json` with `--verbose`: `text` and `json` return only the *last*
///   assistant message, which is not the same thing as the answer; `--verbose` is mandatory
///   alongside `stream-json` under `--print` or the CLI rejects it.
/// - `--setting-sources ""`: loads no user, project or local settings, so no `Stop` hook can
///   reopen the finished turn.
///   This is structural — it does not depend on any hook cooperating.
///   It does not suppress `CLAUDE.md` discovery, so a delegate still sees the project's own
///   instructions.
/// - `--strict-mcp-config` with an empty config: keeps the delegate from re-entering agentmux.
/// - `--permission-mode plan` with an edit-free `--tools` list: a second opinion, not an edit.
///   See [`CLAUDE_READ_ONLY_TOOLS`] for what that does and does not guarantee.
/// - `--no-session-persistence` is deliberately **absent**.
///   It defeats `--resume`, and a consultation must stay open to a follow-up.
fn claude_args(model: &ModelId, effort: &Effort, plan: &TurnPlan<'_>) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.push("--print".to_owned());
    if let Some(session) = plan.resume {
        args.push("--resume".to_owned());
        args.push(session.as_str().to_owned());
    }
    args.push("--model".to_owned());
    args.push(model.as_str().to_owned());
    args.push("--effort".to_owned());
    args.push(effort.as_str().to_owned());
    args.push("--permission-mode".to_owned());
    args.push("plan".to_owned());
    args.push("--tools".to_owned());
    args.push(CLAUDE_READ_ONLY_TOOLS.to_owned());
    args.push("--output-format".to_owned());
    args.push("stream-json".to_owned());
    args.push("--verbose".to_owned());
    args.push("--setting-sources".to_owned());
    args.push(String::new());
    args.push("--strict-mcp-config".to_owned());
    args.push("--mcp-config".to_owned());
    args.push(EMPTY_MCP_CONFIG.to_owned());
    args
}

/// Argument vector for the Codex CLI.
///
/// - `--json`: without it stdout carries only the final answer and stays empty until the process
///   exits, so there is neither progress nor a recovery path.
/// - `-c features.hooks=false`: the structural hook defence on this side.
/// - `--ignore-user-config`: the isolation control.
///   It drops `$CODEX_HOME/config.toml`, and with it the machine's configured MCP servers, so the
///   delegate cannot re-enter agentmux or reach any other server the host happens to run.
///   Authentication is unaffected, because it comes from `CODEX_HOME` rather than the config
///   file.
/// - `--output-last-message`: written by the codex process rather than by the agent, so a
///   read-only sandbox does not block it.
///   It is a convenience; the event stream is the truth.
/// - `resume` **rejects `--sandbox`** and exits 2.
///   The sandbox goes through `-c sandbox_mode` there instead.
///   Resume also does not inherit the original model, so model and effort are re-pinned on every
///   turn.
/// - `--ephemeral` is deliberately **absent**: it does not persist the session, which would make
///   a follow-up impossible.
/// - `-c mcp_servers={}` is deliberately **absent** because it does not work.
///   Measured against codex-cli 0.153.4: a delegate launched with that override still reached a
///   configured MCP server and successfully called one of its tools.
///   Only `--ignore-user-config` actually detaches them.
///   Its cost is the user's `model_catalog_json`, so a delegate may report fallback metadata for
///   a very new model; that arrives as an advisory item rather than a failure.
fn codex_args(
    model: &ModelId,
    effort: &Effort,
    sandbox: CodexSandbox,
    plan: &TurnPlan<'_>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.push("exec".to_owned());
    if let Some(session) = plan.resume {
        args.push("resume".to_owned());
        args.push(session.as_str().to_owned());
    }
    args.push("--json".to_owned());
    args.push("--skip-git-repo-check".to_owned());
    args.push("--model".to_owned());
    args.push(model.as_str().to_owned());
    // `Effort` and the sandbox literals are validated to contain no quote or backslash, so a
    // bare TOML basic string needs no escaping.
    args.push("--config".to_owned());
    args.push(format!("model_reasoning_effort=\"{effort}\""));
    args.push("--ignore-user-config".to_owned());
    // Redundant with `--ignore-user-config` for config-file hooks, and cheap insurance against a
    // hook reaching the delegate by any other route.
    args.push("--config".to_owned());
    args.push("features.hooks=false".to_owned());
    if plan.resume.is_some() {
        args.push("--config".to_owned());
        args.push(format!("sandbox_mode=\"{}\"", sandbox.as_str()));
    } else {
        args.push("--sandbox".to_owned());
        args.push(sandbox.as_str().to_owned());
    }
    args.push("--output-last-message".to_owned());
    args.push(plan.last_message_path.to_string_lossy().into_owned());
    // `-` makes Codex read the prompt from stdin.
    // Without it Codex still drains stdin looking for extra input and a child whose stdin never
    // closes blocks.
    args.push("-".to_owned());
    args
}
