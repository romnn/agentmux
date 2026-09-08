//! What this machine offers a delegate: named accounts, and the environment launches run under.
//!
//! An agent asking for a second opinion knows it wants "the personal account"; it has no way to
//! know where that account's credentials live, and the answer differs on every machine.
//! This module is the indirection between the two, so a prompt that says `account: personal` works
//! unchanged on a laptop, a workstation and a CI box that each store it somewhere different.
//!
//! agentmux keeps no roster of account names, exactly as it keeps none of model ids.
//! Configuration is the only authority on which accounts exist, and one it does not define is an
//! error naming the ones it does.
//!
//! # Two files, two jobs
//!
//! A **machine** file defines accounts: paths, credentials, endpoints, environment.
//! It also holds the model rewrites — the machine's answer to "when something asks for *this*
//! model, run *that* one" — which is the other thing a caller cannot know from where it sits.
//! It is found only at fixed locations under the home directory, or at the one absolute path
//! `AGENTMUX_CONFIG` names.
//!
//! A **project** file is found by walking up from the delegate's working directory, so it can
//! arrive with a `git clone`.
//! It may *select* an account and nothing else.
//! Were it allowed to define one, a cloned repository could name an endpoint of its own and
//! forward the caller's real key to it on the first delegation.
//!
//! Both roles are read only from a file the current user owns and nobody else can write, because
//! either can change which identity a consultation runs as.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::delegate::Vendor;

/// Environment variable holding an explicit path to the configuration file.
pub const CONFIG_PATH_ENV: &str = "AGENTMUX_CONFIG";

/// The file name looked for, in both roles.
pub const FILE_NAME: &str = "agentmux.toml";

/// Why a configuration file could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but is not readable.
    #[error("cannot read the agentmux config at {path}: {source}")]
    Unreadable {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
    /// The file exists but is not valid TOML, or does not match the expected shape.
    ///
    /// Carries the parser's message and position but not the parser's error, whose own rendering
    /// quotes the offending source line — and the offending line of this file is as likely as
    /// not to hold a key.
    /// Kept out of the error chain too, so no reporter can print it.
    #[error("the agentmux config at {path} is not valid{}: {message}", describe_offset(*offset))]
    Malformed {
        /// The file that could not be parsed.
        path: PathBuf,
        /// What the parser objected to.
        message: String,
        /// Where in the file, as a byte offset, when the parser knows.
        offset: Option<usize>,
    },
    /// An alias or environment name in the file is not one agentmux can use.
    #[error("the agentmux config at {path} names {what} {value:?}, which {reason}")]
    InvalidName {
        /// The file.
        path: PathBuf,
        /// What kind of name it was.
        what: &'static str,
        /// The name as written.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A project file tried to define an account, a launch environment or a model rewrite,
    /// rather than select an account.
    #[error(
        "{path} defines accounts, model rewrites or a [launch] environment, and a project \
         agentmux.toml may only select an account with [defaults.<vendor>]. Move the definitions \
         to your machine configuration: a file that travels with a repository must not be able to \
         name a credential, an endpoint, an environment variable, or which model your questions \
         are answered by."
    )]
    ProjectFileOversteps {
        /// The offending file.
        path: PathBuf,
    },
    /// A model rewrite names a model the same file rewrites again.
    #[error(
        "the agentmux config at {path} rewrites the model {from:?} to {to:?}, which it rewrites \
         again. A rewrite is one substitution, never a chain, so this rule does not do what it \
         reads as: name the identifier you want run on both lines."
    )]
    ChainedRewrite {
        /// The file.
        path: PathBuf,
        /// The identifier a caller would ask for.
        from: String,
        /// What it is rewritten to, which is itself rewritten.
        to: String,
    },
    /// A configuration file another user could have written was found.
    #[error(
        "{path} {problem}, and it can name commands, endpoints and credentials. Make it owned by \
         you and writable only by you (`chmod go-w {path}`) before agentmux will read it."
    )]
    Untrusted {
        /// The offending file.
        path: PathBuf,
        /// What is wrong with it.
        problem: &'static str,
    },
    /// [`CONFIG_PATH_ENV`] named a file that does not exist.
    ///
    /// Only an explicit path is an error when missing.
    /// The searched locations are optional by design: a machine using one account per vendor needs
    /// no configuration at all.
    #[error("{CONFIG_PATH_ENV} points at {path}, which does not exist")]
    MissingExplicit {
        /// The path that was named.
        path: PathBuf,
    },
    /// [`CONFIG_PATH_ENV`] named a relative path.
    ///
    /// A relative path would resolve against whatever directory agentmux happens to run in, which
    /// under an MCP host is the project — and a project must not be able to supply the machine
    /// file.
    #[error("{CONFIG_PATH_ENV} must be an absolute path, not {path}")]
    RelativeExplicit {
        /// The path that was named.
        path: PathBuf,
    },
}

/// Report every key of `written` that `understood` does not hold, depth first.
///
/// A key whose value is an empty table or array is passed over: serialising drops an empty `env`
/// or `request_env`, so a file that spells one out would otherwise be told its own key is unknown.
/// Nothing is lost by the exception, because an empty value is what an absent key already means.
fn collect_unknown_keys(
    written: &toml::Table,
    understood: &toml::Table,
    prefix: &str,
    found: &mut Vec<String>,
) {
    for (key, value) in written {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match understood.get(key) {
            Some(toml::Value::Table(understood)) => {
                if let toml::Value::Table(written) = value {
                    collect_unknown_keys(written, understood, &path, found);
                }
            }
            Some(_) => {}
            None if is_empty_value(value) => {}
            None => found.push(path),
        }
    }
}

/// Whether a value says nothing that an absent key would not say.
fn is_empty_value(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table.is_empty(),
        toml::Value::Array(array) => array.is_empty(),
        _ => false,
    }
}

/// A parser message with every quoted value blanked.
///
/// A message about the wrong kind of value repeats the value — `invalid type: string "…"` — and
/// in this file a value is as likely as not to be a key.
fn without_quoted_values(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut quoted = false;
    for character in message.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                out.push('"');
                if quoted {
                    out.push('…');
                }
            }
            _ if quoted => {}
            _ => out.push(character),
        }
    }
    out
}

/// The byte offset of a parse failure, when the parser knows it.
fn describe_offset(offset: Option<usize>) -> String {
    offset.map_or_else(String::new, |offset| {
        format!(" (at byte {offset} of the file)")
    })
}

/// A credential value held in the configuration file.
///
/// Serialises as a placeholder rather than as itself.
/// [`Config`] is `Serialize` so it can be shown to a caller asking what this machine offers, and
/// that answer must never carry the key with it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wrap a value that must not be shown again.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for handing to the delegate's environment and nowhere else.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("<redacted>")
    }
}

/// Environment a launch runs under, layered from three places.
///
/// The child environment is otherwise an allowlist built from empty, which is deliberate but
/// cannot anticipate every setup: a Bedrock or Vertex delegate needs variables agentmux has never
/// heard of, and a user may want to switch off their own hooks for delegate runs.
/// These fields are the extension point, so agentmux never has to learn what Bedrock is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchEnv {
    /// Variables set to a literal value.
    ///
    /// Held as secrets because the module docs recommend this very table for a Bedrock key, and
    /// `agentmux accounts` prints the configuration back.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, Secret>,
    /// Host variables to forward, by name.
    ///
    /// Named rather than valued so a rotated credential is picked up without editing the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_passthrough: Vec<String>,
    /// Names a single request may set through its own `env`.
    ///
    /// Nothing by default.
    /// A request comes from a delegating agent acting on text it was given, and there is no
    /// list of names dangerous enough to refuse that stays complete: `PATH` runs a program of the
    /// caller's choosing, `NODE_OPTIONS` runs code inside the CLI before it reads a setting, and
    /// the next runtime will read one more.
    /// So the operator names what may be set, here, in a file no request can reach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_env: Vec<String>,
}

impl LaunchEnv {
    /// Whether this layer contributes nothing.
    ///
    /// Compared against the empty layer rather than field by field, so a field added later is
    /// counted by construction; this is the one gate on what a project file may say.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn canonicalise_env_names(&mut self) {
        self.env = std::mem::take(&mut self.env)
            .into_iter()
            .map(|(name, value)| (canonical_env_name(&name), value))
            .collect();
        for name in self
            .env_passthrough
            .iter_mut()
            .chain(self.request_env.iter_mut())
        {
            *name = canonical_env_name(name);
        }
    }

    /// Refuse a name that no environment could carry.
    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        let names = self
            .env
            .keys()
            .chain(&self.env_passthrough)
            .chain(&self.request_env);
        for name in names {
            check_env_name(name).map_err(|reason| ConfigError::InvalidName {
                path: path.to_path_buf(),
                what: "the environment variable",
                value: name.clone(),
                reason,
            })?;
        }
        Ok(())
    }

    /// Apply this layer over `target`.
    ///
    /// Passthrough is resolved first so an explicit `env` value in the same layer wins over a
    /// forwarded one of the same name.
    pub fn apply(
        &self,
        target: &mut BTreeMap<String, String>,
        host_env: &BTreeMap<String, String>,
    ) {
        for name in &self.env_passthrough {
            if let Some(value) = host_env.get(name) {
                target.insert(name.clone(), value.clone());
            }
        }
        for (name, value) in &self.env {
            target.insert(name.clone(), value.expose().to_owned());
        }
    }
}

/// Whether a string can name an environment variable.
///
/// The rule is the operating system's, not agentmux's: a name with `=` in it would be read by
/// the child as a different, shorter name, and a NUL byte cannot be carried at all.
///
/// # Errors
///
/// Returns the reason, phrased to follow the name in a message.
pub(crate) fn check_env_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("is empty");
    }
    if name.contains('=') {
        return Err("contains `=`, which would end the name early");
    }
    if name.chars().any(char::is_control) {
        return Err("contains a control character");
    }
    Ok(())
}

/// The spelling of an environment name the platform will read it under.
///
/// Windows reads names without regard to case; every other platform reads them exactly.
/// Every name agentmux handles — from the host environment, a configuration file or a request —
/// passes through here on the way in, so one comparison rule serves every map.
#[must_use]
pub fn canonical_env_name(name: &str) -> String {
    if cfg!(windows) {
        name.to_ascii_uppercase()
    } else {
        name.to_owned()
    }
}

/// One account: where its credentials live, or what they are.
///
/// Every field is optional and at least one must be set.
/// A `config_dir` account authenticates as a logged-in CLI profile; a `base_url` account points the
/// CLI at some other endpoint, which is how a locally served model is reached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// The CLI's configuration directory: `CLAUDE_CONFIG_DIR`, or `CODEX_HOME` for Codex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<PathBuf>,
    /// The API key, written into the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<Secret>,
    /// Name of a host environment variable to read the API key from instead.
    ///
    /// Preferred over [`Self::api_key`]: it keeps the secret out of a file that tends to be synced
    /// between machines and committed by accident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// The OAuth token, for Claude only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<Secret>,
    /// Name of a host environment variable to read the OAuth token from instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_env: Option<String>,
    /// An alternative API endpoint, such as a local server or a gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Run delegates on this account with its own settings, hooks and MCP servers.
    ///
    /// Off unless set.
    /// Turning it on gives up the three guarantees isolation buys — see
    /// [`crate::delegate::Isolation`] — and is worth it when the point of the consultation is to
    /// exercise the tooling that configuration sets up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit_settings: Option<bool>,
    /// Free text shown by `agentmux accounts`, so a caller can choose without guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Environment applied when this account is used.
    #[serde(flatten)]
    pub launch: LaunchEnv,
}

impl Account {
    /// Whether this account says nothing at all.
    ///
    /// An empty table is rejected rather than treated as the default account: it is always a
    /// mistake, and silently running as the default identity is the wrong way to resolve it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// What a file asks for when the caller asks for nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defaults {
    /// The account alias to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// The machine's agentmux configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Accounts by vendor, then by alias.
    ///
    /// The vendor level is a plain string rather than a closed enum so that a file naming a vendor
    /// this build has never heard of is ignored rather than rejected.
    /// One file is synced across machines that will not all be running the same version.
    #[serde(default)]
    pub accounts: BTreeMap<String, BTreeMap<String, Account>>,
    /// Which account to use per vendor when the caller names none.
    #[serde(default)]
    pub defaults: BTreeMap<String, Defaults>,
    /// Model identifiers to substitute, by vendor, then by the identifier a caller asks for.
    ///
    /// Read through [`Config::rewritten_model`].
    /// Keyed by vendor as [`Self::accounts`] is, and for the same reason: a file naming a vendor
    /// this build has never heard of loads anyway.
    #[serde(default)]
    pub models: BTreeMap<String, BTreeMap<String, String>>,
    /// Environment applied to every delegate launch.
    #[serde(default)]
    pub launch: LaunchEnv,
    /// Which machine file the accounts came from, or `None` when none was found.
    ///
    /// Skipped on the wire: it records where the file was, not what it said, and round-tripping it
    /// would let a copied file claim an origin it does not have.
    #[serde(skip)]
    pub source: Option<PathBuf>,
    /// Which project file selected a default, when one did.
    #[serde(skip)]
    pub project_source: Option<PathBuf>,
    /// Keys the files set that this build does not read, in the order they were found.
    ///
    /// Empty unless something is wrong: either the file was written for a newer agentmux, or a
    /// key is misspelled and doing nothing.
    /// Skipped on the wire for the same reason as [`Self::source`]: it records what a file said,
    /// not what agentmux understood of it.
    #[serde(skip)]
    pub unknown: Vec<UnknownKey>,
    /// The vendors whose default the project file changed.
    ///
    /// Read only through [`Config::default_account`], which is where the answer is typed.
    /// Public so a caller can build a `Config` in full; nothing outside this module should read
    /// it.
    #[serde(skip)]
    pub project_defaults: BTreeSet<String>,
}

/// A key a configuration file set that this build does not read.
///
/// Reported rather than refused, so that a file written for a newer agentmux still loads on an
/// older one: a long-running MCP server started before the key existed keeps working, and only
/// what the key was for is missing.
/// The cost is that a misspelling is no longer fatal either — `configdir` for `config_dir` leaves
/// an account authenticating as the CLI's own login — which is why every reader of a
/// configuration says these out loud.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnknownKey {
    /// The file that set it.
    pub file: PathBuf,
    /// The dotted path to the key, as `accounts.claude.work.configdir`.
    pub key: String,
}

impl std::fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} sets {}", self.file.display(), self.key)
    }
}

/// Which file chose a default account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChosenBy<'a> {
    /// The machine file, or a configuration built without one.
    Machine(Option<&'a Path>),
    /// A project file, which arrived with the checkout.
    Project(&'a Path),
}

impl ChosenBy<'_> {
    /// The file that made the choice, when there was one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Machine(path) => *path,
            Self::Project(path) => Some(path),
        }
    }
}

/// The account one vendor falls back to when the caller names none, and who said so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultAccount<'a> {
    /// The alias the file names, already checked to be one a caller could pass back.
    pub alias: &'a str,
    /// Which file chose it, because a checkout's choice may switch on less than the machine's.
    pub chosen_by: ChosenBy<'a>,
}

impl Config {
    /// Machine-level locations, in the order they are tried.
    ///
    /// These are the only files allowed to *define* an account, because defining one means naming
    /// a credential, an endpoint or an environment variable.
    /// None is reachable by walking up from a working directory, so a checkout can never become
    /// one.
    ///
    /// `~/.config/agentmux/` comes before anything platform-specific on every OS, macOS included:
    /// it is the path someone syncing dotfiles between machines can symlink, and
    /// `~/Library/Application Support/` is not.
    ///
    /// Every path is derived from `host_env` rather than from the process environment, so a caller
    /// that captured the environment resolves against the same snapshot the delegate will run
    /// under — and so a test cannot accidentally read the developer's own configuration.
    ///
    /// A base directory named by the environment counts only when it lies under the home
    /// directory.
    /// One that is relative would resolve against whatever directory agentmux runs in — under an
    /// MCP host, the project — and one pointed elsewhere, as a container image may point
    /// `XDG_CONFIG_HOME` at a workspace, would let a checkout supply the machine file.
    /// [`CONFIG_PATH_ENV`] is the deliberate way to keep the file anywhere else.
    #[must_use]
    pub fn machine_paths(host_env: &BTreeMap<String, String>) -> Vec<PathBuf> {
        let home = home_dir(host_env).filter(|home| home.is_absolute());
        // A prefix test alone would accept `$HOME/../../workspace`, which is under home only
        // component by component, so a path that climbs is not under anything.
        let under_home = |name: &str| {
            host_env
                .get(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .filter(|path| {
                    !path
                        .components()
                        .any(|part| part == std::path::Component::ParentDir)
                })
                .filter(|path| home.as_deref().is_some_and(|home| path.starts_with(home)))
        };
        let mut paths = Vec::new();
        if let Some(xdg) = under_home("XDG_CONFIG_HOME") {
            paths.push(xdg.join("agentmux").join(FILE_NAME));
        }
        if let Some(home) = &home {
            let xdg_default = home.join(".config").join("agentmux").join(FILE_NAME);
            if !paths.contains(&xdg_default) {
                paths.push(xdg_default);
            }
            paths.push(home.join(FILE_NAME));
        }
        // Windows keeps per-user application data here rather than under a dot directory.
        if let Some(appdata) = under_home("APPDATA") {
            paths.push(appdata.join("agentmux").join(FILE_NAME));
        }
        paths
    }

    /// Project-level locations: every directory from `start` up to the root of the repository it
    /// is in, or up to but excluding the home directory when it is in none.
    ///
    /// The repository root — the nearest ancestor holding `.git` — is the boundary a checked-out
    /// file means: it is where a clone puts it, and stopping there keeps a checkout on a shared
    /// mount from inheriting whatever the directory above it happens to hold.
    /// A directory in no repository is walked only inside home, where every ancestor is the
    /// user's own; outside home there is no such assurance and nothing is read.
    /// The home directory itself is never a project location, so the file there keeps its
    /// machine-level meaning.
    ///
    /// Both paths are canonicalised first.
    /// Without that a `start` containing `..` satisfies a component-wise `starts_with` while its
    /// ancestors climb straight out of home — `/home/u/../../tmp/x` would examine `/tmp` and `/`
    /// ahead of the user's own file, and `/tmp` is world-writable.
    #[must_use]
    pub fn project_paths(start: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
        let Some(start) = start else {
            return Vec::new();
        };
        // A path that cannot be resolved is not walked at all.
        // Failing closed costs a project file in an exotic setup; failing open costs the guarantee
        // this function exists for.
        let Ok(start) = start.canonicalize() else {
            return Vec::new();
        };
        let home = home.and_then(|home| home.canonicalize().ok());

        let mut paths = Vec::new();
        for dir in start.ancestors() {
            if home.as_deref() == Some(dir) {
                break;
            }
            paths.push(dir.join(FILE_NAME));
            if dir.join(".git").exists() {
                return paths;
            }
        }
        // No repository root: only a walk that stayed inside home is trusted.
        match home {
            Some(home) if start.starts_with(&home) => paths,
            _ => Vec::new(),
        }
    }

    /// Load what the machine offers, plus any project file's choice of which account to use.
    ///
    /// `host_env` is read rather than the process environment so a caller that has already
    /// captured the environment resolves against the same snapshot the delegate will run under.
    ///
    /// Within each role the first file found is used whole; files are not merged, because a
    /// half-overridden account map is harder to reason about than one that is either in effect or
    /// not.
    /// The two roles hold disjoint keys, so using both is not a merge.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::MissingExplicit`] when [`CONFIG_PATH_ENV`] names a file that is not
    /// there and [`ConfigError::RelativeExplicit`] when it names one relatively;
    /// [`ConfigError::Unreadable`], [`ConfigError::Malformed`], [`ConfigError::Untrusted`] or
    /// [`ConfigError::InvalidName`] for a file that is there; and
    /// [`ConfigError::ProjectFileOversteps`] for a project file that describes an account or a
    /// model rewrite rather than naming an account, and [`ConfigError::ChainedRewrite`] for a
    /// rewrite whose result is itself rewritten.
    pub fn load(host_env: &BTreeMap<String, String>, start: &Path) -> Result<Self, ConfigError> {
        let home = home_dir(host_env);

        // An exported-but-empty variable means unset, as it does for the state directory: a shell
        // that exports it unconditionally would otherwise make every command fail on a path of "".
        let mut config = if let Some(explicit) = host_env
            .get(CONFIG_PATH_ENV)
            .filter(|value| !value.is_empty())
        {
            let path = PathBuf::from(explicit);
            if !path.is_absolute() {
                return Err(ConfigError::RelativeExplicit { path });
            }
            if !path.exists() {
                return Err(ConfigError::MissingExplicit { path });
            }
            Self::read(&path)?
        } else {
            let mut found = Self::default();
            for path in Self::machine_paths(host_env) {
                if path.is_file() {
                    found = Self::read(&path)?;
                    break;
                }
            }
            found
        };

        // The machine file keeps its role even when the walk passes over it, which happens when
        // an explicit path points inside the checkout.
        let machine_file = config
            .source
            .as_deref()
            .and_then(|path| path.canonicalize().ok());
        for path in Self::project_paths(Some(start), home.as_deref()) {
            if !path.is_file() || path.canonicalize().ok() == machine_file {
                continue;
            }
            let project = Self::read(&path)?;
            if !project.accounts.is_empty()
                || !project.launch.is_empty()
                || !project.models.is_empty()
            {
                return Err(ConfigError::ProjectFileOversteps { path });
            }
            // Extended, not replaced: a repository pinning its Codex account must not silently
            // drop a machine-wide Claude default, which would spend a different subscription.
            // A project entry that merely restates the machine's own default changes nothing,
            // and is not counted as the project's choice.
            for (vendor, chosen) in project.defaults {
                if config.defaults.get(&vendor) != Some(&chosen) {
                    config.project_defaults.insert(vendor.clone());
                }
                config.defaults.insert(vendor, chosen);
            }
            config.project_source = Some(path);
            config.unknown.extend(project.unknown);
            break;
        }

        for unknown in &config.unknown {
            // Warned at every load rather than once: this is a file the operator can fix, and the
            // reader who needs to see it is whoever is looking at a delegation that behaved
            // unexpectedly, not whoever started the process.
            tracing::warn!(%unknown, "this agentmux does not read that key, and is ignoring it");
        }
        Ok(config)
    }

    /// Read and parse one file.
    ///
    /// The file is opened once and inspected through that handle, so what is checked is what is
    /// read: a link swapped between the check and the read would otherwise pass one file's
    /// permissions off as another's.
    fn read(path: &Path) -> Result<Self, ConfigError> {
        let unreadable = |source| ConfigError::Unreadable {
            path: path.to_path_buf(),
            source,
        };
        let mut file = std::fs::File::open(path).map_err(unreadable)?;
        refuse_if_untrusted(path, &file)?;
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(unreadable)?;
        let mut config: Self = toml::from_str(&text).map_err(|source| ConfigError::Malformed {
            path: path.to_path_buf(),
            message: without_quoted_values(source.message()),
            offset: source.span().map(|span| span.start),
        })?;
        // Before the names are canonicalised: on Windows that rewrites the keys of `env`, and a
        // comparison against rewritten keys would report the file's own spelling as unknown.
        config.unknown = config.unknown_keys(&text, path);
        config.validate(path)?;
        config.canonicalise_env_names();
        config.source = Some(path.to_path_buf());
        Ok(config)
    }

    /// Which of a file's keys this build does not read.
    ///
    /// Answered by asking what the parsed configuration serialises back to and comparing that with
    /// the file, so the answer cannot drift from the types: a field added to a struct is known
    /// here the moment it exists, and one removed stops being known, with no second list to keep
    /// in step.
    ///
    /// Re-parsing cannot fail — the same text has just parsed as a `Config` — and neither can
    /// serialising a `Config`; either way round, a build that cannot say what it does not know
    /// says nothing rather than refusing a file it has already read.
    fn unknown_keys(&self, text: &str, path: &Path) -> Vec<UnknownKey> {
        let (Ok(written), Ok(toml::Value::Table(understood))) =
            (text.parse::<toml::Table>(), toml::Value::try_from(self))
        else {
            return Vec::new();
        };
        let mut found = Vec::new();
        collect_unknown_keys(&written, &understood, "", &mut found);
        found
            .into_iter()
            .map(|key| UnknownKey {
                file: path.to_path_buf(),
                key,
            })
            .collect()
    }

    /// Spell every environment name the way the platform will read it.
    ///
    /// Windows reads names without regard to case, so `path` and `PATH` are one variable there
    /// and every table in the file is spelled in upper case once it is read; everywhere else a
    /// name is what it is.
    /// Done once here, so every later comparison — withholding a credential, matching a request
    /// against `request_env` — is a plain one.
    fn canonicalise_env_names(&mut self) {
        self.launch.canonicalise_env_names();
        for accounts in self.accounts.values_mut() {
            for account in accounts.values_mut() {
                account.launch.canonicalise_env_names();
                if let Some(name) = account.api_key_env.as_mut() {
                    *name = canonical_env_name(name);
                }
                if let Some(name) = account.auth_token_env.as_mut() {
                    *name = canonical_env_name(name);
                }
            }
        }
    }

    /// Refuse a name the file uses that could never be used back.
    ///
    /// An alias is advertised to callers and passed back as an argument, so one the argument
    /// parser would refuse must not be advertised; an environment name the operating system
    /// would misread must not reach a child.
    /// Vendor keys are left alone, so a file naming a vendor this build does not know still
    /// loads.
    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        for (vendor, accounts) in &self.accounts {
            for (alias, account) in accounts {
                crate::delegate::AccountAlias::parse(alias).map_err(|_| {
                    ConfigError::InvalidName {
                        path: path.to_path_buf(),
                        what: "the account alias",
                        value: alias.clone(),
                        reason: "may only contain letters, digits, `-`, `_` and `.`",
                    }
                })?;
                if account.is_empty() {
                    return Err(ConfigError::InvalidName {
                        path: path.to_path_buf(),
                        what: "the account",
                        value: format!("{vendor}.{alias}"),
                        reason: "is empty; give it a `config_dir`, an `api_key`, an \
                                 `api_key_env` or a `base_url`",
                    });
                }
                account.launch.validate(path)?;
                for name in [&account.api_key_env, &account.auth_token_env]
                    .into_iter()
                    .flatten()
                {
                    check_env_name(name).map_err(|reason| ConfigError::InvalidName {
                        path: path.to_path_buf(),
                        what: "the environment variable",
                        value: name.clone(),
                        reason,
                    })?;
                }
            }
        }
        for rewrites in self.models.values() {
            for (from, to) in rewrites {
                for value in [from, to] {
                    crate::delegate::ModelId::parse(value).map_err(|_| {
                        ConfigError::InvalidName {
                            path: path.to_path_buf(),
                            what: "the model",
                            value: value.clone(),
                            reason: "is not an identifier a delegate CLI could be given",
                        }
                    })?;
                }
                // A rule naming another rule's key reads as a chain, and one substitution is all
                // there is; refused here rather than resolved, because either answer would be a
                // guess at which of the two lines the operator meant.
                // A rule that maps a name to itself changes nothing, so a rule pointing at it
                // does not chain.
                let is_rewritten = |name: &String| rewrites.get(name).is_some_and(|to| to != name);
                if is_rewritten(from) && is_rewritten(to) {
                    return Err(ConfigError::ChainedRewrite {
                        path: path.to_path_buf(),
                        from: from.clone(),
                        to: to.clone(),
                    });
                }
            }
        }
        for chosen in self.defaults.values().filter_map(|d| d.account.as_ref()) {
            crate::delegate::AccountAlias::parse(chosen).map_err(|_| ConfigError::InvalidName {
                path: path.to_path_buf(),
                what: "the default account",
                value: chosen.clone(),
                reason: "may only contain letters, digits, `-`, `_` and `.`",
            })?;
        }
        self.launch.validate(path)
    }

    /// The accounts defined for one vendor, which may be none.
    #[must_use]
    pub fn accounts(&self, vendor: Vendor) -> &BTreeMap<String, Account> {
        static NONE: BTreeMap<String, Account> = BTreeMap::new();
        self.accounts.get(vendor.program()).unwrap_or(&NONE)
    }

    /// One account, by alias.
    #[must_use]
    pub fn account(&self, vendor: Vendor, alias: &str) -> Option<&Account> {
        self.accounts.get(vendor.program())?.get(alias)
    }

    /// The model rewrites defined for one vendor, which may be none.
    ///
    /// For a reader listing what the machine offers; [`Self::rewritten_model`] is what a launch
    /// asks.
    #[must_use]
    pub fn model_rewrites(&self, vendor: Vendor) -> &BTreeMap<String, String> {
        static NONE: BTreeMap<String, String> = BTreeMap::new();
        self.models.get(vendor.program()).unwrap_or(&NONE)
    }

    /// What to run when a caller asks for `model`, when this machine rewrites it.
    ///
    /// The rewrite is how an operator says once, in one file, that "fable-5" means whichever
    /// point release they actually want today.
    /// agentmux keeps no roster of models and this does not become one: an identifier the file
    /// says nothing about is not touched, so a model released this morning reaches the delegate
    /// CLI unchanged and without an agentmux release.
    ///
    /// The match is exact, because the delegate CLI's own match is, and one substitution is
    /// applied: the result is never looked up again.
    /// A rule naming the same identifier on both sides asks for nothing and answers `None`.
    /// A file whose rules would chain is refused when it is read, so what is written on one line
    /// is what runs.
    #[must_use]
    pub fn rewritten_model(&self, vendor: Vendor, model: &str) -> Option<&str> {
        self.model_rewrites(vendor)
            .get(model)
            .map(String::as_str)
            .filter(|rewritten| *rewritten != model)
    }

    /// The account to use for one vendor when the caller names none, and which file chose it.
    ///
    /// A project file may choose which of the machine's accounts pays; it may not, through that
    /// choice, switch on anything else the account is configured with.
    /// Whoever acts on a default needs to know which file did the choosing, and this is the one
    /// place that is decided.
    #[must_use]
    pub fn default_account(&self, vendor: Vendor) -> Option<DefaultAccount<'_>> {
        let alias = self
            .defaults
            .get(vendor.program())
            .and_then(|d| d.account.as_deref())?;
        let chosen_by = match (
            self.project_defaults.contains(vendor.program()),
            self.project_source.as_deref(),
        ) {
            (true, Some(path)) => ChosenBy::Project(path),
            _ => ChosenBy::Machine(self.source.as_deref()),
        };
        Some(DefaultAccount { alias, chosen_by })
    }

    /// Every alias defined for one vendor, for an error message or a tool description.
    #[must_use]
    pub fn alias_names(&self, vendor: Vendor) -> Vec<String> {
        self.accounts(vendor).keys().cloned().collect()
    }
}

/// Refuse a configuration file that another user owns or could write.
///
/// It can name commands, endpoints and credentials, so a writable file is an injection point
/// rather than merely an exposed one — the same reason `ssh` refuses a group-writable config.
/// Ownership matters for the same reason: a project walk can pass through directories other
/// users write to, and a file they own with tidy permissions is still theirs.
/// Readability is not checked: `0644` is normal inside a container image, and failing a delegation
/// over it would cost a turn for no gain.
#[cfg(unix)]
fn refuse_if_untrusted(path: &Path, file: &std::fs::File) -> Result<(), ConfigError> {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(metadata) = file.metadata() else {
        // An unreadable file is reported by the read that follows, with a better message.
        return Ok(());
    };
    let refuse = |problem| {
        Err(ConfigError::Untrusted {
            path: path.to_path_buf(),
            problem,
        })
    };
    if !metadata.is_file() {
        return refuse("is not a regular file");
    }
    if metadata.uid() != nix::unistd::geteuid().as_raw() {
        return refuse("is owned by another user");
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return refuse("is writable by other users");
    }
    Ok(())
}

/// Permission bits and ownership do not carry the same meaning on Windows, so only the shape is
/// checked there.
#[cfg(not(unix))]
fn refuse_if_untrusted(path: &Path, file: &std::fs::File) -> Result<(), ConfigError> {
    if file.metadata().is_ok_and(|metadata| !metadata.is_file()) {
        return Err(ConfigError::Untrusted {
            path: path.to_path_buf(),
            problem: "is not a regular file",
        });
    }
    Ok(())
}

/// The user's home directory, as the host environment reports it.
///
/// Windows sets `USERPROFILE` rather than `HOME`, and agentmux ships Windows binaries, so reading
/// only `HOME` would leave discovery finding nothing there.
#[must_use]
pub fn home_dir(host_env: &BTreeMap<String, String>) -> Option<PathBuf> {
    host_env
        .get("HOME")
        .or_else(|| host_env.get("USERPROFILE"))
        .map(PathBuf::from)
}

/// Expand a leading `~` against `home`.
///
/// A config file is written by a person, and a person writes `~/.claude-personal`.
/// Only a leading `~/` is expanded: `~other` means another user's home to a shell, and agentmux has
/// no business guessing at that.
#[must_use]
pub fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    home.join(rest)
}
