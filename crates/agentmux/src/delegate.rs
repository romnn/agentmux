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
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{Config, Secret};

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

    /// The requested account alias is not defined by this machine's configuration.
    ///
    /// The message lists what *is* defined, because the caller is usually an agent that guessed a
    /// plausible name and cannot see the file.
    #[error("no {vendor} account named `{alias}` is configured{}. {}",
        describe_selection(selected_by.as_deref()),
        describe_configured(configured, config_path.as_deref()))]
    UnknownAccount {
        /// Which CLI the alias was being resolved for.
        vendor: Vendor,
        /// The alias that was asked for.
        alias: String,
        /// Every alias that vendor does have.
        configured: Vec<String>,
        /// Which file supplied them, when one did.
        config_path: Option<std::path::PathBuf>,
        /// The file that chose this alias, when the caller did not.
        ///
        /// A default selected by a checked-out `agentmux.toml` is the likeliest way to meet this
        /// error without having named anything, and the file is the thing to go and fix.
        selected_by: Option<std::path::PathBuf>,
    },

    /// An account names a configuration directory that is not there.
    #[error(
        "the {vendor} account `{alias}` points at {path}, which does not exist. Either that \
         account has not been logged in on this machine, or the path is wrong."
    )]
    AccountDirMissing {
        /// Which CLI the account belongs to.
        vendor: Vendor,
        /// The alias that was asked for.
        alias: String,
        /// The directory that is missing.
        path: std::path::PathBuf,
    },
}

/// How an account reads in a one-line summary.
///
/// Named rather than left blank when absent, because "which identity ran this" is the question a
/// reader of a surprising result asks first.
/// "The CLI's own login" and "a configured account" are spelled differently, because a listing in
/// which both read `default` cannot answer that question.
fn describe_account(alias: Option<&AccountAlias>) -> &str {
    alias.map_or("cli-default", AccountAlias::as_str)
}

/// Name the file that chose an alias the caller did not ask for.
fn describe_selection(selected_by: Option<&std::path::Path>) -> String {
    selected_by.map_or_else(String::new, |path| {
        format!(", and {} selects it as the default", path.display())
    })
}

/// Describe which aliases exist, for an unknown-alias error.
fn describe_configured(configured: &[String], source: Option<&std::path::Path>) -> String {
    if configured.is_empty() {
        return match source {
            Some(path) => format!("{} defines none for it.", path.display()),
            None => "No agentmux.toml was found, so no accounts are configured. Create one at \
                     ~/.config/agentmux/agentmux.toml, or omit `account` to use the CLI's own \
                     default."
                .to_owned(),
        };
    }
    let names = configured.join(", ");
    match source {
        Some(path) => format!("{} defines: {names}.", path.display()),
        None => format!("Configured: {names}."),
    }
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

/// An account alias, resolved through the machine's configuration.
///
/// Opaque for the same reason a model id is: agentmux keeps no roster of accounts, and the set of
/// them is a property of the machine rather than of this crate.
/// The alias is what an agent names; [`crate::config::Config`] is what turns it into a directory,
/// so one prompt works across machines that store the same account in different places.
///
/// Omitting an alias entirely runs the vendor CLI against its own default configuration, which is
/// what a machine with a single account of that vendor wants and needs no configuration for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountAlias(String);

impl AccountAlias {
    /// Longest accepted alias.
    const MAX_LEN: usize = 64;

    /// Validate an alias.
    ///
    /// Aliases name a table key in `agentmux.toml` and appear in error messages, so they are held
    /// to a plain-identifier shape rather than to argv-safety alone.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::Argument`] when the alias is empty, over-long, or contains
    /// anything but letters, digits, `-`, `_` or `.`.
    pub fn parse(value: &str) -> Result<Self, DelegateError> {
        parse_token("account", value, Self::MAX_LEN, |c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
        })
        .map(Self)
    }

    /// The alias as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

string_newtype_conversions!(AccountAlias);

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
        /// Which configured account to authenticate as, or the CLI's own default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<AccountAlias>,
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
        /// Which configured account to authenticate as, or the CLI's own default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<AccountAlias>,
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

    /// The same delegate, with its account pinned to `alias`.
    ///
    /// Used to record the account a consultation actually ran as, once a configured default has
    /// chosen one.
    /// Without it the record would say the caller named nothing, which is true but does not answer
    /// the question a reader of a surprising result asks: which identity paid for this.
    #[must_use]
    pub fn with_account(mut self, alias: AccountAlias) -> Self {
        match &mut self {
            Self::Claude { account, .. } | Self::Codex { account, .. } => *account = Some(alias),
        }
        self
    }

    /// The configured account this delegate authenticates as, if one was named.
    #[must_use]
    pub fn account(&self) -> Option<&AccountAlias> {
        match self {
            Self::Claude { account, .. } | Self::Codex { account, .. } => account.as_ref(),
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
                format!(
                    "claude {model} effort={effort} account={}",
                    describe_account(account.as_ref())
                )
            }
            Self::Codex {
                model,
                effort,
                sandbox,
                account,
            } => {
                format!(
                    "codex {model} effort={effort} sandbox={} account={}",
                    sandbox.as_str(),
                    describe_account(account.as_ref())
                )
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
    /// Environment the caller asked for on this consultation.
    ///
    /// Applied last, over the machine and account layers, so a one-off can override a standing
    /// setting.
    /// It may not name a credential or an endpoint: those decide which identity pays, and a
    /// request arriving from a delegating agent must not be able to redirect that.
    pub extra_env: &'a BTreeMap<String, String>,
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

/// Variables a Windows process needs before it can do anything at all.
///
/// Both delegates are Node programs, and a child started without `SystemRoot` cannot initialise
/// winsock, so it fails to reach the network rather than failing to authenticate.
/// `USERPROFILE` is Windows' `HOME`, which is how the CLIs find their own configuration.
#[cfg(windows)]
const PLATFORM_ALLOWLIST: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "windir",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "TEMP",
    "TMP",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// Nothing beyond [`BASE_ALLOWLIST`] is needed on a unix host.
#[cfg(not(windows))]
const PLATFORM_ALLOWLIST: &[&str] = &[];

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
        config: &Config,
    ) -> Result<Invocation, DelegateError> {
        let mut env: BTreeMap<String, String> = BASE_ALLOWLIST
            .iter()
            .chain(PLATFORM_ALLOWLIST.iter())
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

        // The machine-wide layer, before any account chooses an identity.
        config.launch.apply(&mut env, host_env);

        let args = match self {
            Self::Claude {
                model,
                effort,
                account,
            } => {
                apply_account(&mut env, Vendor::Claude, account.as_ref(), config, host_env)?;
                claude_args(model, effort, plan)
            }
            Self::Codex {
                model,
                effort,
                sandbox,
                account,
            } => {
                apply_account(&mut env, Vendor::Codex, account.as_ref(), config, host_env)?;
                codex_args(model, effort, *sandbox, plan)
            }
        };

        apply_request_env(&mut env, plan.extra_env)?;

        Ok(Invocation {
            program: self.vendor().program(),
            args,
            env,
        })
    }
}

/// The environment variables a vendor reads for credentials, in the order they are documented.
///
/// Forwarded from the host only when no account alias was chosen.
/// Choosing an alias means "authenticate as this identity", and a key exported in the launching
/// agent's own shell would otherwise win over it silently — the caller would believe it had
/// switched accounts while spending the other one.
fn credential_keys(vendor: Vendor) -> &'static [&'static str] {
    match vendor {
        Vendor::Claude => CLAUDE_CREDENTIAL_ALLOWLIST,
        Vendor::Codex => CODEX_CREDENTIAL_ALLOWLIST,
    }
}

/// Names of the variables an account's fields map onto, per vendor.
struct CredentialVars {
    config_dir: &'static str,
    api_key: &'static str,
    base_url: &'static str,
}

fn credential_vars(vendor: Vendor) -> CredentialVars {
    match vendor {
        Vendor::Claude => CredentialVars {
            config_dir: "CLAUDE_CONFIG_DIR",
            api_key: "ANTHROPIC_API_KEY",
            base_url: "ANTHROPIC_BASE_URL",
        },
        Vendor::Codex => CredentialVars {
            config_dir: "CODEX_HOME",
            api_key: "OPENAI_API_KEY",
            base_url: "OPENAI_BASE_URL",
        },
    }
}

/// Put one account's credentials into the child environment.
///
/// With no alias the host's own credential variables are forwarded and the CLI uses its default
/// configuration, which is what a machine with a single account of that vendor needs.
fn apply_account(
    env: &mut BTreeMap<String, String>,
    vendor: Vendor,
    alias: Option<&AccountAlias>,
    config: &Config,
    host_env: &BTreeMap<String, String>,
) -> Result<(), DelegateError> {
    // A caller that names no account gets the machine's or the checkout's chosen default, which is
    // the only thing a project file is allowed to say and therefore the only reason it exists.
    //
    // `selected_by` records the file that made the choice, so an unresolvable default sends the
    // reader to the file that named it rather than to their own tool call.
    let (alias, selected_by) = if let Some(alias) = alias {
        (alias.clone(), None)
    } else if let Some(name) = config.default_account(vendor) {
        (
            AccountAlias::parse(name)?,
            config
                .project_source
                .clone()
                .or_else(|| config.source.clone()),
        )
    } else {
        for key in credential_keys(vendor) {
            if let Some(value) = host_env.get(*key) {
                env.insert((*key).to_owned(), value.clone());
            }
        }
        return Ok(());
    };

    let account =
        config
            .account(vendor, alias.as_str())
            .ok_or_else(|| DelegateError::UnknownAccount {
                vendor,
                alias: alias.to_string(),
                configured: config.alias_names(vendor),
                config_path: config.source.clone(),
                selected_by,
            })?;

    if account.is_empty() {
        return Err(DelegateError::Environment(format!(
            "the {vendor} account `{alias}` is configured but empty. Give it a `config_dir`, an \
             `api_key`, an `api_key_env` or a `base_url`."
        )));
    }

    let vars = credential_vars(vendor);
    let home = host_env.get("HOME").map(std::path::PathBuf::from);

    if let Some(dir) = &account.config_dir {
        let resolved = crate::config::expand_tilde(dir, home.as_deref());
        // Checked here rather than left to the CLI: both vendors happily create a fresh config
        // directory and then report "Not logged in", which sends the caller looking at their
        // credentials instead of at the path that was wrong.
        if !resolved.is_dir() {
            return Err(DelegateError::AccountDirMissing {
                vendor,
                alias: alias.to_string(),
                path: resolved,
            });
        }
        env.insert(
            vars.config_dir.to_owned(),
            resolved.to_string_lossy().into_owned(),
        );
    }

    if let Some(key) = secret_value(
        account.api_key.as_ref(),
        account.api_key_env.as_deref(),
        host_env,
        vendor,
        &alias,
        "api_key",
    )? {
        env.insert(vars.api_key.to_owned(), key);
    }

    if let Some(token) = secret_value(
        account.auth_token.as_ref(),
        account.auth_token_env.as_deref(),
        host_env,
        vendor,
        &alias,
        "auth_token",
    )? {
        if vendor == Vendor::Codex {
            return Err(DelegateError::Environment(format!(
                "the codex account `{alias}` sets an auth token, which only the claude CLI reads. \
                 Use `api_key` or `api_key_env` instead."
            )));
        }
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), token);
    }

    if let Some(base) = &account.base_url {
        env.insert(vars.base_url.to_owned(), base.clone());
    }

    account.launch.apply(env, host_env);
    Ok(())
}

/// Apply the environment the caller asked for on this one consultation.
///
/// Refuses any name that decides which identity runs.
/// The caller here is frequently another model acting on text it was given, and a request that
/// could set `ANTHROPIC_BASE_URL` could send the account's credentials somewhere else.
/// Those names belong in the machine's own configuration, which no request can reach.
fn apply_request_env(
    env: &mut BTreeMap<String, String>,
    requested: &BTreeMap<String, String>,
) -> Result<(), DelegateError> {
    for (name, value) in requested {
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(DelegateError::Argument {
                field: "env",
                reason: "is not a usable environment variable name",
                value: name.clone(),
            });
        }
        if is_reserved(name) {
            return Err(DelegateError::Environment(format!(
                "`{name}` is part of how agentmux decides which account a consultation \
                 authenticates as, which program runs and how it reaches the network, so it \
                 cannot be set per request. Put it in an account or in `[launch]` in \
                 agentmux.toml, which no request can reach."
            )));
        }
        env.insert(name.clone(), value.clone());
    }
    Ok(())
}

/// Whether a variable name is agentmux's to decide rather than a caller's.
///
/// Derived from the allowlists rather than listed by hand, because a hand-kept denylist forgets
/// the name that was added last week — the same reason the child environment is an allowlist in
/// the first place.
///
/// The set is wider than credentials alone, and every part of it is load-bearing.
/// `PATH` selects which binary the child actually executes, so a caller able to set it runs a
/// program of its choosing with the resolved account's credentials in its environment.
/// `HOME` relocates the default account's identity.
/// The proxy and CA variables together route every request through a chosen host and make that
/// host's certificate trusted, which is credential capture without touching a credential.
///
/// Both vendors' names are refused regardless of which vendor is being launched, so the answer
/// does not depend on a host variable happening to be exported.
fn is_reserved(name: &str) -> bool {
    // A vendor's own namespace, refused wholesale rather than name by name.
    // `CLAUDE_CODE_USE_BEDROCK` reroutes a consultation to a caller-supplied Bedrock endpoint, and
    // matching the prefix closes that without agentmux having to learn what Bedrock is — the same
    // reason it keeps no roster of model identifiers.
    const RESERVED_PREFIXES: &[&str] = &[
        "ANTHROPIC_",
        "CLAUDE_",
        "OPENAI_",
        "CODEX_",
        "AWS_",
        "GOOGLE_",
        "GEMINI_",
        "VERTEX_",
        "AZURE_",
    ];

    BASE_ALLOWLIST.contains(&name)
        || PLATFORM_ALLOWLIST.contains(&name)
        || CLAUDE_CREDENTIAL_ALLOWLIST.contains(&name)
        || CODEX_CREDENTIAL_ALLOWLIST.contains(&name)
        || matches!(name, "TERM" | "NO_COLOR" | "CI")
        || RESERVED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        // A proxy variable is spelled either case, and both spellings are read.
        || name.to_ascii_uppercase().contains("PROXY")
}

/// Resolve a credential written into the file, or named as a host variable.
fn secret_value(
    literal: Option<&Secret>,
    from_env: Option<&str>,
    host_env: &BTreeMap<String, String>,
    vendor: Vendor,
    alias: &AccountAlias,
    field: &'static str,
) -> Result<Option<String>, DelegateError> {
    if let Some(name) = from_env {
        let value = host_env.get(name).ok_or_else(|| {
            DelegateError::Environment(format!(
                "the {vendor} account `{alias}` reads {field} from ${name}, which is not set in \
                 the environment agentmux was launched with."
            ))
        })?;
        return Ok(Some(value.clone()));
    }
    Ok(literal.map(|secret| secret.expose().to_owned()))
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
