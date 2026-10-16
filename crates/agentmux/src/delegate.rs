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
//!
//! One identifier may still be exchanged for another before it is launched, when the machine's
//! configuration says so — see [`crate::config::Config::rewritten_model`].
//! That is a substitution the operator wrote down in a file, not a roster this crate keeps: the
//! set of models agentmux knows about is still empty.
//! The same file may name a reasoning effort for a model, used only when the caller named none —
//! see [`crate::config::Config::default_effort`] — and when neither did, no effort is passed and
//! the CLI's own default runs.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{Account, ChosenBy, Config, DefaultAccount, Secret};

/// A delegate argument that could not be used.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DelegateError {
    /// A model identifier, reasoning effort, alias or environment name was empty, over-long, or
    /// not safe to hand to a child process.
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

    /// An option that belongs to one vendor was given for the other.
    #[error(
        "`{option}` belongs to the {owner} delegate, which is the only one that has it; drop it \
         for a {vendor} consultation"
    )]
    NotForVendor {
        /// The option that was set.
        option: &'static str,
        /// The vendor it belongs to.
        owner: Vendor,
        /// The vendor that was asked for.
        vendor: Vendor,
    },

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

    /// An account supplies a key, a token or an endpoint, and the consultation runs on a
    /// subscription.
    ///
    /// Refused rather than stripped: dropping the field silently would run the delegate as the
    /// CLI's own login, which is a different identity from the one the account names.
    #[error(
        "the {vendor} account `{alias}` sets `{field}`, which would run the delegate on API \
         pricing or against another endpoint, but this consultation runs on a subscription: \
         {why}. Name an account that authenticates with a logged-in subscription, or, if API \
         pricing is meant, mark this account `billing = \"api\"` in the machine's \
         agentmux.toml."
    )]
    ApiBillingNotAllowed {
        /// Which CLI the account belongs to.
        vendor: Vendor,
        /// The alias that was asked for.
        alias: String,
        /// The account field that names the credential or endpoint.
        field: &'static str,
        /// Why the consultation runs on a subscription.
        why: &'static str,
    },

    /// The caller asked for API pricing, which only an account the machine opted in may give.
    #[error(
        "this consultation asked for API pricing, but {why}. Leave `billing` unset to run on the \
         subscription, or name an account the machine's agentmux.toml marks \
         `billing = \"api\"`."
    )]
    ApiBillingNotOptedIn {
        /// Which account, or the absence of one, stands in the way.
        why: String,
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

/// Name the effort, or say that the CLI is choosing it, in the same words as for an account.
fn describe_effort(effort: Option<&Effort>) -> &str {
    effort.map_or("cli-default", Effort::as_str)
}

/// Name the file that chose an alias the caller did not ask for.
fn describe_selection(selected_by: Option<&std::path::Path>) -> String {
    selected_by.map_or_else(String::new, |path| {
        format!(", and {} selects it as the default", path.display())
    })
}

/// The isolation a consultation actually runs under.
///
/// An explicit choice wins; otherwise the account decides, because settings live in an account's
/// own configuration directory and "use my personal account, with its hooks" is one thought rather
/// than two.
/// With neither, the answer is isolation: it is the safe default and needs no configuration.
///
/// An account chosen by a *project* file does not get to decide.
/// Such a file arrives with a `git clone`, and while it may say which of the operator's accounts
/// pays, letting it also switch on that account's hooks and MCP servers would make a checkout a
/// third way to change isolation, which the operator was promised is imposed by argument alone.
fn resolve_isolation(
    explicit: Option<Isolation>,
    vendor: Vendor,
    alias: Option<&AccountAlias>,
    config: &Config,
) -> Isolation {
    if let Some(isolation) = explicit {
        return isolation;
    }
    let alias = match alias {
        Some(alias) => alias.as_str(),
        None => match config.default_account(vendor) {
            Some(DefaultAccount {
                chosen_by: ChosenBy::Project(_),
                ..
            })
            | None => return Isolation::Isolated,
            Some(default) => default.alias,
        },
    };
    let inherits = config
        .account(vendor, alias)
        .and_then(|account| account.inherit_settings)
        .unwrap_or(false);
    if inherits {
        Isolation::Inherit
    } else {
        Isolation::Isolated
    }
}

/// Name the isolation mode in a summary, but only when it is not the default.
///
/// A line that says `isolated` on every consultation teaches a reader to skip it, and the one
/// consultation where it matters is the one that inherited.
fn describe_isolation(isolation: Option<Isolation>) -> &'static str {
    match isolation {
        Some(Isolation::Inherit) => " settings=inherited",
        // Written out rather than wildcarded, so a third mode has to be considered here rather
        // than silently reading as isolated.
        None | Some(Isolation::Isolated) => "",
    }
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

/// Whether a delegate runs isolated, or as the account's own CLI would.
///
/// Isolation is the default and is what makes a consultation a second opinion rather than a second
/// copy of the caller's own setup: no settings, no hooks, no MCP servers.
/// It is imposed by argument, not by environment — no variable can switch it on or off.
///
/// `Inherit` exists because the default is sometimes wrong.
/// A reviewer that is supposed to exercise the project's own tooling needs that tooling, and a
/// hook the operator wants applied to every model they run is not usefully suppressed here.
/// It costs the three guarantees named on [`Self::Isolated`], and agentmux says so in the
/// transcript rather than letting a reopened turn look like a model changing its mind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// No user, project or local settings; no hooks; no MCP servers.
    ///
    /// Three things follow, and all three are lost by inheriting.
    /// A blocking `Stop` hook cannot reopen the finished turn and blank its result.
    /// The delegate cannot re-enter agentmux through a configured MCP server and recurse.
    /// And what the delegate saw does not depend on the caller's machine, so a consultation is
    /// reproducible somewhere else.
    #[default]
    Isolated,
    /// The account's own settings, hooks and MCP servers, exactly as its CLI would load them.
    Inherit,
}

impl Isolation {
    /// Whether the delegate loads the account's own configuration.
    #[must_use]
    pub fn inherits(self) -> bool {
        matches!(self, Self::Inherit)
    }
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

/// Whether a delegate's environment may carry billing credentials and endpoint overrides.
///
/// `Subscription` is the default and is what every launch gets unless the account it runs as opts
/// out in the machine's configuration.
/// Under it, every variable that makes a CLI bill per token or talk to another endpoint is removed
/// from the child's environment, a request may not set one, and an account that names a key, a
/// token or an endpoint is refused.
/// `Api` lifts exactly those restrictions, for an account the operator marked `billing = "api"`.
/// Neither says what the CLI's own configuration or login then does: that is the user's own
/// setup, and agentmux leaves it alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    /// The vendor's subscription, through the CLI's logged-in profile.
    #[default]
    Subscription,
    /// Per-token pricing, or an endpoint other than the vendor's own.
    Api,
}

impl Billing {
    /// The name used in configuration and in every report.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::Api => "api",
        }
    }
}

impl std::fmt::Display for Billing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which CLI a consultation is talking to, without the options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vendor {
    /// Anthropic's `claude` CLI.
    Claude,
    /// The `codex` CLI from `OpenAI`.
    Codex,
}

impl Vendor {
    /// Every vendor, for the places that fan out over all of them.
    ///
    /// The one list, so that a third vendor cannot be added to the enum and left out of `quota`,
    /// `accounts` or the server's roster: those iterate this rather than spelling the set again.
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    /// The executable name, as found on `PATH`.
    #[must_use]
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// This vendor's configuration directory, as its CLI resolves it from `env`.
    ///
    /// The directory the vendor's own variable names — `CLAUDE_CONFIG_DIR`, `CODEX_HOME` — or the
    /// CLI's default under the home directory when the variable is unset or empty.
    /// `None` when neither is known, which is a child with no home at all.
    #[must_use]
    pub(crate) fn config_dir(self, env: &BTreeMap<String, String>) -> Option<std::path::PathBuf> {
        let configured = env
            .get(credential_vars(self).config_dir)
            .filter(|dir| !dir.is_empty())
            .map(std::path::PathBuf::from);
        let default_dir = match self {
            Self::Claude => ".claude",
            Self::Codex => ".codex",
        };
        configured.or_else(|| Some(crate::config::home_dir(env)?.join(default_dir)))
    }

    /// Where this vendor's CLI saves a session for a later resume, as it resolves it from `env`.
    ///
    /// Claude keeps a transcript per session under `projects` in its configuration directory, and
    /// Codex a rollout under `sessions` in `CODEX_HOME`; a resume reads that file back.
    /// Nothing is read from it here: the directory is only named, so agentmux can ask whether the
    /// child will be able to write into it.
    #[must_use]
    pub(crate) fn session_store(
        self,
        env: &BTreeMap<String, String>,
    ) -> Option<std::path::PathBuf> {
        let store = match self {
            Self::Claude => "projects",
            Self::Codex => "sessions",
        };
        Some(self.config_dir(env)?.join(store))
    }
}

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.program())
    }
}

/// Who is being consulted, and on what terms.
///
/// Each variant carries only what that vendor accepts: a Claude consultation with a sandbox mode
/// does not compile.
/// The flat shapes a tool schema or a command line present are turned into this by
/// [`Delegate::from_parts`], which is where a vendor-specific option given for the other vendor
/// is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "vendor", rename_all = "snake_case")]
pub enum Delegate {
    /// Anthropic's CLI.
    Claude {
        /// Model identifier, forwarded verbatim.
        model: ModelId,
        /// Reasoning effort, forwarded verbatim, or the CLI's own default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
        /// Which configured account to authenticate as, or the CLI's own default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<AccountAlias>,
        /// Whether to load the account's own settings, or the account's preference when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        isolation: Option<Isolation>,
    },
    /// The `OpenAI` CLI.
    Codex {
        /// Model identifier, forwarded verbatim.
        model: ModelId,
        /// Reasoning effort, forwarded verbatim, or the CLI's own default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
        /// How much the delegate may write.
        #[serde(default)]
        sandbox: CodexSandbox,
        /// Which configured account to authenticate as, or the CLI's own default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<AccountAlias>,
        /// Whether to load the account's own settings, or the account's preference when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        isolation: Option<Isolation>,
    },
}

impl Delegate {
    /// Build a delegate from the flat shape a tool schema or a command line presents.
    ///
    /// This is the one parse step both front ends share, so they cannot come to accept
    /// different things.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::Argument`] when the model, effort or alias is not argv-safe, and
    /// [`DelegateError::NotForVendor`] when `sandbox` is given for a Claude consultation.
    pub fn from_parts(
        vendor: Vendor,
        model: &str,
        effort: Option<&str>,
        account: Option<&str>,
        isolation: Option<Isolation>,
        sandbox: Option<CodexSandbox>,
    ) -> Result<Self, DelegateError> {
        let model = ModelId::parse(model)?;
        let effort = effort.map(Effort::parse).transpose()?;
        let account = account.map(AccountAlias::parse).transpose()?;
        match vendor {
            Vendor::Claude => {
                if sandbox.is_some() {
                    return Err(DelegateError::NotForVendor {
                        option: "sandbox",
                        owner: Vendor::Codex,
                        vendor,
                    });
                }
                Ok(Self::Claude {
                    model,
                    effort,
                    account,
                    isolation,
                })
            }
            Vendor::Codex => Ok(Self::Codex {
                model,
                effort,
                sandbox: sandbox.unwrap_or_default(),
                account,
                isolation,
            }),
        }
    }

    /// Which CLI this delegate runs.
    #[must_use]
    pub fn vendor(&self) -> Vendor {
        match self {
            Self::Claude { .. } => Vendor::Claude,
            Self::Codex { .. } => Vendor::Codex,
        }
    }

    /// The isolation recorded on this delegate, once it has been pinned.
    #[must_use]
    pub fn isolation(&self) -> Option<Isolation> {
        match self {
            Self::Claude { isolation, .. } | Self::Codex { isolation, .. } => *isolation,
        }
    }

    /// The same delegate, with its isolation pinned.
    ///
    /// Recorded for the same reason the account is: an account may ask to inherit, and a run whose
    /// provenance stops being reproducible is the one run whose provenance most needs writing
    /// down.
    #[must_use]
    pub fn with_isolation(mut self, resolved: Isolation) -> Self {
        match &mut self {
            Self::Claude { isolation, .. } | Self::Codex { isolation, .. } => {
                *isolation = Some(resolved);
            }
        }
        self
    }

    /// The isolation this delegate would run under, given the machine's configuration.
    #[must_use]
    pub fn resolved_isolation(&self, config: &Config) -> Isolation {
        match self {
            Self::Claude {
                account, isolation, ..
            } => resolve_isolation(*isolation, Vendor::Claude, account.as_ref(), config),
            Self::Codex {
                account, isolation, ..
            } => resolve_isolation(*isolation, Vendor::Codex, account.as_ref(), config),
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

    /// The same delegate, with its model identifier replaced.
    ///
    /// Used where the machine's configuration rewrites the identifier a caller asked for, so that
    /// every later reader of the record — the launch, the transcript, the status — names the model
    /// that actually ran.
    #[must_use]
    pub fn with_model(mut self, replacement: ModelId) -> Self {
        match &mut self {
            Self::Claude { model, .. } | Self::Codex { model, .. } => *model = replacement,
        }
        self
    }

    /// The same delegate, with a reasoning effort filled in.
    ///
    /// Used where the caller named none and the machine's configuration names one for the model
    /// that runs, so the record says what the CLI was actually given.
    #[must_use]
    pub fn with_effort(mut self, chosen: Effort) -> Self {
        match &mut self {
            Self::Claude { effort, .. } | Self::Codex { effort, .. } => *effort = Some(chosen),
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

    /// The reasoning effort being requested, when one is.
    ///
    /// `None` once the record is pinned means neither the caller nor the machine's configuration
    /// named one, and the CLI's own default runs.
    #[must_use]
    pub fn effort(&self) -> Option<&Effort> {
        match self {
            Self::Claude { effort, .. } | Self::Codex { effort, .. } => effort.as_ref(),
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
                isolation,
            } => {
                format!(
                    "claude {model} effort={} account={}{}",
                    describe_effort(effort.as_ref()),
                    describe_account(account.as_ref()),
                    describe_isolation(*isolation)
                )
            }
            Self::Codex {
                model,
                effort,
                sandbox,
                account,
                isolation,
            } => {
                format!(
                    "codex {model} effort={} sandbox={} account={}{}",
                    describe_effort(effort.as_ref()),
                    sandbox.as_str(),
                    describe_account(account.as_ref()),
                    describe_isolation(*isolation)
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
    /// Only names the machine's configuration lists under `request_env` are accepted: a request
    /// arriving from a delegating agent must not be able to decide which identity pays, which
    /// program runs, or what that program loads before it reads a single setting.
    pub extra_env: &'a BTreeMap<String, String>,
    /// The pricing this consultation is pinned to, or `None` to let the account decide.
    ///
    /// [`Billing::Api`] is honoured only for an account the machine's configuration opts in;
    /// [`Billing::Subscription`] always is.
    pub billing: Option<Billing>,
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
    /// Executable name, resolved against the child's `PATH`, or a path to it.
    pub program: String,
    /// The complete argument vector, excluding `argv[0]`.
    pub args: Vec<String>,
    /// The child's entire environment, built as an allowlist from empty.
    pub env: BTreeMap<String, String>,
    /// The pricing the child may run on.
    ///
    /// Under [`Billing::Subscription`] the launcher scrubs the environment once more at the
    /// spawn itself, so no layer added between here and there can reintroduce a key.
    pub billing: Billing,
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

/// Marks a process as already running underneath agentmux.
///
/// Read before every launch to refuse a delegate spawned from inside one.
pub const DELEGATE_MARKER: &str = "AGENTMUX_DELEGATE";

/// Credential variables the Claude CLI reads.
///
/// Withheld when an account is named, so the account authenticates as itself rather than as
/// whatever key is exported.
/// With no account named they are not forwarded either, because every one of them is a
/// [billing name](is_billing_name) and a delegate without an account runs on its subscription.
///
/// `CLAUDE_CONFIG_DIR` is deliberately not among them, although it is where a login lives.
/// The process that launches agentmux is usually a Claude Code session, and that variable is how
/// such a session names its *own* identity; a delegate started from it must not quietly become a
/// second session on the same login.
/// A no-account Claude delegate therefore runs against the CLI's default directory, and any other
/// login is reached by naming an account.
/// Codex is the other way round: `CODEX_HOME` is forwarded, because a Codex session does not set
/// it for itself and it is the only way a relocated Codex login is found at all.
const CLAUDE_CREDENTIAL_ALLOWLIST: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
];

/// Credential variables the Codex CLI reads, including where its login is kept.
const CODEX_CREDENTIAL_ALLOWLIST: &[&str] = &["OPENAI_API_KEY", "OPENAI_BASE_URL", "CODEX_HOME"];

/// Variables that make a delegate CLI bill per token or talk to an endpoint other than its
/// vendor's subscription, beyond what the suffix rules in [`is_billing_name`] already catch.
///
/// Each is either a credential the CLI prefers over its logged-in profile, a switch onto a cloud
/// provider's metered API, or a header or endpoint override that can carry a key of its own.
const BILLING_NAMES: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CODEX_ACCESS_TOKEN",
];

/// Namespaces whose endpoint and token names reroute a delegate or authenticate it.
const BILLING_PREFIXES: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_",
    "OPENAI_",
    "AZURE_OPENAI_",
    "CODEX_",
];

/// Whether an environment name can put a delegate on API pricing or another endpoint.
///
/// Matched without regard to case, so a spelling a platform would read as the same variable
/// cannot slip through.
/// The rule is deliberately wider than either CLI's documented list: any `*_API_KEY` or
/// `*_API_KEY_FILE` name counts, because Codex reads a model provider's key from whatever name
/// its configuration gives it.
/// In every vendor namespace, any name with a `URL` segment counts, and so do `*_ISSUER` and
/// `*_AUTH_TOKEN`: an endpoint override decides where a login's own tokens are sent, which is how
/// `CODEX_REFRESH_TOKEN_URL_OVERRIDE` would hand a subscription's refresh token to another host.
/// A subscription token such as `CLAUDE_CODE_OAUTH_TOKEN` is not among them.
#[must_use]
pub fn is_billing_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let in_vendor_namespace = BILLING_PREFIXES
        .iter()
        .any(|prefix| upper.starts_with(prefix));
    BILLING_NAMES.contains(&upper.as_str())
        || upper.ends_with("_API_KEY")
        || upper.ends_with("_API_KEY_FILE")
        || (in_vendor_namespace
            && (upper.split('_').any(|segment| segment == "URL")
                || upper.ends_with("_ISSUER")
                || upper.ends_with("_AUTH_TOKEN")))
}

/// Remove every variable [`is_billing_name`] matches, and return the names removed.
///
/// The one scrub every spawn site applies to a child that runs under [`Billing::Subscription`]:
/// the delegate launcher, the Codex app-server driver, and both quota probes.
pub fn scrub_billing_env(env: &mut BTreeMap<String, String>) -> Vec<String> {
    let removed: Vec<String> = env
        .keys()
        .filter(|name| is_billing_name(name))
        .cloned()
        .collect();
    for name in &removed {
        env.remove(name);
    }
    removed
}

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
    /// Returns [`DelegateError::UnknownAccount`] for an alias the configuration does not define,
    /// [`DelegateError::AccountDirMissing`] for one whose directory is not there,
    /// [`DelegateError::ApiBillingNotAllowed`] for one that names a key, a token or an endpoint
    /// without opting into API pricing, [`DelegateError::Environment`] for an account
    /// that is empty, reads a secret from a host variable that is not set, or is otherwise
    /// unusable, and [`DelegateError::Argument`] or [`DelegateError::Environment`] for a request
    /// environment entry the configuration does not allow.
    pub fn invocation(
        &self,
        plan: &TurnPlan<'_>,
        host_env: &BTreeMap<String, String>,
        config: &Config,
    ) -> Result<Invocation, DelegateError> {
        let Identity {
            mut env,
            account,
            chosen_by_project,
            billing,
        } = resolve_identity(
            self.vendor(),
            self.account(),
            config,
            host_env,
            plan.billing,
        )?;
        let isolation = self.resolved_isolation(config);

        let args = match self {
            Self::Claude { model, effort, .. } => {
                claude_args(model, effort.as_ref(), plan, isolation)
            }
            Self::Codex {
                model,
                effort,
                sandbox,
                ..
            } => codex_args(model, effort.as_ref(), *sandbox, plan, isolation),
        };

        // An account's own `request_env` counts only when the account was the machine's choice
        // or the caller's.
        // A checkout that selects an account is allowed to say which account pays, and nothing
        // more; the names a request may set are policy the same checkout's instructions could
        // otherwise steer a caller into using.
        let allowed: Vec<&str> = config
            .launch
            .request_env
            .iter()
            .chain(
                account
                    .filter(|_| !chosen_by_project)
                    .into_iter()
                    .flat_map(|a| a.launch.request_env.iter()),
            )
            .map(String::as_str)
            .collect();
        apply_request_env(&mut env, plan.extra_env, &allowed, billing)?;

        // The recursion guard, set last so no layer can change even its value.
        // An isolated delegate cannot reach agentmux because it loads no MCP servers, but an
        // inheriting one loads the operator's own — and if agentmux is among them, a delegate can
        // consult a delegate until something runs out.
        // The marker makes the depth visible to the next agentmux, which refuses rather than
        // relying on isolation it may not have.
        env.insert(DELEGATE_MARKER.to_owned(), "1".to_owned());

        Ok(Invocation {
            program: self.vendor().program().to_owned(),
            args,
            env,
            billing,
        })
    }

    /// How this delegate's turns run over `codex app-server`, or `None` when they cannot.
    ///
    /// Only a Codex delegate that inherits its account's settings qualifies.
    /// The isolation every other consultation gets rests on `codex exec --ignore-user-config`,
    /// and `codex app-server` has no equivalent: codex-cli 0.159.3 refuses that flag there, its
    /// protocol schema has no field that skips `$CODEX_HOME/config.toml`, and a `-c` override
    /// merges into the user's tables rather than replacing them, as `codex_args` records.
    /// An isolated delegate therefore always runs over `exec`, where it cannot be steered; why a
    /// private `CODEX_HOME` is no way round that is in [`crate::appserver`].
    /// `codegen/app-server/generate.sh` refuses the first codex whose app-server takes the flag,
    /// which is when to pass it here and let isolated delegates through.
    #[must_use]
    pub fn app_server_settings(&self, config: &Config) -> Option<AppServerSettings> {
        let Self::Codex {
            model,
            effort,
            sandbox,
            ..
        } = self
        else {
            return None;
        };
        if !self.resolved_isolation(config).inherits() {
            return None;
        }
        Some(AppServerSettings {
            program: Vendor::Codex.program().to_owned(),
            args: codex_app_server_args(effort.as_ref()),
            model: model.clone(),
            sandbox: *sandbox,
        })
    }
}

/// What a Codex turn driven over `codex app-server` is started with.
///
/// The counterpart of the argument vector `codex_args` builds for `codex exec`, with the same
/// model, effort and sandbox; the rest of the plan — the working directory, the session to
/// resume, where the turn's files live — belongs to the run store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppServerSettings {
    /// The executable, resolved against the child's `PATH`.
    pub program: String,
    /// The arguments that start the server, effort included.
    pub args: Vec<String>,
    /// The model the thread runs, pinned on every turn as `exec` pins it.
    pub model: ModelId,
    /// How much the delegate may write.
    pub sandbox: CodexSandbox,
}

impl Delegate {
    /// The environment that decides which identity this delegate runs as.
    ///
    /// The machine's launch layer and the account's own, over the base every child starts from,
    /// and nothing a request adds.
    /// Anything that needs to know what a delegate would authenticate as — the quota probe, the
    /// search for a Codex session's own record — reads it from here, so it cannot disagree with
    /// the launch.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Delegate::invocation`] for the account.
    pub fn identity_env(
        &self,
        host_env: &BTreeMap<String, String>,
        config: &Config,
    ) -> Result<BTreeMap<String, String>, DelegateError> {
        Ok(resolve_identity(self.vendor(), self.account(), config, host_env, None)?.env)
    }

    /// The pricing this delegate runs on when a consultation asks for `requested`.
    ///
    /// Decided once, when a consultation starts, and pinned for its follow-ups, so a later turn
    /// cannot run on a pricing the first one was refused.
    ///
    /// # Errors
    ///
    /// Returns the account errors documented on [`Delegate::invocation`], and
    /// [`DelegateError::ApiBillingNotOptedIn`] when `requested` is [`Billing::Api`] and no
    /// account the machine opted in is in play.
    pub fn billing(
        &self,
        requested: Option<Billing>,
        host_env: &BTreeMap<String, String>,
        config: &Config,
    ) -> Result<Billing, DelegateError> {
        Ok(resolve_identity(self.vendor(), self.account(), config, host_env, requested)?.billing)
    }
}

/// The layers below a request: who the delegate is, and the environment that makes it so.
pub(crate) struct Identity<'c> {
    /// The environment, complete but for the request's own names and the recursion marker.
    pub(crate) env: BTreeMap<String, String>,
    /// The account that was applied, when one was.
    pub(crate) account: Option<&'c Account>,
    /// Whether that account was the checkout's choice rather than the machine's or the caller's.
    pub(crate) chosen_by_project: bool,
    /// The pricing the delegate may run on.
    pub(crate) billing: Billing,
}

/// Resolve the identity a delegate of one vendor runs as, with or without a named account.
///
/// # Errors
///
/// Returns the account errors documented on [`Delegate::invocation`].
pub(crate) fn resolve_identity<'c>(
    vendor: Vendor,
    alias: Option<&AccountAlias>,
    config: &'c Config,
    host_env: &BTreeMap<String, String>,
    requested: Option<Billing>,
) -> Result<Identity<'c>, DelegateError> {
    let mut env = base_env(host_env);
    // The machine-wide layer, before any account chooses an identity.
    config.launch.apply(&mut env, host_env);
    let selection = select_alias(vendor, alias, config)?;
    // Whether the checkout's file names this alias as the default, whoever else did: a
    // consultation records the alias it resolved to, so a follow-up asks for it by name, and a
    // caller told by the checkout's own instructions to name it is the checkout's choice too.
    let chosen_by_project = selection.as_ref().is_some_and(|selection| {
        matches!(
            config.default_account(vendor),
            Some(DefaultAccount {
                alias,
                chosen_by: ChosenBy::Project(_),
            }) if alias == selection.alias.as_str()
        )
    });
    let (account, billing) = apply_account(
        &mut env,
        vendor,
        selection,
        config,
        host_env,
        chosen_by_project,
        requested,
    )?;
    // Whatever the machine's or the account's launch layer forwarded or set, a subscription
    // delegate carries no key and no endpoint override.
    if billing == Billing::Subscription {
        let removed = scrub_billing_env(&mut env);
        if !removed.is_empty() {
            tracing::warn!(
                %vendor,
                ?removed,
                "withholding variables that would bill per token; the account does not opt into \
                 `billing = \"api\"`",
            );
        }
    }
    Ok(Identity {
        env,
        account,
        chosen_by_project,
        billing,
    })
}

/// The environment every child starts from: the host's path, home, locale, proxy and CA settings,
/// plus the marks of a non-interactive terminal.
///
/// Built as an allowlist from empty rather than by filtering a denylist.
/// A denylist forgets the variable that was added last week; an allowlist cannot leak a variable
/// nobody thought about.
/// Shared with the quota probes, so a delegate and a probe that must reach the same network
/// cannot quietly diverge in what they are given to reach it with.
#[must_use]
pub(crate) fn base_env(host_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    // Looked up and carried under the platform's own spelling of each name, which is how the
    // captured host environment spells them too.
    let mut env: BTreeMap<String, String> = BASE_ALLOWLIST
        .iter()
        .chain(PLATFORM_ALLOWLIST.iter())
        .map(|key| crate::config::canonical_env_name(key))
        .filter_map(|key| host_env.get(&key).map(|value| (key, value.clone())))
        .collect();
    // A delegate is a non-interactive child with no terminal.
    // Saying so keeps CLIs from emitting cursor control sequences and colour into the file
    // that captures their event stream, and `CI` is the conventional way to ask a tool for
    // its non-interactive behaviour.
    env.insert("TERM".to_owned(), "dumb".to_owned());
    env.insert("NO_COLOR".to_owned(), "1".to_owned());
    env.insert("CI".to_owned(), "1".to_owned());
    env
}

/// The environment variables a vendor reads for its identity, in the order they are documented.
///
/// Forwarded from the host only when no account alias was chosen, and then only those that are
/// not [billing names](is_billing_name).
/// Choosing an alias means "authenticate as this identity", and a key exported in the launching
/// agent's own shell would otherwise win over it silently — the caller would believe it had
/// switched accounts while spending the other one.
pub(crate) fn credential_keys(vendor: Vendor) -> &'static [&'static str] {
    match vendor {
        Vendor::Claude => CLAUDE_CREDENTIAL_ALLOWLIST,
        Vendor::Codex => CODEX_CREDENTIAL_ALLOWLIST,
    }
}

/// Whether a name is one either CLI reads a credential from, which a named account withholds.
#[must_use]
pub fn is_credential_name(name: &str) -> bool {
    Vendor::ALL
        .iter()
        .any(|vendor| credential_keys(*vendor).contains(&name))
}

/// Names of the variables an account's fields map onto, per vendor.
pub(crate) struct CredentialVars {
    /// The configuration directory, and so the login: `CLAUDE_CONFIG_DIR` or `CODEX_HOME`.
    pub(crate) config_dir: &'static str,
    /// The API key.
    ///
    /// Preferred over a stored login by Claude, but not by Codex, which uses the `chatgpt` login
    /// its `auth.json` records even when this is exported.
    pub(crate) api_key: &'static str,
    /// A bearer token, which only the Claude CLI reads.
    pub(crate) auth_token: Option<&'static str>,
    /// The endpoint, whose presence means the vendor's own window is not what is being spent.
    pub(crate) base_url: &'static str,
}

pub(crate) fn credential_vars(vendor: Vendor) -> CredentialVars {
    match vendor {
        Vendor::Claude => CredentialVars {
            config_dir: "CLAUDE_CONFIG_DIR",
            api_key: "ANTHROPIC_API_KEY",
            auth_token: Some("ANTHROPIC_AUTH_TOKEN"),
            base_url: "ANTHROPIC_BASE_URL",
        },
        Vendor::Codex => CredentialVars {
            config_dir: "CODEX_HOME",
            api_key: "OPENAI_API_KEY",
            auth_token: None,
            base_url: "OPENAI_BASE_URL",
        },
    }
}

/// Who chose the alias a launch resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chooser {
    Caller,
    Machine,
    Project,
}

/// Which alias a launch resolves to, and who chose it.
struct Selection {
    alias: AccountAlias,
    chooser: Chooser,
}

/// The alias a launch resolves to: the caller's, else the configured default, else none.
///
/// A caller that names no account gets the machine's or the checkout's chosen default, which is
/// the only thing a project file is allowed to say and therefore the only reason it exists.
fn select_alias(
    vendor: Vendor,
    requested: Option<&AccountAlias>,
    config: &Config,
) -> Result<Option<Selection>, DelegateError> {
    if let Some(alias) = requested {
        return Ok(Some(Selection {
            alias: alias.clone(),
            chooser: Chooser::Caller,
        }));
    }
    let Some(default) = config.default_account(vendor) else {
        return Ok(None);
    };
    Ok(Some(Selection {
        alias: AccountAlias::parse(default.alias)?,
        chooser: match default.chosen_by {
            ChosenBy::Machine(_) => Chooser::Machine,
            ChosenBy::Project(_) => Chooser::Project,
        },
    }))
}

/// Put one account's credentials into the child environment.
///
/// With no alias the CLI uses its default configuration, which is what a machine with a single
/// account of that vendor needs; only the host's pointer to a relocated Codex login is
/// forwarded, because no account has opted into API pricing and a key would outrank that login.
/// Returns the account that was applied, when one was, and the pricing the delegate runs on.
fn apply_account<'c>(
    env: &mut BTreeMap<String, String>,
    vendor: Vendor,
    selection: Option<Selection>,
    config: &'c Config,
    host_env: &BTreeMap<String, String>,
    chosen_by_project: bool,
    requested: Option<Billing>,
) -> Result<(Option<&'c Account>, Billing), DelegateError> {
    let Some(Selection { alias, chooser }) = selection else {
        if requested == Some(Billing::Api) {
            return Err(DelegateError::ApiBillingNotOptedIn {
                why: format!(
                    "no {vendor} account is named, and only an account can opt into API pricing"
                ),
            });
        }
        for key in credential_keys(vendor) {
            if is_billing_name(key) {
                continue;
            }
            if let Some(value) = host_env.get(*key) {
                env.insert((*key).to_owned(), value.clone());
            }
        }
        return Ok((None, Billing::Subscription));
    };
    // Withholding is the other half of the forwarding above, not an absence.
    // The machine's `[launch]` layer runs before this and may forward the host's own key; left
    // in place it would outrank the account's login and the caller would believe it had switched
    // accounts while spending the other one.
    for key in credential_keys(vendor) {
        env.remove(*key);
    }

    // `selected_by` names the file that made the choice, so an unresolvable default sends the
    // reader to the file that named it rather than to their own tool call.
    let selected_by = match chooser {
        Chooser::Project => config.project_source.clone(),
        Chooser::Machine => config.source.clone(),
        Chooser::Caller => None,
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

    // The opt-in is the machine file's, for this account, and a checkout that selected the
    // account does not carry it along: a file that arrives with a clone may say which account
    // pays, never that it pays per token.
    // A caller may ask for less than the account allows, never for more.
    let opted_in = account.billing == Some(Billing::Api);
    let billing = match (requested, opted_in && !chosen_by_project) {
        (Some(Billing::Api) | None, true) => Billing::Api,
        (Some(Billing::Api), false) => {
            return Err(DelegateError::ApiBillingNotOptedIn {
                why: if opted_in {
                    format!(
                        "the {vendor} account `{alias}` was selected by a checkout's \
                         agentmux.toml, which never runs on API pricing"
                    )
                } else {
                    format!(
                        "the {vendor} account `{alias}` does not set `billing = \"api\"` in the \
                         machine's agentmux.toml"
                    )
                },
            });
        }
        (Some(Billing::Subscription), _) | (None, false) => Billing::Subscription,
    };
    if billing == Billing::Subscription
        && let Some(field) = account.api_credential_field()
    {
        return Err(DelegateError::ApiBillingNotAllowed {
            vendor,
            alias: alias.to_string(),
            field,
            why: if requested == Some(Billing::Subscription) && opted_in {
                "the consultation asked for subscription pricing"
            } else if opted_in {
                "a checkout's agentmux.toml selected the account, and that never runs on API \
                 pricing"
            } else {
                "the account does not set `billing = \"api\"` in the machine's agentmux.toml"
            },
        });
    }

    apply_credentials(env, vendor, &alias, account, host_env)?;
    account.launch.apply(env, host_env);
    Ok((Some(account), billing))
}

/// Map one account's fields onto the variables its CLI reads.
fn apply_credentials(
    env: &mut BTreeMap<String, String>,
    vendor: Vendor,
    alias: &AccountAlias,
    account: &Account,
    host_env: &BTreeMap<String, String>,
) -> Result<(), DelegateError> {
    let vars = credential_vars(vendor);
    let home = crate::config::home_dir(host_env);

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
        alias,
        "api_key",
    )? {
        env.insert(vars.api_key.to_owned(), key);
    }

    if let Some(token) = secret_value(
        account.auth_token.as_ref(),
        account.auth_token_env.as_deref(),
        host_env,
        vendor,
        alias,
        "auth_token",
    )? {
        let Some(name) = vars.auth_token else {
            return Err(DelegateError::Environment(format!(
                "the {vendor} account `{alias}` sets an auth token, which only the claude CLI \
                 reads. Use `api_key` or `api_key_env` instead."
            )));
        };
        env.insert(name.to_owned(), token);
    }

    if let Some(base) = &account.base_url {
        env.insert(vars.base_url.to_owned(), base.clone());
    }
    Ok(())
}

/// Apply the environment the caller asked for on this one consultation.
///
/// Accepts only names the machine's configuration lists under `request_env`, and nothing by
/// default.
/// The caller here is frequently another model acting on text it was given.
/// A request that could set `ANTHROPIC_BASE_URL` could send the account's credentials somewhere
/// else; one that could set `PATH` could run a program of its choosing with them; one that could
/// set `NODE_OPTIONS` or `LD_PRELOAD` could run code inside the CLI before it reads a single
/// setting.
/// No list of names to refuse stays complete against that, so the operator lists the names to
/// accept instead, in a file no request can reach.
///
/// Under [`Billing::Subscription`] a name that would bill per token is refused whatever the
/// allowlist says, so the machine file's account-level opt-in stays the only way onto API
/// pricing.
fn apply_request_env(
    env: &mut BTreeMap<String, String>,
    requested: &BTreeMap<String, String>,
    allowed: &[&str],
    billing: Billing,
) -> Result<(), DelegateError> {
    for (name, value) in requested {
        crate::config::check_env_name(name).map_err(|reason| DelegateError::Argument {
            field: "env",
            reason,
            value: name.clone(),
        })?;
        if value.contains('\0') {
            return Err(DelegateError::Argument {
                field: "env",
                reason: "has a value containing a NUL byte, which no environment can carry",
                value: name.clone(),
            });
        }
        if billing == Billing::Subscription && is_billing_name(name) {
            return Err(DelegateError::Environment(format!(
                "`{name}` cannot be set per request: it would run the delegate on API pricing or \
                 against another endpoint, which agentmux does only for an account the machine's \
                 agentmux.toml marks `billing = \"api\"`."
            )));
        }
        // Spelled the way the platform reads it, like every name in the allowlist, so `path`
        // cannot slip past an allowlist that says `PATH` is not on it where the two are one
        // variable.
        let canonical = crate::config::canonical_env_name(name);
        if !allowed.contains(&canonical.as_str()) {
            return Err(DelegateError::Environment(format!(
                "`{name}` cannot be set per request: a request may only set the names \
                 agentmux.toml lists under `request_env`{}. Standing settings belong in an \
                 account or in `[launch]`, which no request can reach.",
                if allowed.is_empty() {
                    ", and this machine lists none".to_owned()
                } else {
                    format!(" ({})", allowed.join(", "))
                }
            )));
        }
        env.insert(canonical, value.clone());
    }
    Ok(())
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
fn claude_args(
    model: &ModelId,
    effort: Option<&Effort>,
    plan: &TurnPlan<'_>,
    isolation: Isolation,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.push("--print".to_owned());
    if let Some(session) = plan.resume {
        args.push("--resume".to_owned());
        args.push(session.as_str().to_owned());
    }
    args.push("--model".to_owned());
    args.push(model.as_str().to_owned());
    // Absent when nobody named one: the flag would otherwise have to carry a value agentmux made
    // up, and the CLI's own default is the one thing that is never out of date.
    if let Some(effort) = effort {
        args.push("--effort".to_owned());
        args.push(effort.as_str().to_owned());
    }
    args.push("--permission-mode".to_owned());
    args.push("plan".to_owned());
    args.push("--tools".to_owned());
    args.push(CLAUDE_READ_ONLY_TOOLS.to_owned());
    args.push("--output-format".to_owned());
    args.push("stream-json".to_owned());
    args.push("--verbose".to_owned());
    // Plan mode and the read-only tool list are not part of isolation and stay either way: what
    // the delegate may *do* is a separate question from whose configuration it loads.
    if !isolation.inherits() {
        args.push("--setting-sources".to_owned());
        args.push(String::new());
        args.push("--strict-mcp-config".to_owned());
        args.push("--mcp-config".to_owned());
        args.push(EMPTY_MCP_CONFIG.to_owned());
    }
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
    effort: Option<&Effort>,
    sandbox: CodexSandbox,
    plan: &TurnPlan<'_>,
    isolation: Isolation,
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
    // Absent when nobody named one, for the same reason as Claude's `--effort`.
    if let Some(effort) = effort {
        args.push("--config".to_owned());
        args.push(format!("model_reasoning_effort=\"{effort}\""));
    }
    if !isolation.inherits() {
        args.push("--ignore-user-config".to_owned());
        // Redundant with `--ignore-user-config` for config-file hooks, and cheap insurance against
        // a hook reaching the delegate by any other route.
        args.push("--config".to_owned());
        args.push("features.hooks=false".to_owned());
    }
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

/// Argument vector that starts `codex app-server` for one turn.
///
/// Only the effort travels as an argument, exactly as `exec` passes it, so it applies to every
/// request the server handles; the model, the sandbox and the approval policy are fields of the
/// `thread/start` or `thread/resume` request instead.
/// There is no isolation flag to pass: see [`Delegate::app_server_settings`] for why only an
/// inheriting delegate runs this way.
fn codex_app_server_args(effort: Option<&Effort>) -> Vec<String> {
    let mut args = vec!["app-server".to_owned()];
    if let Some(effort) = effort {
        args.push("--config".to_owned());
        args.push(format!("model_reasoning_effort=\"{effort}\""));
    }
    args
}
