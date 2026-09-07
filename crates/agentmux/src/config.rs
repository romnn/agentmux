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
//! It is found only at fixed locations under the home directory.
//!
//! A **project** file is found by walking up from the delegate's working directory, so it can
//! arrive with a `git clone`.
//! It may *select* an account and nothing else.
//! Were it allowed to define one, a cloned repository could name an endpoint of its own and
//! forward the caller's real key to it on the first delegation.

use std::collections::BTreeMap;
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
    #[error("the agentmux config at {path} is not valid: {source}")]
    Malformed {
        /// The file that could not be parsed.
        path: PathBuf,
        /// The underlying parse failure.
        source: toml::de::Error,
    },
    /// A project file tried to define an account rather than select one.
    #[error(
        "{path} defines accounts, and a project agentmux.toml may only select one with \
         [defaults.<vendor>]. Move the definitions to your machine configuration: a file that \
         travels with a repository must not be able to name a credential or an endpoint."
    )]
    ProjectFileDefinesAccounts {
        /// The offending file.
        path: PathBuf,
    },
    /// A configuration file anyone can write was found.
    #[error(
        "{path} is writable by other users, and it can name commands, endpoints and credentials. \
         Run `chmod go-w {path}` before agentmux will read it."
    )]
    WorldWritable {
        /// The offending file.
        path: PathBuf,
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
/// These two fields are the extension point, so agentmux never has to learn what Bedrock is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchEnv {
    /// Variables set to a literal value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Host variables to forward, by name.
    ///
    /// Named rather than valued so a rotated credential is picked up without editing the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_passthrough: Vec<String>,
}

impl LaunchEnv {
    /// Whether this layer contributes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.env_passthrough.is_empty()
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
            target.insert(name.clone(), value.clone());
        }
    }
}

/// One account: where its credentials live, or what they are.
///
/// Every field is optional and at least one must be set.
/// A `config_dir` account authenticates as a logged-in CLI profile; a `base_url` account points the
/// CLI at some other endpoint, which is how a locally served model is reached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// Whether this account keeps a credential in the file itself.
    #[must_use]
    pub fn holds_a_literal_secret(&self) -> bool {
        self.api_key.is_some() || self.auth_token.is_some()
    }
}

/// What a file asks for when the caller asks for nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[must_use]
    pub fn machine_paths(host_env: &BTreeMap<String, String>) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(xdg) = host_env.get("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            paths.push(PathBuf::from(xdg).join("agentmux").join(FILE_NAME));
        }
        if let Some(home) = home_dir(host_env) {
            let xdg_default = home.join(".config").join("agentmux").join(FILE_NAME);
            if !paths.contains(&xdg_default) {
                paths.push(xdg_default);
            }
            paths.push(home.join(FILE_NAME));
        }
        // Windows keeps per-user application data here rather than under a dot directory.
        if let Some(appdata) = host_env.get("APPDATA").filter(|v| !v.is_empty()) {
            paths.push(PathBuf::from(appdata).join("agentmux").join(FILE_NAME));
        }
        paths
    }

    /// Project-level locations: every directory from `start` up to, but excluding, the home
    /// directory.
    ///
    /// The walk stops below home so the home-directory file keeps its machine-level meaning, and
    /// never rises above it: a checkout on a shared mount would otherwise inherit whatever
    /// configuration the directory above it happens to hold.
    ///
    /// Both paths are canonicalised first.
    /// Without that a `start` containing `..` satisfies a component-wise `starts_with` while its
    /// ancestors climb straight out of home — `/home/u/../../tmp/x` would examine `/tmp` and `/`
    /// ahead of the user's own file, and `/tmp` is world-writable.
    #[must_use]
    pub fn project_paths(start: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
        let (Some(start), Some(home)) = (start, home) else {
            return Vec::new();
        };
        // A path that cannot be resolved is not walked at all.
        // Failing closed costs a project file in an exotic setup; failing open costs the guarantee
        // this function exists for.
        let (Ok(start), Ok(home)) = (start.canonicalize(), home.canonicalize()) else {
            return Vec::new();
        };
        if !start.starts_with(&home) {
            return Vec::new();
        }
        start
            .ancestors()
            .take_while(|dir| *dir != home)
            .map(|dir| dir.join(FILE_NAME))
            .collect()
    }

    /// Load the machine's accounts, plus any project file's choice of which to use.
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
    /// there, [`ConfigError::Unreadable`], [`ConfigError::Malformed`] or
    /// [`ConfigError::WorldWritable`] for a file that is, and
    /// [`ConfigError::ProjectFileDefinesAccounts`] for a project file that describes an account
    /// rather than naming one.
    pub fn load(host_env: &BTreeMap<String, String>, start: &Path) -> Result<Self, ConfigError> {
        let home = home_dir(host_env);

        // An exported-but-empty variable means unset, as it does for the state directory: a shell
        // that exports it unconditionally would otherwise make every command fail on a path of "".
        let mut config = if let Some(explicit) = host_env
            .get(CONFIG_PATH_ENV)
            .filter(|value| !value.is_empty())
        {
            let path = PathBuf::from(explicit);
            if !path.exists() {
                return Err(ConfigError::MissingExplicit { path });
            }
            Self::read(&path)?
        } else {
            let mut found = Self::default();
            for path in Self::machine_paths(host_env) {
                if path.exists() {
                    found = Self::read(&path)?;
                    break;
                }
            }
            found
        };

        for path in Self::project_paths(Some(start), home.as_deref()) {
            if !path.exists() {
                continue;
            }
            let project = Self::read(&path)?;
            if !project.accounts.is_empty() || !project.launch.is_empty() {
                return Err(ConfigError::ProjectFileDefinesAccounts { path });
            }
            // Extended, not replaced: a repository pinning its Codex account must not silently
            // drop a machine-wide Claude default, which would spend a different subscription.
            config.defaults.extend(project.defaults);
            config.project_source = Some(path);
            break;
        }

        Ok(config)
    }

    /// Read and parse one file.
    fn read(path: &Path) -> Result<Self, ConfigError> {
        refuse_if_world_writable(path)?;
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Unreadable {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: Self = toml::from_str(&text).map_err(|source| ConfigError::Malformed {
            path: path.to_path_buf(),
            source,
        })?;
        config.source = Some(path.to_path_buf());
        Ok(config)
    }

    /// The accounts defined for one vendor.
    #[must_use]
    pub fn accounts(&self, vendor: Vendor) -> BTreeMap<String, Account> {
        self.accounts
            .get(vendor.program())
            .cloned()
            .unwrap_or_default()
    }

    /// One account, by alias.
    #[must_use]
    pub fn account(&self, vendor: Vendor, alias: &str) -> Option<&Account> {
        self.accounts.get(vendor.program())?.get(alias)
    }

    /// The account to use for one vendor when the caller names none.
    #[must_use]
    pub fn default_account(&self, vendor: Vendor) -> Option<&str> {
        self.defaults
            .get(vendor.program())
            .and_then(|d| d.account.as_deref())
    }

    /// Every alias defined for one vendor, for an error message or a tool description.
    #[must_use]
    pub fn alias_names(&self, vendor: Vendor) -> Vec<String> {
        self.accounts
            .get(vendor.program())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// Refuse a configuration file that other users can write.
///
/// It can name commands, endpoints and credentials, so a writable file is an injection point
/// rather than merely an exposed one — the same reason `ssh` refuses a group-writable config.
/// Readability is not checked: `0644` is normal inside a container image, and failing a delegation
/// over it would cost a turn for no gain.
#[cfg(unix)]
fn refuse_if_world_writable(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(metadata) = std::fs::metadata(path) else {
        // An unreadable file is reported by the read that follows, with a better message.
        return Ok(());
    };
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(ConfigError::WorldWritable {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Permission bits do not carry the same meaning on Windows, so the check is skipped there.
#[cfg(not(unix))]
fn refuse_if_world_writable(_path: &Path) -> Result<(), ConfigError> {
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
