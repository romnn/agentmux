//! The closed set of delegates: the header every vendor shares, each vendor's permissions, and
//! the environment a launch starts from.
//!
//! This module is the enforcement the whole crate exists for.
//! A caller does not compose a command line; it fills in a [`Delegate`], and its vendor's module
//! under [`crate::vendor`] builds the launch from it as a total function over [`Permissions`].
//! There is no path through [`Delegate::invocation`], or through a run store's launch, that omits
//! the streaming flags, the permission flags or the recursion marker, because none of them is
//! conditional, or that skips the machine file's limits, because the delegate is held to them
//! before anything is built.
//! Only the isolation flags depend on a choice, and that choice is recorded with the
//! consultation.
//!
//! An enum of vendors' permissions, not a trait: the set is closed, and an exhaustive `match` on
//! [`Permissions`] makes the compiler enumerate every place another vendor would need handling.
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
//! configuration says so — see [`crate::config::ModelRules::resolve`].
//! That is a substitution the operator wrote down in a file, not a roster this crate keeps: the
//! set of models agentmux knows about is still empty.
//! The same file may name a reasoning effort for a model, used only when the caller named none —
//! see [`crate::config::ModelRules::resolve`] — and when neither did, no effort is passed and
//! the CLI's own default runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use strum::VariantArray;

use crate::config::{Account, ChosenBy, Config, Secret};
use crate::launch::Launcher;
use crate::run::LaunchMode;
use crate::run::layout::TurnDir;
use crate::vendor::agy::{self, agy_args, agy_user_rules};
use crate::vendor::claude::{
    self, ClaudePermissionMode, ClaudePermissions, ClaudeTool, claude_args,
};
use crate::vendor::codex::{self, CodexPermissions, CodexSandbox, codex_args};

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

    /// A request spelled its isolation twice, once as `ignore_user_config` and once as its inverse
    /// `inherit_settings`, and the two disagree.
    #[error(
        "Conflicting settings: ignore_user_config={ignore_user_config} and \
         inherit_settings={inherit_settings}. Supply only one spelling, or matching inverse values."
    )]
    ConflictingSettings {
        /// What `ignore_user_config` said.
        ignore_user_config: bool,
        /// What `inherit_settings` said.
        inherit_settings: bool,
    },

    /// The machine file's limits refuse what the consultation asked for.
    #[error(transparent)]
    Permission(#[from] crate::config::PermissionRefusal),

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
        alias: AccountAlias,
        /// Every alias that vendor does have.
        configured: Vec<AccountAlias>,
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
        alias: AccountAlias,
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
    ApiOnlyFieldOnSubscription {
        /// Which CLI the account belongs to.
        vendor: Vendor,
        /// The alias that was asked for.
        alias: AccountAlias,
        /// The account field that names the credential or endpoint.
        field: &'static str,
        /// Why the consultation runs on a subscription.
        why: &'static str,
    },

    /// An isolated agy delegate was built outside the run store, which keeps the home it runs in.
    #[error(
        "an isolated agy delegate runs in the home its run store keeps, so it can only be launched \
         through the store"
    )]
    AgyHomeUnknown,

    /// agy's own settings file exists but cannot be read, and its permission rules have to be
    /// carried into the launch.
    #[error(
        "cannot launch agy: agentmux carries the deny and ask rules of {path} into every agy \
         launch, because the settings it passes replace them, and that file cannot be read: \
         {reason}. Fix the file or move it aside, then ask again."
    )]
    AgySettingsUnreadable {
        /// The settings file.
        path: PathBuf,
        /// Why it could not be read or parsed.
        reason: String,
    },

    /// The caller asked for API pricing, which only an account the machine opted in may give.
    #[error(
        "this consultation asked for API pricing, but {why}. Leave `billing` unset to run on the \
         subscription, or name an account the machine's agentmux.toml marks \
         `billing = \"api\"`."
    )]
    ApiBillingNotPermitted {
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
fn describe_account(account: &AccountChoice) -> &str {
    match account {
        AccountChoice::Named(alias) => alias.as_str(),
        AccountChoice::CliLogin => "cli-default",
        AccountChoice::Default => "default",
    }
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
/// In order, the first that has an answer decides:
///
/// 1. the caller's explicit choice;
/// 2. an account a checkout chose, which always runs isolated, because a project file switches
///    nothing on (see [`AccountPolicy`]);
/// 3. the account's own `inherit_settings`, because settings live in an account's own
///    configuration directory and "use my personal account, with its hooks" is one thought
///    rather than two;
/// 4. the vendor's `ignore_user_config` default in the machine file;
/// 5. inheriting, unless the vendor's limits rule it out.
///
/// The machine file's own preferences cannot contradict its limits — that is refused when the
/// file is read — so only an explicit choice can be refused; see
/// [`crate::config::PermissionPolicy::hold_isolation`].
fn resolve_isolation(
    explicit: Option<Isolation>,
    vendor: Vendor,
    account: &AccountChoice,
    config: &Config,
) -> Isolation {
    if let Some(isolation) = explicit {
        return isolation;
    }
    // An account that cannot be resolved is refused by the launch, which says why.
    if let Ok(Some(policy)) = AccountPolicy::resolve(vendor, account, config) {
        if policy.is_project_selected() {
            return Isolation::Isolated;
        }
        if let Some(inherit) = policy.account.inherit_settings {
            return Isolation::ignoring_user_config(!inherit);
        }
    }
    config.permissions(vendor).default_isolation()
}

/// Name the isolation mode in a summary, but only when it loads the user's configuration.
///
/// That is what a consultation gets unless something chose otherwise, and it is still the one
/// worth naming on every line: a run that inherited is the one whose finished turn a hook can
/// reopen and whose answer depends on the machine.
fn describe_isolation(isolation: Option<Isolation>) -> &'static str {
    match isolation {
        Some(Isolation::Inherit) => " settings=inherited",
        // Written out rather than wildcarded, so a third mode has to be considered here rather
        // than silently reading as isolated.
        None | Some(Isolation::Isolated) => "",
    }
}

/// Describe which aliases exist, for an unknown-alias error.
fn describe_configured(configured: &[AccountAlias], source: Option<&std::path::Path>) -> String {
    if configured.is_empty() {
        return match source {
            Some(path) => format!("{} defines none for it.", path.display()),
            None => "No agentmux.toml was found, so no accounts are configured. Create one at \
                     ~/.config/agentmux/agentmux.toml, or omit `account` to use the CLI's own \
                     default."
                .to_owned(),
        };
    }
    let names = configured
        .iter()
        .map(AccountAlias::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    match source {
        Some(path) => format!("{} defines: {names}.", path.display()),
        None => format!("Configured: {names}."),
    }
}

pub(crate) fn parse_token(
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
    /// Well past anything any vendor ships.
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
            type Error = $crate::delegate::DelegateError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(&value)
            }
        }
        impl From<$ty> for String {
            fn from(value: $ty) -> Self {
                value.0
            }
        }
        // Padded, so a table can align a column of them.
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.pad(&self.0)
            }
        }
        impl PartialEq<str> for $ty {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }
    )+};
}
pub(crate) use string_newtype_conversions;
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
/// Isolation is what makes a consultation a second opinion rather than a second copy of the
/// caller's own setup: no settings, no hooks, no MCP servers.
/// It is imposed by argument, not by environment — no variable can switch it on or off — except
/// for agy, which has no such argument and runs in a home the run store keeps for it instead; see
/// `agy_args`.
/// The public spelling is `ignore_user_config`: `true` is [`Self::Isolated`], `false` is
/// [`Self::Inherit`].
///
/// `Inherit` is what a consultation gets when nobody chose, because a delegate that runs as the
/// owner's own CLI runs with the tools, permissions and providers the owner set up for it, and
/// what it may *do* is held separately, by the permissions every launch passes explicitly.
/// It costs the three guarantees named on [`Self::Isolated`], and agentmux says so in the
/// transcript rather than letting a reopened turn look like a model changing its mind.
/// `codex.defaults.ignore_user_config` and `claude.defaults.ignore_user_config` in the machine
/// file change what nobody choosing means, and `allow_user_config = false` among a vendor's limits
/// rules inheriting out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// No user, project or local settings; no hooks; no MCP servers.
    ///
    /// Three things follow, and all three are lost by inheriting.
    /// A blocking `Stop` hook cannot reopen the finished turn and blank its result.
    /// The delegate cannot re-enter agentmux through a configured MCP server and recurse.
    /// And what the delegate saw does not depend on the caller's machine, so a consultation is
    /// reproducible somewhere else.
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

    /// The isolation one request spells, from either of its two spellings.
    ///
    /// `ignore_user_config` is the canonical spelling and `inherit_settings` its inverse, kept so
    /// existing callers keep working; a request that gives both must give matching values.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::ConflictingSettings`] when the two spellings disagree.
    pub fn from_spellings(
        ignore_user_config: Option<bool>,
        inherit_settings: Option<bool>,
    ) -> Result<Option<Self>, DelegateError> {
        match (ignore_user_config, inherit_settings) {
            (Some(ignore), Some(inherit)) if ignore == inherit => {
                Err(DelegateError::ConflictingSettings {
                    ignore_user_config: ignore,
                    inherit_settings: inherit,
                })
            }
            (Some(ignore), _) => Ok(Some(Self::ignoring_user_config(ignore))),
            (None, Some(inherit)) => Ok(Some(Self::ignoring_user_config(!inherit))),
            (None, None) => Ok(None),
        }
    }

    /// The isolation `ignore_user_config = ignore` names.
    #[must_use]
    pub fn ignoring_user_config(ignore: bool) -> Self {
        if ignore {
            Self::Isolated
        } else {
            Self::Inherit
        }
    }

    /// Whether the delegate skips the user's own configuration, as `ignore_user_config` says it.
    #[must_use]
    pub fn ignores_user_config(self) -> bool {
        !self.inherits()
    }
}

/// The permission options a front end presents flat, for every vendor.
///
/// What [`Delegate::from_parts`] builds a request from, and what [`Delegate::with_options`]
/// applies to a recorded consultation as a follow-up's overrides; an absent value takes the
/// default there, and keeps the recorded choice here.
/// Each option belongs to one vendor except the isolation, and one given for another vendor is
/// refused rather than ignored.
/// agy has none of its own: its permissions are fixed, as `agy_args` describes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionOptions {
    /// Whether to skip the user's own configuration; see [`Isolation::from_spellings`].
    pub isolation: Option<Isolation>,
    /// The Codex sandbox mode.
    pub sandbox: Option<CodexSandbox>,
    /// Whether a Codex delegate's commands may reach the network.
    pub network_access: Option<bool>,
    /// The Claude permission mode.
    pub permission_mode: Option<ClaudePermissionMode>,
    /// The built-in Claude tools offered.
    pub tools: Option<Vec<ClaudeTool>>,
    /// The Claude calls preapproved.
    pub allowed_tools: Option<Vec<ClaudeTool>>,
    /// The Claude calls denied.
    pub disallowed_tools: Option<Vec<ClaudeTool>>,
}

impl PermissionOptions {
    /// The first option set that belongs to another vendor than `vendor`, by the name a caller
    /// spells it, with the vendor it belongs to.
    fn first_foreign(&self, vendor: Vendor) -> Option<(&'static str, Vendor)> {
        [
            ("sandbox", self.sandbox.is_some(), Vendor::Codex),
            (
                "network_access",
                self.network_access.is_some(),
                Vendor::Codex,
            ),
            (
                "permission_mode",
                self.permission_mode.is_some(),
                Vendor::Claude,
            ),
            ("tools", self.tools.is_some(), Vendor::Claude),
            (
                "allowed_tools",
                self.allowed_tools.is_some(),
                Vendor::Claude,
            ),
            (
                "disallowed_tools",
                self.disallowed_tools.is_some(),
                Vendor::Claude,
            ),
        ]
        .into_iter()
        .find_map(|(option, set, owner)| (set && owner != vendor).then_some((option, owner)))
    }

    /// Refuse an option that belongs to another vendor.
    fn refuse_foreign(&self, vendor: Vendor) -> Result<(), DelegateError> {
        match self.first_foreign(vendor) {
            Some((option, owner)) => Err(DelegateError::NotForVendor {
                option,
                owner,
                vendor,
            }),
            None => Ok(()),
        }
    }

    /// The permissions these options name for a `vendor` delegate, each one they leave out unset.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::NotForVendor`] when an option belongs to another vendor.
    fn for_vendor(self, vendor: Vendor) -> Result<Permissions, DelegateError> {
        self.refuse_foreign(vendor)?;
        Ok(match vendor {
            Vendor::Claude => Permissions::Claude(self.claude()),
            Vendor::Codex => Permissions::Codex(self.codex()),
            Vendor::Agy => Permissions::Agy,
        })
    }

    /// The Claude permissions these options name.
    fn claude(self) -> ClaudePermissions {
        let Self {
            permission_mode,
            tools,
            allowed_tools,
            disallowed_tools,
            ..
        } = self;
        ClaudePermissions {
            permission_mode,
            tools,
            allowed_tools,
            disallowed_tools,
        }
    }

    /// The Codex permissions these options name.
    fn codex(self) -> CodexPermissions {
        let Self {
            sandbox,
            network_access,
            ..
        } = self;
        CodexPermissions {
            sandbox,
            network_access,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, VariantArray)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
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

/// What a delegate may do, which is particular to its CLI: each variant is a vendor, carrying
/// that vendor's own permissions.
///
/// It is also where the roster of vendors is spelled, once: [`Vendor`] is derived from it, and
/// `Vendor::VARIANTS` is what `quota`, `accounts` and the server's roster fan out over, so a
/// vendor cannot be added here and left out there.
/// agy has no permission options: every launch runs under the fixed policy `agy_args` describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, strum::EnumDiscriminants)]
#[serde(tag = "vendor", rename_all = "snake_case")]
#[strum_discriminants(
    name(Vendor),
    doc = "Which CLI a consultation is talking to, without the options.",
    derive(Hash, PartialOrd, Ord, Serialize, Deserialize, VariantArray),
    serde(rename_all = "snake_case"),
    cfg_attr(feature = "clap", derive(clap::ValueEnum))
)]
pub enum Permissions {
    /// Anthropic's `claude` CLI.
    ///
    /// Also spelled `claude-code` on the command line, because the CLI and the harness that runs
    /// it are named differently, and `--deny` is written while thinking of the harness.
    #[strum_discriminants(cfg_attr(feature = "clap", value(alias = "claude-code")))]
    Claude(ClaudePermissions),
    /// The `codex` CLI from `OpenAI`.
    Codex(CodexPermissions),
    /// Google's Antigravity CLI, `agy`.
    Agy,
}

impl Vendor {
    /// The executable name, as found on `PATH`.
    #[must_use]
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Agy => "agy",
        }
    }

    /// This vendor's configuration directory, as its CLI resolves it from `env`.
    ///
    /// The directory the vendor's own variable names — `CLAUDE_CONFIG_DIR`, `CODEX_HOME` — or the
    /// CLI's default under the home directory when the variable is unset or empty.
    /// agy has no such variable and always keeps it at `~/.gemini/antigravity-cli`.
    /// `None` when neither is known, which is a child with no home at all.
    #[must_use]
    pub(crate) fn config_dir(self, env: &BTreeMap<String, String>) -> Option<std::path::PathBuf> {
        let configured = credential_vars(self)
            .and_then(|vars| env.get(vars.config_dir))
            .filter(|dir| !dir.is_empty())
            .map(std::path::PathBuf::from);
        let default_dir = match self {
            Self::Claude => Path::new(".claude").to_path_buf(),
            Self::Codex => Path::new(".codex").to_path_buf(),
            Self::Agy => Path::new(".gemini").join("antigravity-cli"),
        };
        configured.or_else(|| Some(crate::config::home_dir(env)?.join(default_dir)))
    }

    /// Where this vendor's CLI saves a session for a later resume, as it resolves it from `env`.
    ///
    /// Claude keeps a transcript per session under `projects` in its configuration directory,
    /// Codex a rollout under `sessions` in `CODEX_HOME`, and agy a database per conversation under
    /// `conversations`; a resume reads that file back.
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
            Self::Agy => "conversations",
        };
        Some(self.config_dir(env)?.join(store))
    }
}

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.program())
    }
}

/// Which account a delegate authenticates as.
///
/// [`AccountChoice::Default`] is what a caller asks for by naming no account, and a consultation
/// resolves it once, when it starts, into one of the other two: a later configuration change
/// must not move a follow-up onto another identity, whose session it could not resume.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AccountChoice {
    /// The account the machine's or the checkout's configuration names as the default, or the
    /// CLI's own login when it names none.
    #[default]
    Default,
    /// The CLI's own login, with no configured account.
    CliLogin,
    /// This configured account.
    Named(AccountAlias),
}

impl AccountChoice {
    /// The configured account, when one is named.
    #[must_use]
    pub fn alias(&self) -> Option<&AccountAlias> {
        match self {
            Self::Named(alias) => Some(alias),
            Self::Default | Self::CliLogin => None,
        }
    }

    fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }
}

impl From<Option<AccountAlias>> for AccountChoice {
    fn from(alias: Option<AccountAlias>) -> Self {
        alias.map_or(Self::Default, Self::Named)
    }
}

/// How a record keeps an [`AccountChoice`]: absent for the default, the alias itself for a named
/// account — which is what every record written before the CLI login was pinned holds — and
/// `{"kind": "cli_login"}` for the CLI's own login, which no alias can spell.
mod stored_account {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{AccountAlias, AccountChoice};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum StoredAccountChoice {
        Named(AccountAlias),
        Tagged { kind: AccountChoiceTag },
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum AccountChoiceTag {
        CliLogin,
    }

    pub(super) fn serialize<S: Serializer>(
        choice: &AccountChoice,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match choice {
            AccountChoice::Named(alias) => {
                StoredAccountChoice::Named(alias.clone()).serialize(serializer)
            }
            AccountChoice::CliLogin => StoredAccountChoice::Tagged {
                kind: AccountChoiceTag::CliLogin,
            }
            .serialize(serializer),
            AccountChoice::Default => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<AccountChoice, D::Error> {
        Ok(
            match Option::<StoredAccountChoice>::deserialize(deserializer)? {
                None => AccountChoice::Default,
                Some(StoredAccountChoice::Named(alias)) => AccountChoice::Named(alias),
                Some(StoredAccountChoice::Tagged {
                    kind: AccountChoiceTag::CliLogin,
                }) => AccountChoice::CliLogin,
            },
        )
    }
}

/// Who is being consulted, and on what terms.
///
/// What every vendor shares is a field here, and what only one vendor accepts is in its
/// [`Permissions`]: a Claude consultation with a sandbox mode does not compile.
/// The flat shapes a tool schema or a command line present are turned into this by
/// [`Delegate::from_parts`], which is where a vendor-specific option given for another vendor is
/// refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegate {
    /// Model identifier, forwarded verbatim.
    ///
    /// Where the machine's configuration rewrites the identifier a caller asked for, the record
    /// names the one that ran, so every later reader of it — the launch, the transcript, the
    /// status — names the model that actually ran.
    pub model: ModelId,
    /// Reasoning effort, forwarded verbatim, or the CLI's own default when absent.
    ///
    /// Filled in when the caller named none and the machine's configuration names one for the
    /// model that runs, so the record says what the CLI was actually given; `None` once the
    /// record is pinned means neither named one, and the CLI's own default runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Which account to authenticate as.
    ///
    /// Pinned to the account a consultation actually ran as once a configured default has chosen
    /// one.
    /// Without that the record would say the caller named nothing, which is true but does not
    /// answer the question a reader of a surprising result asks: which identity paid for this.
    #[serde(
        default,
        skip_serializing_if = "AccountChoice::is_default",
        with = "stored_account"
    )]
    pub account: AccountChoice,
    /// Whether to load the account's own settings, or the configured default when absent.
    ///
    /// Pinned for the same reason the account is: whether a run loads the user's configuration
    /// decides whether it is reproducible, and a run whose provenance stops being reproducible is
    /// the one run whose provenance most needs writing down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
    /// Which CLI runs, and what it may do.
    #[serde(flatten)]
    pub permissions: Permissions,
}

impl Delegate {
    /// Build a delegate from the flat shape a tool schema or a command line presents.
    ///
    /// This is the one parse step both front ends share, so they cannot come to accept
    /// different things.
    /// What `options` leaves out stays unset, for [`crate::run::RunStore::start`] to fill from the
    /// machine file's defaults once it knows which configuration applies.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::Argument`] when the model, effort or alias is not argv-safe, and
    /// [`DelegateError::NotForVendor`] when `options` sets an option of another vendor.
    pub fn from_parts(
        vendor: Vendor,
        model: &str,
        effort: Option<&str>,
        account: Option<&str>,
        options: PermissionOptions,
    ) -> Result<Self, DelegateError> {
        Ok(Self {
            model: ModelId::parse(model)?,
            effort: effort.map(Effort::parse).transpose()?,
            account: AccountChoice::from(account.map(AccountAlias::parse).transpose()?),
            isolation: options.isolation,
            permissions: options.for_vendor(vendor)?,
        })
    }

    /// The same delegate, with every option `options` sets in place of the recorded one.
    ///
    /// What a follow-up's explicit overrides do: an option left out keeps the consultation's
    /// recorded choice, and the machine file's defaults are not consulted again.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::NotForVendor`] when `options` sets an option of another vendor.
    pub fn with_options(mut self, options: PermissionOptions) -> Result<Self, DelegateError> {
        options.refuse_foreign(self.vendor())?;
        self.isolation = options.isolation.or(self.isolation);
        self.permissions = match self.permissions {
            Permissions::Claude(recorded) => {
                Permissions::Claude(recorded.overridden_by(options.claude()))
            }
            Permissions::Codex(recorded) => {
                Permissions::Codex(recorded.overridden_by(options.codex()))
            }
            Permissions::Agy => Permissions::Agy,
        };
        Ok(self)
    }

    /// The same delegate, with the isolation and permissions a turn of it ran with.
    ///
    /// Only those are taken from `turn`: who is consulted — vendor, model, effort and account —
    /// is fixed when a consultation starts, so a turn's record cannot move a follow-up onto
    /// another.
    /// A record of another vendor's delegate carries no permissions this one has, and changes
    /// nothing.
    #[must_use]
    pub fn with_settings_of(mut self, turn: &Self) -> Self {
        if turn.vendor() == self.vendor() {
            self.isolation = turn.isolation;
            self.permissions.clone_from(&turn.permissions);
        }
        self
    }

    /// Which CLI this delegate runs.
    #[must_use]
    pub fn vendor(&self) -> Vendor {
        Vendor::from(&self.permissions)
    }

    /// The isolation this delegate would run under, given the machine's configuration.
    #[must_use]
    pub fn resolved_isolation(&self, config: &Config) -> Isolation {
        resolve_isolation(self.isolation, self.vendor(), &self.account, config)
    }

    /// The same delegate, with every permission the request left unset filled from the machine
    /// file's defaults, and from agentmux's own where the file names none.
    ///
    /// Done once, when a consultation starts, alongside the isolation: a follow-up keeps what was
    /// filled in here, so a default edited later never changes a consultation already running.
    #[must_use]
    pub fn with_default_permissions(mut self, config: &Config) -> Self {
        self.permissions = match self.permissions {
            Permissions::Claude(permissions) => {
                Permissions::Claude(config.claude.fill(&permissions))
            }
            Permissions::Codex(permissions) => Permissions::Codex(config.codex.fill(permissions)),
            Permissions::Agy => Permissions::Agy,
        };
        self
    }

    /// The same delegate, held to the machine file's current limits: what the next turn actually
    /// launches with.
    ///
    /// Checked before every turn, because a limit the owner tightened after a consultation began
    /// applies to its next turn too.
    /// Nothing is clamped: a choice beyond a limit is refused, naming the limit and its file.
    /// The one value filled in is the network of a Codex delegate whose limits forbid network and
    /// that nobody chose for, which is switched off explicitly rather than left to the user's own
    /// configuration.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::Permission`] when the isolation or a permission exceeds the
    /// limits.
    pub fn held_to_limits(&self, config: &Config) -> Result<Self, DelegateError> {
        config
            .permissions(self.vendor())
            .hold_isolation(self.resolved_isolation(config))?;
        let mut held = self.clone();
        match &mut held.permissions {
            Permissions::Claude(permissions) => {
                config.claude.hold(permissions, config.source.as_deref())?;
            }
            Permissions::Codex(permissions) => {
                *permissions = config.codex.hold(*permissions, config.source.as_deref())?;
            }
            Permissions::Agy => {}
        }
        Ok(held)
    }

    /// A one-line description, for `list` output and log lines.
    #[must_use]
    pub fn summary(&self) -> String {
        let Self {
            model,
            effort,
            account,
            isolation,
            permissions,
        } = self;
        let permissions = match permissions {
            Permissions::Claude(permissions) => permissions.describe(),
            Permissions::Codex(permissions) => format!(" sandbox={}", permissions.describe()),
            Permissions::Agy => String::new(),
        };
        format!(
            "{} {model} effort={}{permissions} account={}{}",
            self.vendor(),
            describe_effort(effort.as_ref()),
            describe_account(account),
            describe_isolation(*isolation)
        )
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
    /// Directories outside the working directory the delegate must be able to read.
    ///
    /// Claude and agy read only their working directory unless granted more, so each is passed
    /// as `--add-dir`; Codex's sandboxes read everywhere and need nothing.
    pub read_dirs: &'a [PathBuf],
}

/// The delegate CLI's own handle on a conversation, so a later turn can continue it.
///
/// Claude calls this a session id and Codex calls it a thread id; both are opaque to agentmux and
/// both are recovered by parsing the delegate's event stream.
///
/// Read back from disk through [`SessionRef::parse`] too, so a record edited or corrupted outside
/// agentmux cannot hand the CLI an identifier `parse` would refuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionRef(String);

impl SessionRef {
    /// Longest identifier accepted.
    /// Every vendor uses UUIDs; this leaves room for a thread name.
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

string_newtype_conversions!(SessionRef);

/// A fully prepared child process: what to run, with which arguments, in which environment.
///
/// Produced by [`Delegate::invocation`], or by a run store as each vendor prepares its launch, and
/// its fields are private, so the flags that make capture correct, the recursion marker and the
/// environment allowlist cannot be left out at a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    billing: Billing,
}

impl Invocation {
    /// Executable name, resolved against the child's `PATH`, or a path to it.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The complete argument vector, excluding `argv[0]`.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The child's entire environment, built as an allowlist from empty.
    #[must_use]
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// The pricing the child may run on.
    ///
    /// Under [`Billing::Subscription`] the launcher scrubs the environment once more at the
    /// spawn itself, so no layer added between here and there can reintroduce a key.
    #[must_use]
    pub fn billing(&self) -> Billing {
        self.billing
    }

    /// `vendor`'s CLI with `args`, in `env`, marked as a delegate.
    ///
    /// The recursion guard is set last, so no layer can change even its value.
    /// An isolated delegate cannot reach agentmux because it loads no MCP servers, but an
    /// inheriting one loads the operator's own — and if agentmux is among them, a delegate can
    /// consult a delegate until something runs out.
    /// The marker makes the depth visible to the next agentmux, which refuses rather than relying
    /// on isolation it may not have.
    pub(crate) fn of_delegate(
        vendor: Vendor,
        args: Vec<String>,
        mut env: BTreeMap<String, String>,
        billing: Billing,
    ) -> Self {
        env.insert(DELEGATE_MARKER.to_owned(), "1".to_owned());
        Self {
            program: vendor.program().to_owned(),
            args,
            env,
            billing,
        }
    }

    /// Tell the child where the run store is, so an agentmux it starts opens the same one.
    pub(crate) fn set_state_dir_env(&mut self, name: &str, dir: &Path) {
        self.env
            .insert(name.to_owned(), dir.to_string_lossy().into_owned());
    }

    /// Add `args` to the end of the argument vector.
    pub(crate) fn push_args(&mut self, args: &[&str]) {
        self.args.extend(args.iter().map(|arg| (*arg).to_owned()));
    }

    /// Run this executable's hidden `subcommand` on `plan_path` in place of the vendor's CLI, with
    /// the same environment and pricing: the app-server driver that is then the delegate, or the
    /// supervisor that starts it.
    pub(crate) fn set_driver_command(&mut self, driver: &Path, subcommand: &str, plan_path: &Path) {
        self.program = driver.to_string_lossy().into_owned();
        self.args = vec![
            subcommand.to_owned(),
            plan_path.to_string_lossy().into_owned(),
        ];
    }

    /// The delegate a supervisor starts: what its plan names, in the environment the supervisor
    /// was started with, which is the one the initiating process built.
    ///
    /// Nothing is resolved again, so the supervisor runs exactly what was recorded in
    /// `invocation.json`, and no credential needs to be written down for it.
    pub(crate) fn prepared(
        program: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        billing: Billing,
    ) -> Self {
        Self {
            program,
            args,
            env,
            billing,
        }
    }

    /// An invocation of anything at all, for a test of the launcher itself.
    #[doc(hidden)]
    #[must_use]
    pub fn for_launcher_test(
        program: &str,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        billing: Billing,
    ) -> Self {
        Self {
            program: program.to_owned(),
            args,
            env,
            billing,
        }
    }
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
/// [billing name](is_billing_env_name) and a delegate without an account runs on its subscription.
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
pub fn is_billing_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_API_KEY")
        || upper.ends_with("_API_KEY_FILE")
        || Vendor::VARIANTS
            .iter()
            .any(|vendor| is_vendor_billing_env_name(*vendor, &upper))
}

/// Whether `upper`, a name in upper case, is one of `vendor`'s own billing names, beyond what the
/// suffix rules in [`is_billing_env_name`] already catch: one it names outright, or an endpoint or
/// a token in its namespaces.
///
/// A name named outright is either a credential the CLI prefers over its logged-in profile, a
/// switch onto a cloud provider's metered API, or a header or endpoint override that can carry a
/// key of its own.
fn is_vendor_billing_env_name(vendor: Vendor, upper: &str) -> bool {
    match vendor {
        Vendor::Claude => {
            [
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_CUSTOM_HEADERS",
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
                "CLAUDE_CODE_USE_FOUNDRY",
                "AWS_BEARER_TOKEN_BEDROCK",
            ]
            .contains(&upper)
                || reroutes_within(upper, &["ANTHROPIC_", "CLAUDE_"])
        }
        Vendor::Codex => {
            upper == "CODEX_ACCESS_TOKEN"
                || reroutes_within(upper, &["OPENAI_", "AZURE_OPENAI_", "CODEX_"])
        }
        // agy's gateway, which routes it to another backend with headers and a key of its own, the
        // switch onto another of its logins, and the switch onto Google Cloud's application default
        // credentials with the file they are read from; named by its binary and its documentation.
        Vendor::Agy => [
            "AGY_ACCOUNT",
            "AGY_ADC_AUTH",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "AGY_LLM_GATEWAY_URL",
            "AGY_LLM_GATEWAY_MODELS",
            "AGY_LLM_GATEWAY_API_KEY",
            "AGY_LLM_GATEWAY_HEADERS",
        ]
        .contains(&upper),
    }
}

/// Whether `upper` is an endpoint or a token name in one of `namespaces`, the prefixes whose
/// endpoint and token names reroute a delegate or authenticate it.
fn reroutes_within(upper: &str, namespaces: &[&str]) -> bool {
    namespaces.iter().any(|prefix| upper.starts_with(prefix))
        && (upper.split('_').any(|segment| segment == "URL")
            || upper.ends_with("_ISSUER")
            || upper.ends_with("_AUTH_TOKEN"))
}

/// Remove every variable [`is_billing_env_name`] matches, and return the names removed.
///
/// The one scrub every spawn site applies to a child that runs under [`Billing::Subscription`]:
/// the delegate launcher, the Codex app-server driver, and both quota probes.
pub fn scrub_billing_env(env: &mut BTreeMap<String, String>) -> Vec<String> {
    let removed: Vec<String> = env
        .keys()
        .filter(|name| is_billing_env_name(name))
        .cloned()
        .collect();
    for name in &removed {
        env.remove(name);
    }
    removed
}

impl Delegate {
    /// Build the child process for one turn.
    ///
    /// `host_env` is the environment agentmux itself was launched with; only the variables named
    /// in the allowlists above are carried into the child.
    /// This is the delegate's CLI as a caller with no run store would run it; a store prepares
    /// its launches with the same arguments, and decides how each runs.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::UnknownAccount`] for an alias the configuration does not define,
    /// [`DelegateError::AccountDirMissing`] for one whose directory is not there,
    /// [`DelegateError::ApiOnlyFieldOnSubscription`] for one that names a key, a token or an
    /// endpoint without opting into API pricing, [`DelegateError::Environment`] for an account that
    /// is empty, reads a secret from a host variable that is not set, or is otherwise unusable, and
    /// [`DelegateError::Argument`] or [`DelegateError::Environment`] for a request environment
    /// entry the configuration does not allow, [`DelegateError::AgySettingsUnreadable`] for an
    /// agy delegate whose own settings file cannot be read, and [`DelegateError::AgyHomeUnknown`]
    /// for an isolated agy delegate, which only its run store can launch.
    pub fn invocation(
        &self,
        plan: &TurnPlan<'_>,
        host_env: &BTreeMap<String, String>,
        config: &Config,
    ) -> Result<Invocation, DelegateError> {
        let Admitted {
            delegate,
            env,
            billing,
        } = self.admitted(plan, host_env, config)?;
        let isolation = delegate.resolved_isolation(config);
        let Self { model, effort, .. } = &delegate;
        let args = match &delegate.permissions {
            Permissions::Claude(permissions) => {
                claude_args(model, effort.as_ref(), permissions, plan, isolation)
            }
            Permissions::Codex(permissions) => {
                codex_args(model, effort.as_ref(), *permissions, plan, isolation)
            }
            // The home an isolated agy delegate runs in is its run store's; see ADR 0035.
            Permissions::Agy if !isolation.inherits() => {
                return Err(DelegateError::AgyHomeUnknown);
            }
            // Built from the complete environment, whose home is where agy reads its own settings.
            Permissions::Agy => agy_args(model, effort.as_ref(), plan, &agy_user_rules(&env)?),
        };
        Ok(Invocation::of_delegate(
            delegate.vendor(),
            args,
            env,
            billing,
        ))
    }

    /// This delegate, held to the machine file's limits, with the environment and the pricing a
    /// launch of it starts from: its identity's layers and its request's, and nothing its vendor
    /// sets.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Delegate::invocation`] for the account, the limits and the
    /// request's environment.
    pub(crate) fn admitted(
        &self,
        plan: &TurnPlan<'_>,
        host_env: &BTreeMap<String, String>,
        config: &Config,
    ) -> Result<Admitted, DelegateError> {
        let Identity {
            mut env,
            policy,
            billing,
        } = resolve_identity(self.vendor(), &self.account, config, host_env, plan.billing)?;
        // Held here, the one step every launch a caller asks for goes through, so no turn runs
        // past a limit the owner set, however it was reached; an automatic resume continues a
        // turn that was held here, in the environment built here.
        let delegate = self.held_to_limits(config)?;

        let allowed: Vec<&str> = config
            .launch
            .request_env
            .iter()
            .chain(
                policy
                    .iter()
                    .flat_map(AccountPolicy::allowed_request_env_names),
            )
            .map(String::as_str)
            .collect();
        apply_request_env(&mut env, plan.extra_env, &allowed, billing)?;
        Ok(Admitted {
            delegate,
            env,
            billing,
        })
    }

    /// The isolation a launch of this delegate runs under once it is pinned: its own, or isolated
    /// for a record from before isolation was pinned.
    #[must_use]
    pub(crate) fn pinned_isolation(&self) -> Isolation {
        self.isolation.unwrap_or(Isolation::Isolated)
    }

    /// Prepare one turn's launch for a run store: the delegate's vendor builds all of it and says
    /// how it runs.
    ///
    /// `self` is held to the machine file's limits by whoever admitted the turn, and its
    /// [pinned isolation](Delegate::pinned_isolation) is the one the turn runs under.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::AgySettingsUnreadable`] for an agy delegate whose own settings
    /// file cannot be read, [`DelegateError::Environment`] when the home an isolated agy delegate
    /// runs in cannot be made, and [`DelegateError::Permission`] for a `codex exec` turn that
    /// needs a permission profile from a codex not verified to honour one.
    pub(crate) fn prepare(
        &self,
        request: LaunchRequest<'_>,
    ) -> Result<PreparedLaunch, DelegateError> {
        match &self.permissions {
            Permissions::Claude(permissions) => Ok(claude::prepare(self, permissions, request)),
            Permissions::Codex(permissions) => codex::prepare(self, *permissions, request),
            Permissions::Agy => agy::prepare(self, request),
        }
    }
}

/// A delegate admitted for one launch, and what that launch starts from.
pub(crate) struct Admitted {
    /// The delegate, held to the machine file's limits.
    pub(crate) delegate: Delegate,
    /// The environment of its identity and its request, which its vendor completes.
    pub(crate) env: BTreeMap<String, String>,
    /// The pricing it may run on.
    pub(crate) billing: Billing,
}

/// What a run store gives a delegate's vendor to prepare one turn's launch from.
pub(crate) struct LaunchRequest<'a> {
    /// The turn: its files, the session it resumes and the directories it reads.
    pub(crate) plan: &'a TurnPlan<'a>,
    /// The question, for a CLI that reads it as a record of its own.
    pub(crate) question: &'a str,
    /// The turn's directory, where a driver's plan is kept.
    pub(crate) turn: &'a TurnDir,
    /// The directory the delegate reads the project from, where its CLI is asked its version too.
    pub(crate) cwd: &'a Path,
    /// The environment the launch starts from: its identity's layers and its request's, or the
    /// one the turn an automatic resume continues ran in.
    pub(crate) env: BTreeMap<String, String>,
    /// The pricing the delegate may run on.
    pub(crate) billing: Billing,
    /// What asks the delegate's CLI for its version.
    pub(crate) launcher: &'a dyn Launcher,
    /// The run store's root, resolved, which the child is told and where agy's isolated home is.
    ///
    /// Told rather than left to whatever the host exported: an agentmux started underneath the
    /// delegate has to open the same store to recognise the delegate as its ancestor, and the
    /// platform default can differ between the two environments.
    pub(crate) store_root: &'a Path,
    /// Whether a supervisor starts the delegate, and so can hold its input.
    pub(crate) supervisor: bool,
    /// The agentmux executable that drives a Codex turn over `codex app-server`, when the store
    /// drives them.
    pub(crate) codex_driver: Option<&'a Path>,
    /// Whether every Codex turn of the consultation runs with `codex exec`; only Codex reads it.
    pub(crate) codex_exec: bool,
}

/// One turn's launch, as its vendor prepared it.
pub(crate) struct PreparedLaunch {
    /// What to run: the vendor's CLI, or the driver that stands in for it.
    pub(crate) invocation: Invocation,
    /// What the delegate reads on stdin.
    pub(crate) stdin: Stdin,
    /// The plan a driver runs under, serialised, which the store writes into the turn's directory.
    pub(crate) plan_file: Option<String>,
    /// How the delegate runs, which the store records and every later reader goes by.
    pub(crate) mode: LaunchMode,
    /// The CLI's version, as it printed it, or `None` when it could not say.
    pub(crate) cli_version: Option<String>,
    /// Where the launch keeps its configuration, when it is known.
    pub(crate) config_dir: Option<PathBuf>,
    /// Where it saves its session, which the store asks whether the child could write into.
    pub(crate) session_store: Option<PathBuf>,
}

/// What a delegate reads on stdin.
pub(crate) enum Stdin {
    /// The turn's question, as it was asked.
    Question,
    /// A record of the CLI's own carrying the question, which the store writes beside it.
    Record(String),
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
        Ok(resolve_identity(self.vendor(), &self.account, config, host_env, None)?.env)
    }

    /// The pricing this delegate runs on when a consultation asks for `requested`.
    ///
    /// Decided once, when a consultation starts, and pinned for its follow-ups, so a later turn
    /// cannot run on a pricing the first one was refused.
    ///
    /// # Errors
    ///
    /// Returns the account errors documented on [`Delegate::invocation`], and
    /// [`DelegateError::ApiBillingNotPermitted`] when `requested` is [`Billing::Api`] and no
    /// account the machine opted in is in play.
    pub fn resolve_billing(
        &self,
        requested: Option<Billing>,
        host_env: &BTreeMap<String, String>,
        config: &Config,
    ) -> Result<Billing, DelegateError> {
        Ok(resolve_identity(self.vendor(), &self.account, config, host_env, requested)?.billing)
    }
}

/// The layers below a request: who the delegate is, and the environment that makes it so.
pub(crate) struct Identity<'c> {
    /// The environment, complete but for the request's own names and the recursion marker.
    pub(crate) env: BTreeMap<String, String>,
    /// The account that was applied, when one was.
    pub(crate) policy: Option<AccountPolicy<'c>>,
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
    account: &'c AccountChoice,
    config: &'c Config,
    host_env: &BTreeMap<String, String>,
    requested: Option<Billing>,
) -> Result<Identity<'c>, DelegateError> {
    let mut env = base_env(host_env);
    // The machine-wide layer, before any account chooses an identity.
    config.launch.apply(&mut env, host_env);
    let policy = AccountPolicy::resolve(vendor, account, config)?;
    let billing = apply_account(&mut env, vendor, policy, host_env, requested)?;
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
        policy,
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
/// not [billing names](is_billing_env_name).
/// Choosing an alias means "authenticate as this identity", and a key exported in the launching
/// agent's own shell would otherwise win over it silently — the caller would believe it had
/// switched accounts while spending the other one.
pub(crate) fn credential_keys(vendor: Vendor) -> &'static [&'static str] {
    match vendor {
        Vendor::Claude => CLAUDE_CREDENTIAL_ALLOWLIST,
        Vendor::Codex => CODEX_CREDENTIAL_ALLOWLIST,
        // agy's login is in the operating system's keyring, and every variable it authenticates
        // with otherwise is a billing name.
        Vendor::Agy => &[],
    }
}

/// Whether a name is one any CLI reads a credential from, which a named account withholds.
#[must_use]
pub fn is_credential_name(name: &str) -> bool {
    Vendor::VARIANTS
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

/// The variables a Claude account's fields map onto.
pub(crate) const CLAUDE_CREDENTIAL_VARS: CredentialVars = CredentialVars {
    config_dir: "CLAUDE_CONFIG_DIR",
    api_key: "ANTHROPIC_API_KEY",
    auth_token: Some("ANTHROPIC_AUTH_TOKEN"),
    base_url: "ANTHROPIC_BASE_URL",
};

/// The variables a Codex account's fields map onto.
pub(crate) const CODEX_CREDENTIAL_VARS: CredentialVars = CredentialVars {
    config_dir: "CODEX_HOME",
    api_key: "OPENAI_API_KEY",
    auth_token: None,
    base_url: "OPENAI_BASE_URL",
};

/// The variables a vendor's account fields map onto, or `None` for agy, which reads neither its
/// login nor its configuration directory from a variable an account could set.
pub(crate) fn credential_vars(vendor: Vendor) -> Option<CredentialVars> {
    match vendor {
        Vendor::Claude => Some(CLAUDE_CREDENTIAL_VARS),
        Vendor::Codex => Some(CODEX_CREDENTIAL_VARS),
        Vendor::Agy => None,
    }
}

/// The account a launch runs as, and what whoever chose it lets it switch on.
///
/// A project file may say which of the operator's accounts pays, and nothing more.
/// So an account it chose runs isolated, never on API pricing, and with none of the request
/// environment names the machine file lists for it, whatever that file says: each is something
/// the same checkout's instructions could otherwise steer a caller into using.
/// An alias that is the checkout's default counts as its choice even when a caller names it,
/// because a consultation records the default it resolved by name and a follow-up asks for it
/// that way, and because a caller told by the checkout's instructions to name it is the
/// checkout's choice too.
/// This is the one place that rule is decided.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AccountPolicy<'a> {
    /// The alias.
    pub(crate) alias: &'a AccountAlias,
    /// The account the machine file defines under it.
    pub(crate) account: &'a Account,
    /// The file whose default the alias is, or `None` when only the caller named it.
    chosen_by: Option<ChosenBy<'a>>,
}

impl<'a> AccountPolicy<'a> {
    /// The account `choice` resolves to: the one named, else the configured default unless the
    /// CLI's own login was chosen, else none.
    ///
    /// A caller that names no account gets the machine's or the checkout's chosen default,
    /// which is the only thing a project file is allowed to say and therefore the only reason it
    /// exists.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::UnknownAccount`] for an alias the configuration does not define,
    /// and [`DelegateError::Environment`] for an account that is empty.
    pub(crate) fn resolve(
        vendor: Vendor,
        choice: &'a AccountChoice,
        config: &'a Config,
    ) -> Result<Option<Self>, DelegateError> {
        let default = config.default_account(vendor);
        let (alias, chosen_by) = match choice {
            AccountChoice::CliLogin => return Ok(None),
            AccountChoice::Named(alias) => (
                alias,
                default
                    .filter(|default| default.alias == alias)
                    .map(|default| default.chosen_by),
            ),
            AccountChoice::Default => match default {
                Some(default) => (default.alias, Some(default.chosen_by)),
                None => return Ok(None),
            },
        };
        let account =
            config
                .account(vendor, alias)
                .ok_or_else(|| DelegateError::UnknownAccount {
                    vendor,
                    alias: alias.clone(),
                    configured: config.account_aliases(vendor),
                    config_path: config.source.clone(),
                    // The file that made the choice, so an unresolvable default sends the reader
                    // to that file rather than to their own tool call.
                    selected_by: chosen_by
                        .and_then(ChosenBy::path)
                        .map(std::path::Path::to_path_buf),
                })?;
        if account.is_empty() {
            return Err(DelegateError::Environment(format!(
                "the {vendor} account `{alias}` is configured but empty. Give it a `config_dir`, \
                 an `api_key`, an `api_key_env` or a `base_url`."
            )));
        }
        Ok(Some(Self {
            alias,
            account,
            chosen_by,
        }))
    }

    /// Whether a checkout's file chose the account.
    fn is_project_selected(&self) -> bool {
        matches!(self.chosen_by, Some(ChosenBy::Project(_)))
    }

    /// Whether the machine file opted the account into API pricing, and nothing withdrew it.
    fn allows_api_billing(&self) -> bool {
        !self.is_project_selected() && self.account.billing == Some(Billing::Api)
    }

    /// The environment names the account lets a request set, beyond the machine's own.
    fn allowed_request_env_names(&self) -> &'a [String] {
        if self.is_project_selected() {
            &[]
        } else {
            &self.account.launch.request_env
        }
    }
}

/// Put one account's credentials into the child environment.
///
/// With no alias the CLI uses its default configuration, which is what a machine with a single
/// account of that vendor needs; only the host's pointer to a relocated Codex login is
/// forwarded, because no account has opted into API pricing and a key would outrank that login.
/// Returns the pricing the delegate runs on.
fn apply_account(
    env: &mut BTreeMap<String, String>,
    vendor: Vendor,
    policy: Option<AccountPolicy<'_>>,
    host_env: &BTreeMap<String, String>,
    requested: Option<Billing>,
) -> Result<Billing, DelegateError> {
    let Some(policy) = policy else {
        if requested == Some(Billing::Api) {
            return Err(DelegateError::ApiBillingNotPermitted {
                why: format!(
                    "no {vendor} account is named, and only an account can opt into API pricing"
                ),
            });
        }
        for key in credential_keys(vendor) {
            if is_billing_env_name(key) {
                continue;
            }
            if let Some(value) = host_env.get(*key) {
                env.insert((*key).to_owned(), value.clone());
            }
        }
        return Ok(Billing::Subscription);
    };
    let AccountPolicy { alias, account, .. } = policy;
    // Withholding is the other half of the forwarding above, not an absence.
    // The machine's `[launch]` layer runs before this and may forward the host's own key; left
    // in place it would outrank the account's login and the caller would believe it had switched
    // accounts while spending the other one.
    for key in credential_keys(vendor) {
        env.remove(*key);
    }

    // The opt-in is the machine file's, for this account, and a checkout that selected the
    // account does not carry it along: a file that arrives with a clone may say which account
    // pays, never that it pays per token.
    // A caller may ask for less than the account allows, never for more.
    let account_opts_into_api = account.billing == Some(Billing::Api);
    let billing = match (requested, policy.allows_api_billing()) {
        (Some(Billing::Api) | None, true) => Billing::Api,
        (Some(Billing::Api), false) => {
            return Err(DelegateError::ApiBillingNotPermitted {
                why: if account_opts_into_api {
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
        && let Some(field) = account.api_only_field()
    {
        return Err(DelegateError::ApiOnlyFieldOnSubscription {
            vendor,
            alias: alias.clone(),
            field,
            why: if requested == Some(Billing::Subscription) && account_opts_into_api {
                "the consultation asked for subscription pricing"
            } else if account_opts_into_api {
                "a checkout's agentmux.toml selected the account, and that never runs on API \
                 pricing"
            } else {
                "the account does not set `billing = \"api\"` in the machine's agentmux.toml"
            },
        });
    }

    apply_credentials(env, vendor, alias, account, host_env)?;
    account.launch.apply(env, host_env);
    Ok(billing)
}

/// Map one account's fields onto the variables its CLI reads.
fn apply_credentials(
    env: &mut BTreeMap<String, String>,
    vendor: Vendor,
    alias: &AccountAlias,
    account: &Account,
    host_env: &BTreeMap<String, String>,
) -> Result<(), DelegateError> {
    // An agy account holds only a description, which loading the machine file enforces.
    let Some(vars) = credential_vars(vendor) else {
        return Ok(());
    };
    let home = crate::config::home_dir(host_env);

    if let Some(dir) = &account.config_dir {
        let resolved = crate::config::expand_tilde(dir, home.as_deref());
        // Checked here rather than left to the CLI: both vendors happily create a fresh config
        // directory and then report "Not logged in", which sends the caller looking at their
        // credentials instead of at the path that was wrong.
        if !resolved.is_dir() {
            return Err(DelegateError::AccountDirMissing {
                vendor,
                alias: alias.clone(),
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
        if billing == Billing::Subscription && is_billing_env_name(name) {
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

/// The `major.minor.patch` release a `--version` line names, as `codex-cli 0.163.0` and
/// `2.1.277 (Claude Code)` do, or a bare release such as Claude's stream reports.
pub(crate) fn release_of(version: &str) -> Option<(u64, u64, u64)> {
    let number = version
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = number.split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u64>().ok()
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use serde_json::json;
    use strum::VariantArray as _;

    use super::{
        AccountAlias, AccountChoice, CodexPermissions, CodexSandbox, Delegate, Isolation, ModelId,
        Permissions, Vendor,
    };

    /// The roster derived from the permissions keeps every vendor's name, in its order, as records
    /// and requests spell it.
    #[gtest]
    fn every_vendor_keeps_its_name() -> Result<()> {
        let names: Vec<serde_json::Value> = Vendor::VARIANTS
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<_, _>>()
            .or_fail()?;
        assert_that!(
            names,
            elements_are![eq(&json!("claude")), eq(&json!("codex")), eq(&json!("agy"))]
        );
        assert_that!(
            serde_json::from_value::<Vendor>(json!("agy")).or_fail()?,
            eq(Vendor::Agy)
        );
        // `--delegate` and `--deny` take Claude by its harness's name too.
        #[cfg(feature = "clap")]
        {
            use clap::ValueEnum as _;
            assert_that!(
                Vendor::from_str("claude-code", false),
                ok(eq(&Vendor::Claude))
            );
        }
        Ok(())
    }

    /// A delegate is written with its vendor and its permissions beside the fields every vendor
    /// shares, under the keys a record has always had.
    #[gtest]
    fn a_delegate_is_written_flat() -> Result<()> {
        let codex = Delegate {
            model: ModelId::parse("sol").or_fail()?,
            effort: None,
            account: AccountChoice::CliLogin,
            isolation: Some(Isolation::Isolated),
            permissions: Permissions::Codex(CodexPermissions {
                sandbox: Some(CodexSandbox::ReadOnly),
                network_access: Some(false),
            }),
        };
        assert_that!(
            serde_json::to_value(&codex).or_fail()?,
            eq(&json!({
                "vendor": "codex",
                "model": "sol",
                "sandbox": "read_only",
                "network_access": false,
                "account": {"kind": "cli_login"},
                "isolation": "isolated",
            }))
        );
        let agy = Delegate {
            model: ModelId::parse("gemini-3.8-flash-low").or_fail()?,
            effort: None,
            account: AccountChoice::Named(AccountAlias::parse("personal").or_fail()?),
            isolation: None,
            permissions: Permissions::Agy,
        };
        assert_that!(
            serde_json::to_value(&agy).or_fail()?,
            eq(&json!({
                "vendor": "agy",
                "model": "gemini-3.8-flash-low",
                "account": "personal",
            }))
        );
        Ok(())
    }
}
