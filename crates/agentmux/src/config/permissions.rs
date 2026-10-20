//! What a delegate may do: each vendor's defaults and limits from the machine file, and the one
//! step that holds a consultation to them.
//!
//! `[codex.defaults]` and `[claude.defaults]` say what a consultation gets when its caller names
//! nothing; `[codex.limits]` and `[claude.limits]` say what a caller may ask for at all.
//! Both are the owner's, so only a machine file may set them.
//! Each key is one of the vendor CLI's own controls — Codex's sandbox modes and network switch,
//! Claude's permission modes and tool lists — and whether the delegate loads the user's own
//! configuration; agentmux adds no permission model of its own.
//!
//! A limit refuses; it never clamps.
//! A request beyond one is refused before anything is written, naming the limit and the file, so
//! the caller asks again knowingly rather than receiving an answer from a delegate that could do
//! less than it was told.
//! The limits are rechecked before every turn, so one the owner tightens applies to the next
//! follow-up of a consultation already running.
//! Defaults that their own file's limits would refuse are refused when the file is read, because
//! every consultation that relied on them would fail.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use strum::VariantArray as _;

use crate::delegate::{
    ClaudePermissionMode, ClaudePermissions, ClaudeTool, CodexPermissions, CodexSandbox, Isolation,
    Vendor,
};

use super::{Account, AccountAlias, ConfigError};

/// The most a Codex delegate may write when the machine file sets no `codex.limits.max_sandbox`.
///
/// Full access lifts the sandbox and its network restriction together, which is the owner's to
/// allow, never a caller's.
pub const BUILT_IN_MAX_SANDBOX: CodexSandbox = CodexSandbox::WorkspaceWrite;

/// The oldest codex release whose `exec` is known to honour a permission profile defined and
/// selected with `-c`, as `(major, minor, patch)`.
///
/// Read from the source of codex 0.162.1, the release the app-server protocol is pinned to, and
/// measured against codex-cli 0.163.0.
pub const PERMISSION_PROFILE_MIN_CODEX: (u64, u64, u64) = (0, 162, 1);

/// Why a consultation's permissions were refused.
///
/// Every message names what to ask for instead, because the caller is usually an agent that
/// cannot see the machine file and will retry from this text alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PermissionRefusal {
    /// The sandbox mode exceeds `codex.limits.max_sandbox`.
    #[error(
        "Cannot launch codex: sandbox=\"{}\" exceeds codex.limits.max_sandbox=\"{}\" {set_by}. \
         Request {}. Only the owner can raise this limit.",
        requested.config_name(),
        limit.config_name(),
        sandboxes_within(*limit)
    )]
    SandboxBeyondLimit {
        /// The mode asked for.
        requested: CodexSandbox,
        /// The most the limit allows.
        limit: CodexSandbox,
        /// Where the limit was set.
        set_by: LimitSource,
    },
    /// Network was asked for under `codex.limits.allow_network = false`.
    #[error(
        "Cannot launch codex: network_access=true is forbidden by codex.limits.allow_network=false \
         {set_by}. Request network_access=false. Only the owner can change this limit."
    )]
    NetworkForbidden {
        /// Where the limit was set.
        set_by: LimitSource,
    },
    /// Full access was asked for under `codex.limits.allow_network = false`, and full access has
    /// no network switch.
    #[error(
        "Cannot launch codex: sandbox=\"danger_full_access\" gives commands unrestricted network, \
         which codex.limits.allow_network=false {set_by} forbids. Request {}. Only the owner can \
         change this limit.",
        sandboxes_within(CodexSandbox::WorkspaceWrite)
    )]
    FullAccessNetworkForbidden {
        /// Where the limit was set.
        set_by: LimitSource,
    },
    /// Full access was asked for without network, which Codex cannot do.
    #[error(
        "Cannot launch codex: sandbox=\"danger_full_access\" has no network switch, so \
         network_access=false cannot be honoured. Request sandbox=\"workspace_write\" or \
         \"read_only\" with network_access=false, or leave network_access unset."
    )]
    FullAccessCannotDenyNetwork,
    /// The user's own configuration was asked for under `allow_user_config = false`.
    #[error(
        "Cannot launch {vendor}: ignore_user_config=false is forbidden by \
         {vendor}.limits.allow_user_config=false {set_by}. Request ignore_user_config=true."
    )]
    UserConfigForbidden {
        /// Whose limit it is.
        vendor: Vendor,
        /// Where the limit was set.
        set_by: LimitSource,
    },
    /// The Claude permission mode is not among `claude.limits.allowed_permission_modes`.
    #[error(
        "Cannot launch claude: permission_mode=\"{mode}\" is not in \
         claude.limits.allowed_permission_modes {set_by}. Request {}. Only the owner can change \
         this limit.",
        modes_named(allowed)
    )]
    PermissionModeNotAllowed {
        /// The mode asked for.
        mode: ClaudePermissionMode,
        /// The modes the limit allows.
        allowed: Vec<ClaudePermissionMode>,
        /// Where the limit was set.
        set_by: LimitSource,
    },
    /// Read-only with network needs a permission profile, and the installed codex is not one
    /// whose support for it is verified.
    #[error(
        "Cannot launch codex exec with sandbox=\"read_only\" and network_access=true: this Codex \
         installation lacks verified permission-profile support. Upgrade Codex, request \
         network_access=false, or use a supported app-server launch. It reports {}, and agentmux \
         has verified codex-cli {} and later.",
        describe_version(version.as_deref()),
        describe_min_codex()
    )]
    PermissionProfileUnsupported {
        /// What the installed codex printed for `--version`, when it printed anything.
        version: Option<String>,
    },
}

/// Where a limit came from, as a refusal names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitSource {
    /// The machine file sets it.
    File(PathBuf),
    /// The machine file sets nothing for it, so agentmux's own applies.
    BuiltIn {
        /// The machine file, when there is one, which is where to set it.
        machine_file: Option<PathBuf>,
    },
}

impl LimitSource {
    /// Where a limit came from: `set` says whether the file wrote it, `machine_file` is the file.
    fn of(set: bool, machine_file: Option<&Path>) -> Self {
        match (set, machine_file) {
            (true, Some(path)) => Self::File(path.to_path_buf()),
            (_, machine_file) => Self::BuiltIn {
                machine_file: machine_file.map(Path::to_path_buf),
            },
        }
    }
}

impl std::fmt::Display for LimitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => write!(f, "in {}", path.display()),
            Self::BuiltIn {
                machine_file: Some(path),
            } => write!(
                f,
                "(agentmux's built-in limit; {} can set another)",
                path.display()
            ),
            Self::BuiltIn { machine_file: None } => {
                f.write_str("(agentmux's built-in limit; a machine agentmux.toml can set another)")
            }
        }
    }
}

/// [`PERMISSION_PROFILE_MIN_CODEX`] as a release number.
fn describe_min_codex() -> String {
    let (major, minor, patch) = PERMISSION_PROFILE_MIN_CODEX;
    format!("{major}.{minor}.{patch}")
}

/// What a codex printed for `--version`, as a refusal quotes it.
fn describe_version(version: Option<&str>) -> String {
    version.map_or_else(|| "no version".to_owned(), |version| format!("`{version}`"))
}

/// Every sandbox mode up to `limit`, most permissive first, as a refusal offers them.
fn sandboxes_within(limit: CodexSandbox) -> String {
    let allowed: Vec<String> = CodexSandbox::VARIANTS
        .iter()
        .rev()
        .filter(|sandbox| **sandbox <= limit)
        .map(|sandbox| format!("\"{}\"", sandbox.config_name()))
        .collect();
    format!("sandbox={}", allowed.join(" or "))
}

/// The modes a refusal offers, or the advice that there are none.
fn modes_named(allowed: &[ClaudePermissionMode]) -> String {
    if allowed.is_empty() {
        return "nothing: the owner allows no permission mode".to_owned();
    }
    let names: Vec<String> = allowed.iter().map(|mode| format!("\"{mode}\"")).collect();
    format!("permission_mode={}", names.join(" or "))
}

/// `[codex]` in a machine file: what a Codex consultation gets by default, and may ask for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexPolicy {
    /// What a consultation gets when its caller names nothing.
    #[serde(default)]
    pub defaults: CodexDefaults,
    /// What a caller may ask for.
    #[serde(default)]
    pub limits: CodexLimits,
}

/// `[codex.defaults]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexDefaults {
    /// Whether to skip the user's own Codex configuration; `false` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_user_config: Option<bool>,
    /// The sandbox mode; `read_only` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<CodexSandbox>,
    /// Whether commands may reach the network; left to Codex when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_access: Option<bool>,
}

/// `[codex.limits]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexLimits {
    /// Whether a delegate may load the user's own Codex configuration; `true` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_user_config: Option<bool>,
    /// The most a delegate may write; [`BUILT_IN_MAX_SANDBOX`] when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sandbox: Option<CodexSandbox>,
    /// Whether a delegate's commands may reach the network; `true` when unset.
    ///
    /// `false` also switches the network off explicitly for a consultation that did not choose,
    /// so the user's own configuration cannot switch it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_network: Option<bool>,
}

/// `[claude]` in a machine file: what a Claude consultation gets by default, and may ask for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudePolicy {
    /// What a consultation gets when its caller names nothing.
    #[serde(default)]
    pub defaults: ClaudeDefaults,
    /// What a caller may ask for.
    #[serde(default)]
    pub limits: ClaudeLimits,
}

/// `[claude.defaults]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeDefaults {
    /// Whether to skip the user's own Claude settings, hooks and MCP servers; `false` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_user_config: Option<bool>,
    /// The permission mode; `plan` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<ClaudePermissionMode>,
    /// The built-in tools offered; agentmux's read-only list when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ClaudeTool>>,
    /// Calls preapproved; none when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<ClaudeTool>>,
    /// Calls denied; none when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<ClaudeTool>>,
}

/// `[claude.limits]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeLimits {
    /// Whether a delegate may load the user's own Claude settings; `true` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_user_config: Option<bool>,
    /// The permission modes a delegate may run under; every mode but `bypassPermissions` when
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_permission_modes: Option<Vec<ClaudePermissionMode>>,
}

macro_rules! empty_when_default {
    ($($ty:ident),+ $(,)?) => {$(
        impl $ty {
            /// Whether the table says nothing.
            #[must_use]
            pub fn is_empty(&self) -> bool {
                *self == Self::default()
            }
        }
    )+};
}
empty_when_default!(
    CodexPolicy,
    CodexDefaults,
    CodexLimits,
    ClaudePolicy,
    ClaudeDefaults,
    ClaudeLimits
);

impl CodexLimits {
    /// The most a delegate may write.
    #[must_use]
    pub fn max_sandbox(&self) -> CodexSandbox {
        self.max_sandbox.unwrap_or(BUILT_IN_MAX_SANDBOX)
    }

    /// Whether a delegate's commands may reach the network.
    #[must_use]
    pub fn allows_network(&self) -> bool {
        self.allow_network.unwrap_or(true)
    }
}

impl ClaudeLimits {
    /// Whether a delegate may run under `mode`.
    #[must_use]
    pub fn allows_mode(&self, mode: ClaudePermissionMode) -> bool {
        match &self.allowed_permission_modes {
            Some(allowed) => allowed.contains(&mode),
            None => mode != ClaudePermissionMode::BypassPermissions,
        }
    }

    /// Every mode a delegate may run under, in the CLI's order.
    #[must_use]
    pub fn allowed_modes(&self) -> Vec<ClaudePermissionMode> {
        ClaudePermissionMode::VARIANTS
            .iter()
            .copied()
            .filter(|mode| self.allows_mode(*mode))
            .collect()
    }
}

impl CodexPolicy {
    /// `requested` with what it leaves unset filled from the defaults, and the sandbox from
    /// agentmux's own read-only default when the file names none.
    #[must_use]
    pub fn fill(&self, requested: CodexPermissions) -> CodexPermissions {
        CodexPermissions {
            sandbox: Some(
                requested
                    .sandbox
                    .or(self.defaults.sandbox)
                    .unwrap_or_default(),
            ),
            network_access: requested.network_access.or(self.defaults.network_access),
        }
    }

    /// `permissions` as the limits let them run, or why they may not.
    ///
    /// The one change is a network nobody chose under `allow_network = false`, which becomes an
    /// explicit `false`.
    ///
    /// # Errors
    ///
    /// Returns the [`PermissionRefusal`] naming the first limit `permissions` exceeds.
    pub fn hold(
        &self,
        permissions: CodexPermissions,
        machine_file: Option<&Path>,
    ) -> Result<CodexPermissions, PermissionRefusal> {
        let sandbox = permissions.sandbox();
        let limit = self.limits.max_sandbox();
        if sandbox > limit {
            return Err(PermissionRefusal::SandboxBeyondLimit {
                requested: sandbox,
                limit,
                set_by: LimitSource::of(self.limits.max_sandbox.is_some(), machine_file),
            });
        }
        let network_limit = || LimitSource::of(self.limits.allow_network.is_some(), machine_file);
        let allows_network = self.limits.allows_network();
        let network_access = match (sandbox, permissions.network_access) {
            (_, Some(true)) if !allows_network => {
                return Err(PermissionRefusal::NetworkForbidden {
                    set_by: network_limit(),
                });
            }
            (CodexSandbox::DangerFullAccess, Some(false)) => {
                return Err(PermissionRefusal::FullAccessCannotDenyNetwork);
            }
            (CodexSandbox::DangerFullAccess, None) if !allows_network => {
                return Err(PermissionRefusal::FullAccessNetworkForbidden {
                    set_by: network_limit(),
                });
            }
            (_, None) if !allows_network => Some(false),
            (_, chosen) => chosen,
        };
        Ok(CodexPermissions {
            sandbox: Some(sandbox),
            network_access,
        })
    }
}

impl ClaudePolicy {
    /// `requested` with what it leaves unset filled from the defaults, and the permission mode
    /// from agentmux's own plan mode when the file names none.
    ///
    /// The tool list stays unset when neither names one, which launches agentmux's read-only
    /// list.
    #[must_use]
    pub fn fill(&self, requested: &ClaudePermissions) -> ClaudePermissions {
        let defaults = &self.defaults;
        ClaudePermissions {
            permission_mode: Some(
                requested
                    .permission_mode
                    .or(defaults.permission_mode)
                    .unwrap_or_default(),
            ),
            tools: requested.tools.clone().or_else(|| defaults.tools.clone()),
            allowed_tools: requested
                .allowed_tools
                .clone()
                .or_else(|| defaults.allowed_tools.clone()),
            disallowed_tools: requested
                .disallowed_tools
                .clone()
                .or_else(|| defaults.disallowed_tools.clone()),
        }
    }

    /// Refuse `permissions` when the limits do not allow them.
    ///
    /// # Errors
    ///
    /// Returns [`PermissionRefusal::PermissionModeNotAllowed`] for a mode the limits leave out.
    pub fn hold(
        &self,
        permissions: &ClaudePermissions,
        machine_file: Option<&Path>,
    ) -> Result<(), PermissionRefusal> {
        let mode = permissions.permission_mode();
        if self.limits.allows_mode(mode) {
            return Ok(());
        }
        Err(PermissionRefusal::PermissionModeNotAllowed {
            mode,
            allowed: self.limits.allowed_modes(),
            set_by: LimitSource::of(self.limits.allowed_permission_modes.is_some(), machine_file),
        })
    }
}

/// What one vendor's tables say about loading the user's own configuration.
///
/// The part of the policy both vendors share, read through [`super::Config::permissions`].
#[derive(Debug, Clone, Copy)]
pub struct PermissionPolicy<'a> {
    vendor: Vendor,
    ignore_user_config: Option<bool>,
    allow_user_config: Option<bool>,
    machine_file: Option<&'a Path>,
}

impl<'a> PermissionPolicy<'a> {
    pub(super) fn new(
        vendor: Vendor,
        ignore_user_config: Option<bool>,
        allow_user_config: Option<bool>,
        machine_file: Option<&'a Path>,
    ) -> Self {
        Self {
            vendor,
            ignore_user_config,
            allow_user_config,
            machine_file,
        }
    }

    /// Whether a delegate may load the user's own configuration.
    #[must_use]
    pub fn allows_user_config(&self) -> bool {
        self.allow_user_config.unwrap_or(true)
    }

    /// The isolation a consultation gets when neither its caller nor its account chose.
    ///
    /// The default the file names, else loading the user's configuration, unless the limits
    /// rule that out.
    #[must_use]
    pub fn default_isolation(&self) -> Isolation {
        match self.ignore_user_config {
            Some(ignore) => Isolation::ignoring_user_config(ignore),
            None if self.allows_user_config() => Isolation::Inherit,
            None => Isolation::Isolated,
        }
    }

    /// Refuse an isolation the limits do not allow.
    ///
    /// # Errors
    ///
    /// Returns [`PermissionRefusal::UserConfigForbidden`] for a delegate that would load the
    /// user's configuration under `allow_user_config = false`.
    pub fn hold_isolation(&self, isolation: Isolation) -> Result<(), PermissionRefusal> {
        if isolation.inherits() && !self.allows_user_config() {
            return Err(PermissionRefusal::UserConfigForbidden {
                vendor: self.vendor,
                set_by: LimitSource::of(self.allow_user_config.is_some(), self.machine_file),
            });
        }
        Ok(())
    }

    /// One line naming the default and the limit, for a listing of what this machine allows.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "ignore_user_config={} allow_user_config={}",
            self.default_isolation().ignores_user_config(),
            self.allows_user_config()
        )
    }
}

impl CodexPolicy {
    /// One line naming the effective defaults and limits, for a listing of what this machine
    /// allows.
    #[must_use]
    pub fn describe(&self) -> String {
        let defaults = self.fill(CodexPermissions::default());
        format!(
            "sandbox={} network_access={} max_sandbox={} allow_network={}",
            defaults.sandbox().config_name(),
            describe_choice(defaults.network_access),
            self.limits.max_sandbox().config_name(),
            self.limits.allows_network()
        )
    }
}

impl ClaudePolicy {
    /// One line naming the effective defaults and limits, for a listing of what this machine
    /// allows.
    #[must_use]
    pub fn describe(&self) -> String {
        let defaults = self.fill(&ClaudePermissions::default());
        let modes: Vec<&str> = self
            .limits
            .allowed_modes()
            .into_iter()
            .map(ClaudePermissionMode::as_str)
            .collect();
        let mut line = format!(
            "permission_mode={} allowed_permission_modes={}",
            defaults.permission_mode(),
            modes.join(",")
        );
        let lists = [
            ("tools", &defaults.tools),
            ("allowed_tools", &defaults.allowed_tools),
            ("disallowed_tools", &defaults.disallowed_tools),
        ];
        for (name, list) in lists {
            if let Some(list) = list {
                let names: Vec<&str> = list.iter().map(ClaudeTool::as_str).collect();
                // Writing into a `String` cannot fail.
                write!(line, " {name}={}", names.join(",")).ok();
            }
        }
        line
    }
}

/// An optional boolean as a listing names it, `unset` when nobody chose.
fn describe_choice(choice: Option<bool>) -> String {
    choice.map_or_else(|| "unset".to_owned(), |choice| choice.to_string())
}

/// Refuse defaults that the same file's limits would refuse, which would fail every consultation
/// relying on them.
///
/// Checked as a launch would check them: the vendor's defaults filled in as a consultation that
/// names nothing gets them, then held to the limits; and every account of the vendor that asks
/// to inherit settings, against `allow_user_config`.
pub(super) fn validate(
    path: &Path,
    vendor: Vendor,
    policy: PermissionPolicy<'_>,
    codex: &CodexPolicy,
    claude: &ClaudePolicy,
    accounts: &std::collections::BTreeMap<AccountAlias, Account>,
) -> Result<(), ConfigError> {
    let refused = |setting: String, refusal: PermissionRefusal| ConfigError::DefaultBeyondLimit {
        path: path.to_path_buf(),
        setting,
        refusal: refusal.to_string(),
    };
    policy
        .hold_isolation(policy.default_isolation())
        .map_err(|refusal| refused(format!("{vendor}.defaults.ignore_user_config"), refusal))?;
    for (alias, account) in accounts {
        if account.inherit_settings == Some(true) {
            policy
                .hold_isolation(Isolation::Inherit)
                .map_err(|refusal| {
                    refused(
                        format!("accounts.{vendor}.{alias}.inherit_settings = true"),
                        refusal,
                    )
                })?;
        }
    }
    let defaults = || format!("{vendor}.defaults, or agentmux's own where it sets nothing,");
    match vendor {
        Vendor::Codex => codex
            .hold(codex.fill(CodexPermissions::default()), Some(path))
            .map(drop),
        Vendor::Claude => claude.hold(&claude.fill(&ClaudePermissions::default()), Some(path)),
    }
    .map_err(|refusal| refused(defaults(), refusal))
}
