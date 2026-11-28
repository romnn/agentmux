//! The `codex` CLI: its sandbox and network, and the arguments `codex exec` and
//! `codex app-server` run with.

pub mod fold;
pub mod quota;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use strum::VariantArray;

use crate::accounting::{ReadSpend, Scope, Tokens};
use crate::appserver::DriverPlan;
use crate::config::PermissionRefusal;
use crate::delegate::{
    CredentialVars, Delegate, DelegateError, Effort, Invocation, Isolation, LaunchRequest, ModelId,
    PreparedLaunch, TurnPlan, Vendor, release_of, reroutes_within,
};
use crate::run::LaunchMode;
use crate::transcript::Usage;

/// How a run store launches `invocation`, Codex's own CLI for one turn of `delegate` with
/// `permissions`.
///
/// It runs over `codex app-server`, driven by agentmux so that it can be steered, when it loads
/// its account's settings, the store has a driver, and the consultation did not ask for
/// `codex exec`.
/// The isolation every other consultation gets rests on `codex exec --ignore-user-config`, and
/// `codex app-server` has no equivalent: codex-cli 0.159.3 refuses that flag there, its protocol
/// schema has no field that skips `$CODEX_HOME/config.toml`, and a `-c` override merges into the
/// user's tables rather than replacing them, as `codex_args` records.
/// An isolated delegate therefore always runs over `exec`, where it cannot be steered; why a
/// private `CODEX_HOME` is no way round that is in [`crate::appserver`].
/// `codex-app-server-codegen refresh` refuses the first codex whose app-server takes the flag,
/// which is when to let isolated delegates through here.
///
/// # Errors
///
/// Returns [`DelegateError::Permission`] for a `codex exec` turn that needs a permission profile
/// from a codex whose `--version` is not one verified to honour it.
pub(crate) fn prepare(
    delegate: &Delegate,
    permissions: CodexPermissions,
    mut invocation: Invocation,
    request: &LaunchRequest<'_>,
) -> Result<PreparedLaunch, DelegateError> {
    let &LaunchRequest {
        turn,
        cwd,
        resume,
        isolation,
        cli_version,
        driver,
        codex_exec,
        ..
    } = request;

    // Driven over the app-server
    if let Some(driver) = driver
        && isolation.inherits()
        && !codex_exec
    {
        let driver_plan = DriverPlan {
            settings: AppServerSettings {
                program: Vendor::Codex.program().to_owned(),
                args: codex_app_server_args(delegate.effort.as_ref(), permissions),
                model: delegate.model.clone(),
                sandbox: permissions.sandbox(),
                network_access: permissions.network_access,
            },
            cwd: cwd.to_path_buf(),
            resume_thread_id: resume.cloned(),
            billing: invocation.billing(),
            turn_dir: turn.root.clone(),
        };
        invocation.set_driver_command(
            driver,
            crate::appserver::SUBCOMMAND,
            &turn.app_server_plan_path(),
        );
        return Ok(PreparedLaunch {
            invocation,
            mode: LaunchMode::Driven,
            driver_plan: Some(driver_plan),
        });
    }

    // Run with `codex exec`, which reaches read-only with network only through a permission
    // profile: a codex that does not honour one would replace it with whatever its configuration
    // selects.
    // The app-server sets the same policy as a field of the turn instead, and needs no profile.
    if permissions.needs_permission_profile() && !codex_supports_permission_profiles(cli_version) {
        return Err(DelegateError::Permission(
            PermissionRefusal::PermissionProfileUnsupported {
                version: cli_version.map(ToOwned::to_owned),
            },
        ));
    }
    Ok(PreparedLaunch {
        invocation,
        mode: LaunchMode::DirectCli,
        driver_plan: None,
    })
}

/// What a Codex turn's report says it spent, whichever release wrote it.
///
/// Its tokens are its thread's running total, which a resumed thread carries on from, and its
/// `input_tokens` already count the cached prompt tokens.
/// It reports no price, so a price it did report would be in a scope nothing documents.
pub(crate) fn read_spend(usage: &Usage) -> ReadSpend {
    ReadSpend {
        tokens: Tokens::cache_inside_input(usage),
        tokens_cover: Scope::Session,
        price_covers: None,
    }
}

/// Credential variables the Codex CLI reads, including where its login is kept.
pub(crate) const CREDENTIAL_KEYS: &[&str] = &["OPENAI_API_KEY", "OPENAI_BASE_URL", "CODEX_HOME"];

/// The variables a Codex account's fields map onto.
pub(crate) const CREDENTIAL_VARS: CredentialVars = CredentialVars {
    config_dir: "CODEX_HOME",
    api_key: "OPENAI_API_KEY",
    auth_token: None,
    base_url: "OPENAI_BASE_URL",
};

/// Whether `upper`, a name in upper case, puts Codex on API pricing or another endpoint, beyond
/// what [`crate::delegate::is_billing_env_name`] catches for every vendor: an access token it
/// prefers over its logged-in profile, or an endpoint or a token among its own variables and
/// `OpenAI`'s.
pub(crate) fn is_billing_env_name(upper: &str) -> bool {
    upper == "CODEX_ACCESS_TOKEN" || reroutes_within(upper, &["OPENAI_", "AZURE_OPENAI_", "CODEX_"])
}

/// Codex's configuration directory, as it resolves it from `env`: `CODEX_HOME`, or `~/.codex`
/// when that is unset or empty.
pub(crate) fn config_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    env.get(CREDENTIAL_VARS.config_dir)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(crate::config::home_dir(env)?.join(".codex")))
}

/// Where Codex keeps a rollout per session, which `codex exec resume` reads back.
pub(crate) fn session_store(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    Some(config_dir(env)?.join("sessions"))
}

/// How much of the filesystem a Codex delegate's commands may write.
///
/// These are Codex's own sandbox modes, which govern the commands the delegate runs; they do not
/// constrain MCP servers or hooks a delegate that loads the user's configuration starts.
/// A consultation is a second opinion, so `ReadOnly` is the default.
/// `WorkspaceWrite` exists because a delegate asked to produce a large artefact cannot deliver it
/// through a read-only sandbox — a read-only delegate told to write its report to a path completes
/// the work, fails the write, and reports the failure instead of the findings.
/// `DangerFullAccess` lifts the sandbox altogether, network included; a launch may ask for it only
/// when the machine file raises `codex.limits.max_sandbox` to it.
///
/// Ordered from least to most access, which is the order `codex.limits.max_sandbox` compares in.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    VariantArray,
)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "snake_case")]
pub enum CodexSandbox {
    /// Commands may read files and write none; network is off unless
    /// [`CodexPermissions::network_access`] turns it on.
    #[default]
    ReadOnly,
    /// Commands may also write inside the working directory and the roots Codex adds to it, such
    /// as its temporary directories; network follows Codex's own configuration unless
    /// [`CodexPermissions::network_access`] decides it.
    WorkspaceWrite,
    /// No Codex sandbox at all: commands may write anywhere and reach any network.
    DangerFullAccess,
}

impl CodexSandbox {
    /// The value Codex uses for this mode, both as `--sandbox` and as `sandbox_mode`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    /// The name agentmux's configuration, its MCP tools and its refusals use for this mode.
    #[must_use]
    pub fn config_name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::DangerFullAccess => "danger_full_access",
        }
    }
}

/// What a Codex delegate's commands may do: its sandbox mode and whether they reach the network.
///
/// Both are optional in a request, where an absent value takes the machine file's default, and
/// in a record, where an absent `network_access` means nobody chose and Codex's own configuration
/// decides.
/// A recorded `sandbox` is always present once a consultation has started; one missing from a
/// record means the read-only mode every consultation ran under before it was recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexPermissions {
    /// The sandbox mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<CodexSandbox>,
    /// Whether commands may reach the network, or `None` to leave it to Codex.
    ///
    /// Read-only with network runs under a native permission profile, since Codex's legacy
    /// read-only mode has no network switch; see `codex_args`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_access: Option<bool>,
}

impl CodexPermissions {
    /// The sandbox mode, read-only when none was recorded.
    #[must_use]
    pub fn sandbox(self) -> CodexSandbox {
        self.sandbox.unwrap_or_default()
    }

    /// Whether these permissions need Codex's native permission profile under `codex exec`: a
    /// read-only sandbox whose commands may reach the network.
    #[must_use]
    pub fn needs_permission_profile(self) -> bool {
        self.sandbox() == CodexSandbox::ReadOnly && self.network_access == Some(true)
    }

    /// These permissions, with each one `asked` sets in place of the recorded one.
    ///
    /// Field by field, so a follow-up that changes only the sandbox keeps a network the
    /// consultation switched off.
    pub(crate) fn overridden_by(self, asked: Self) -> Self {
        Self {
            sandbox: asked.sandbox.or(self.sandbox),
            network_access: asked.network_access.or(self.network_access),
        }
    }

    /// How these permissions read in a one-line summary, as `read-only+network`.
    pub(crate) fn describe(self) -> String {
        let network = match self.network_access {
            Some(true) => "+network",
            Some(false) => "+no-network",
            None => "",
        };
        format!("{}{network}", self.sandbox().as_str())
    }
}

/// What a Codex turn driven over `codex app-server` is started with.
///
/// The counterpart of the argument vector `codex_args` builds for `codex exec`, with the same
/// model, effort, sandbox and network; the rest of the plan — the working directory, the session
/// to resume, where the turn's files live — belongs to the run store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppServerSettings {
    /// The executable, resolved against the child's `PATH`.
    pub program: String,
    /// The arguments that start the server, effort and a workspace-write network choice included.
    pub args: Vec<String>,
    /// The model the thread runs, pinned on every turn as `exec` pins it.
    pub model: ModelId,
    /// How much the delegate's commands may write, set on the thread at start and resume.
    pub sandbox: CodexSandbox,
    /// Whether a read-only delegate's commands may reach the network, sent with every
    /// `turn/start` as its sandbox policy; `None` leaves it to Codex.
    ///
    /// Absent from a plan an older agentmux wrote, which ran with Codex's own choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_access: Option<bool>,
}

/// A fresh name for the Codex permission profile one read-only turn with network runs under.
///
/// Drawn anew for every launch, resumes included, because Codex merges configuration tables
/// recursively across its layers: a profile of the same name in the user's or a trusted project's
/// configuration would add its own entries, such as a filesystem write grant, to the one agentmux
/// defines, and a name a configuration file could know in advance could be defined there.
fn permission_profile_name() -> String {
    format!("agentmux_ro_{}", uuid::Uuid::new_v4().simple())
}

/// Argument vector for the Codex CLI.
///
/// - `--json`: without it stdout carries only the final answer and stays empty until the process
///   exits, so there is neither progress nor a recovery path.
/// - `--ignore-user-config` with `-c features.hooks=false`, for an isolated delegate: the
///   isolation control and the structural hook defence on this side.
///   It drops `$CODEX_HOME/config.toml`, and with it the machine's configured MCP servers, so the
///   delegate cannot re-enter agentmux or reach any other server the host happens to run.
///   Authentication is unaffected, because it comes from `CODEX_HOME` rather than the config
///   file.
///   A delegate that loads the user's configuration gets neither, and runs the user's hooks and
///   MCP servers.
/// - The sandbox is always passed, so a delegate that loads the user's configuration does not
///   take its sandbox from it.
///   `--sandbox` on a new session and `-c sandbox_mode` on a resume, which **rejects `--sandbox`**
///   and exits 2; either one selects Codex's legacy sandbox modes, overriding a
///   `default_permissions` profile the user's configuration selects.
/// - A workspace-write delegate's network goes as `-c sandbox_workspace_write.network_access`.
/// - A read-only delegate *with* network has no legacy spelling, so it runs under a native
///   permission profile of the consultation's own, defined and selected with `-c` on every turn:
///   `permissions.<name>={extends=":read-only",network={enabled=true}}` and
///   `default_permissions="<name>"`, with no legacy sandbox selection beside it, because
///   `--sandbox` would force the legacy modes and drop the profile.
///   Measured against codex-cli 0.163.0, whose sandbox refused a write and let a request through
///   under exactly that profile; the run store refuses it on an older codex.
///   The profile's name is fresh on every launch; see `permission_profile_name`.
/// - `-c approval_policy="never"` with `-c approvals_reviewer="user"`, on every launch: nobody is
///   there to approve, and an approval is a way out of the sandbox.
///   `codex exec` defaults to `never`, but codex 0.162.1's `build_exec_config` drops that default
///   when the configuration it loaded names the `auto_review` reviewer, which would let the user's
///   own `approval_policy` have an automatic reviewer approve commands outside the sandbox and
///   past the machine file's limits.
/// - `--output-last-message`: written by the codex process rather than by the agent, so a
///   read-only sandbox does not block it.
///   It is a convenience; the event stream is the truth.
/// - Resume does not inherit the original model, so model and effort are re-pinned on every
///   turn, as is the sandbox.
/// - `--ephemeral` is deliberately **absent**: it does not persist the session, which would make
///   a follow-up impossible.
/// - `-c mcp_servers={}` is deliberately **absent** because it does not work.
///   Measured against codex-cli 0.153.4: a delegate launched with that override still reached a
///   configured MCP server and successfully called one of its tools.
///   Only `--ignore-user-config` actually detaches them.
///   Its cost is the user's `model_catalog_json`, so a delegate may report fallback metadata for
///   a very new model; that arrives as an advisory item rather than a failure.
pub(crate) fn codex_args(
    model: &ModelId,
    effort: Option<&Effort>,
    permissions: CodexPermissions,
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
    args.extend(codex_sandbox_args(permissions, plan));
    args.extend(unattended_approval_args());
    args.push("--output-last-message".to_owned());
    args.push(plan.last_message_path.to_string_lossy().into_owned());
    // `-` makes Codex read the prompt from stdin.
    // Without it Codex still drains stdin looking for extra input and a child whose stdin never
    // closes blocks.
    args.push("-".to_owned());
    args
}

/// The arguments that give a `codex exec` turn its sandbox and network; see `codex_args`.
fn codex_sandbox_args(permissions: CodexPermissions, plan: &TurnPlan<'_>) -> Vec<String> {
    if permissions.needs_permission_profile() {
        let profile = permission_profile_name();
        return vec![
            "--config".to_owned(),
            format!("permissions.{profile}={{extends=\":read-only\",network={{enabled=true}}}}"),
            "--config".to_owned(),
            format!("default_permissions=\"{profile}\""),
        ];
    }
    let sandbox = permissions.sandbox();
    let mut args = if plan.resume.is_some() {
        vec![
            "--config".to_owned(),
            format!("sandbox_mode=\"{}\"", sandbox.as_str()),
        ]
    } else {
        vec!["--sandbox".to_owned(), sandbox.as_str().to_owned()]
    };
    args.extend(workspace_network_args(permissions));
    args
}

/// The overrides that keep a `codex exec` turn from being approved past its sandbox.
fn unattended_approval_args() -> Vec<String> {
    vec![
        "--config".to_owned(),
        "approval_policy=\"never\"".to_owned(),
        "--config".to_owned(),
        "approvals_reviewer=\"user\"".to_owned(),
    ]
}

/// The override that decides a workspace-write delegate's network, when one was chosen.
///
/// Read-only without network needs none, since the legacy read-only mode has none, and the
/// full-access mode has no network switch at all.
fn workspace_network_args(permissions: CodexPermissions) -> Vec<String> {
    match (permissions.sandbox(), permissions.network_access) {
        (CodexSandbox::WorkspaceWrite, Some(network)) => vec![
            "--config".to_owned(),
            format!("sandbox_workspace_write.network_access={network}"),
        ],
        (CodexSandbox::WorkspaceWrite, None)
        | (CodexSandbox::ReadOnly | CodexSandbox::DangerFullAccess, _) => Vec::new(),
    }
}

/// Argument vector that starts `codex app-server` for one turn.
///
/// The effort travels as an argument, exactly as `exec` passes it, so it applies to every request
/// the server handles, and so does a workspace-write delegate's network choice; the model, the
/// sandbox mode and the approval policy are fields of the `thread/start` or `thread/resume`
/// request instead, and a read-only delegate's network is the sandbox policy of its `turn/start`.
/// There is no isolation flag to pass: see [`prepare`] for why only an inheriting delegate runs
/// this way.
pub(crate) fn codex_app_server_args(
    effort: Option<&Effort>,
    permissions: CodexPermissions,
) -> Vec<String> {
    let mut args = vec!["app-server".to_owned()];
    if let Some(effort) = effort {
        args.push("--config".to_owned());
        args.push(format!("model_reasoning_effort=\"{effort}\""));
    }
    args.extend(workspace_network_args(permissions));
    args
}

/// Whether the codex that printed `cli_version` for `--version` is one whose `exec` is verified
/// to honour a permission profile passed with `-c`, which read-only with network needs.
///
/// An unknown version is not verified: a release that ignored the profile's keys would run the
/// delegate under whatever its configuration selects instead, so the launch is refused rather
/// than run on a guess.
#[must_use]
pub fn codex_supports_permission_profiles(cli_version: Option<&str>) -> bool {
    cli_version
        .and_then(release_of)
        .is_some_and(|release| release >= crate::config::PERMISSION_PROFILE_MIN_CODEX)
}
