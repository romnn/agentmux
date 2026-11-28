//! Anthropic's `claude` CLI: its permission mode and tool lists, and the arguments it runs with.

pub mod fold;
pub mod quota;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use strum::VariantArray;

use crate::accounting::{ReadSpend, Scope, Tokens};
use crate::delegate::{
    CredentialVars, DelegateError, Effort, Invocation, Isolation, LaunchRequest, ModelId,
    PreparedLaunch, TurnPlan, parse_token, release_of, reroutes_within, string_newtype_conversions,
};
use crate::run::LaunchMode;
use crate::transcript::Usage;

/// What a Claude turn whose supervisor holds its input runs with, beside the arguments
/// `claude --print` has anyway.
///
/// - `--input-format stream-json`: the question, and then each steer, arrive as lines of input
///   for as long as stdin is open.
/// - `--replay-user-messages`: Claude echoes each input line as it takes it, with the id it was
///   sent under, which is what places a steer in the transcript and proves it arrived.
/// - `--permission-prompts none`: nobody is there to answer a permission prompt over the open
///   input, so anything that would prompt is denied rather than waited for.
const CLAUDE_STREAM_INPUT_ARGS: [&str; 5] = [
    "--input-format",
    "stream-json",
    "--replay-user-messages",
    "--permission-prompts",
    "none",
];

/// How a run store launches `invocation`, Claude's own CLI for one turn.
///
/// The turn reads its input as stream-json from its supervisor, when it has one and the CLI
/// starts as itself rather than through a Windows `.cmd` shim, whose process a cancellation could
/// not stop the CLI through; otherwise it reads its question once.
pub(crate) fn prepare(mut invocation: Invocation, request: &LaunchRequest<'_>) -> PreparedLaunch {
    let mode = if request.supervisor && crate::launch::starts_natively(&invocation) {
        invocation.push_args(&CLAUDE_STREAM_INPUT_ARGS);
        LaunchMode::ClaudeStreamInput
    } else {
        LaunchMode::DirectCli
    };
    PreparedLaunch {
        invocation,
        mode,
        encoded_question: None,
        driver_plan: None,
    }
}

/// The first Claude Code release whose `total_cost_usd` is the session's running total across
/// `--resume` rather than the one call's, as Anthropic's cost-tracking documentation says.
const CLAUDE_SESSION_COST_SINCE: (u64, u64, u64) = (2, 1, 277);

/// What a Claude turn's report, from the release `cli_version` names, says it spent.
///
/// Its tokens cover the one call, and count the prompt tokens read from and written to the cache
/// beside its `input_tokens`.
/// Its `total_cost_usd` covered the one call until Claude Code 2.1.277, and has been the
/// session's running total across `--resume` since.
/// A price whose release cannot be read has no known scope, so it is not counted rather than
/// guessed at.
pub(crate) fn read_spend(usage: &Usage, cli_version: Option<&str>) -> ReadSpend {
    ReadSpend {
        tokens: Tokens::cache_beside_input(usage),
        tokens_cover: Scope::Turn,
        price_covers: cli_version.and_then(release_of).map(|release| {
            if release < CLAUDE_SESSION_COST_SINCE {
                Scope::Turn
            } else {
                Scope::Session
            }
        }),
    }
}

/// Credential variables the Claude CLI reads.
///
/// Withheld when an account is named, so the account authenticates as itself rather than as
/// whatever key is exported.
/// With no account named they are not forwarded either, because every one of them is a
/// [billing name](crate::delegate::is_billing_env_name) and a delegate without an account runs on
/// its subscription.
///
/// `CLAUDE_CONFIG_DIR` is deliberately not among them, although it is where a login lives.
/// The process that launches agentmux is usually a Claude Code session, and that variable is how
/// such a session names its *own* identity; a delegate started from it must not quietly become a
/// second session on the same login.
/// A no-account Claude delegate therefore runs against the CLI's default directory, and any other
/// login is reached by naming an account.
/// Codex is the other way round: `CODEX_HOME` is forwarded, because a Codex session does not set
/// it for itself and it is the only way a relocated Codex login is found at all.
pub(crate) const CREDENTIAL_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
];

/// The variables a Claude account's fields map onto.
pub(crate) const CREDENTIAL_VARS: CredentialVars = CredentialVars {
    config_dir: "CLAUDE_CONFIG_DIR",
    api_key: "ANTHROPIC_API_KEY",
    auth_token: Some("ANTHROPIC_AUTH_TOKEN"),
    base_url: "ANTHROPIC_BASE_URL",
};

/// Whether `upper`, a name in upper case, puts Claude on API pricing or another endpoint, beyond
/// what [`crate::delegate::is_billing_env_name`] catches for every vendor.
///
/// That is a name it reads outright: a bearer token it prefers over its logged-in profile, a
/// header override that can carry a key of its own, or a switch onto a cloud provider's metered
/// API; or an endpoint or a token among its own variables.
pub(crate) fn is_billing_env_name(upper: &str) -> bool {
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

/// Claude's configuration directory, as it resolves it from `env`: `CLAUDE_CONFIG_DIR`, or
/// `~/.claude` when that is unset or empty.
pub(crate) fn config_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    env.get(CREDENTIAL_VARS.config_dir)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(crate::config::home_dir(env)?.join(".claude")))
}

/// Where Claude keeps a transcript per session, which `--resume` reads back.
pub(crate) fn session_store(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    Some(config_dir(env)?.join("projects"))
}

/// A Claude permission mode, spelled as `claude --permission-mode` takes it.
///
/// These are not a scale of strength: `plan` explores without editing, `acceptEdits` accepts
/// edits, `dontAsk` denies whatever would need approval, `auto` lets a classifier decide, and
/// `bypassPermissions` skips every permission check, which a launch may ask for only when the
/// machine file lists it in `claude.limits.allowed_permission_modes`.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    VariantArray,
)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum ClaudePermissionMode {
    /// Asks before acting; Claude Code's own default, also spelled `default`.
    #[serde(rename = "manual", alias = "default")]
    #[cfg_attr(feature = "clap", value(name = "manual", alias = "default"))]
    Manual,
    /// Accepts file edits without asking.
    #[serde(rename = "acceptEdits")]
    #[cfg_attr(feature = "clap", value(name = "acceptEdits"))]
    AcceptEdits,
    /// Explores and plans without editing: what a second opinion runs under unless configured
    /// otherwise.
    #[default]
    #[serde(rename = "plan")]
    #[cfg_attr(feature = "clap", value(name = "plan"))]
    Plan,
    /// Denies whatever would need approval, rather than asking.
    #[serde(rename = "dontAsk")]
    #[cfg_attr(feature = "clap", value(name = "dontAsk"))]
    DontAsk,
    /// Lets Claude Code's classifier decide what to approve.
    #[serde(rename = "auto")]
    #[cfg_attr(feature = "clap", value(name = "auto"))]
    Auto,
    /// Skips every permission check.
    #[serde(rename = "bypassPermissions")]
    #[cfg_attr(feature = "clap", value(name = "bypassPermissions"))]
    BypassPermissions,
}

impl ClaudePermissionMode {
    /// The mode as `--permission-mode` and agentmux's configuration spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
            Self::Auto => "auto",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

impl std::fmt::Display for ClaudePermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One Claude tool name or permission rule, such as `WebFetch` or `Bash(git diff *)`, passed to
/// the CLI verbatim.
///
/// Opaque for the same reason a model id is: the vocabulary is the CLI's.
/// A rule is held to what one entry of the CLI's comma-separated lists can carry, so it may hold
/// spaces and brackets but no comma.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClaudeTool(String);

impl ClaudeTool {
    /// Longest rule accepted.
    pub const MAX_LEN: usize = 256;

    /// Validate one tool name or rule.
    ///
    /// # Errors
    ///
    /// Returns [`DelegateError::Argument`] when the rule is empty, over [`ClaudeTool::MAX_LEN`],
    /// starts with `-`, or holds a comma or a control character.
    pub fn parse(value: &str) -> Result<Self, DelegateError> {
        parse_token("tool", value, Self::MAX_LEN, |c| {
            c != ',' && !c.is_control()
        })
        .map(Self)
    }

    /// Validate every entry of a list, refusing the first that does not parse.
    ///
    /// # Errors
    ///
    /// Returns the first entry's [`DelegateError::Argument`].
    pub fn parse_all<S: AsRef<str>>(values: &[S]) -> Result<Vec<Self>, DelegateError> {
        values
            .iter()
            .map(|value| Self::parse(value.as_ref()))
            .collect()
    }

    /// The rule as the CLI will see it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

string_newtype_conversions!(ClaudeTool);

/// What a Claude delegate may do: its permission mode and which tools it is offered.
///
/// Every field is optional in a request, where an absent value takes the machine file's default.
/// A recorded consultation names its permission mode; an absent one, and an absent tool list,
/// mean what agentmux launches with when nobody chose, which is also what every consultation ran
/// under before these were recorded: plan mode and a read-only tool list, `Bash`, `Read`, `Grep`,
/// `Glob`, `WebFetch` and `WebSearch`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudePermissions {
    /// The permission mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<ClaudePermissionMode>,
    /// The built-in tools the delegate is offered at all, as `--tools`; empty offers none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ClaudeTool>>,
    /// Calls preapproved, as `--allowedTools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<ClaudeTool>>,
    /// Calls denied, as `--disallowedTools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<ClaudeTool>>,
}

impl ClaudePermissions {
    /// The permission mode, plan mode when none was recorded.
    #[must_use]
    pub fn permission_mode(&self) -> ClaudePermissionMode {
        self.permission_mode.unwrap_or_default()
    }

    /// These permissions, with each one `asked` sets in place of the recorded one.
    pub(crate) fn overridden_by(self, asked: Self) -> Self {
        Self {
            permission_mode: asked.permission_mode.or(self.permission_mode),
            tools: asked.tools.or(self.tools),
            allowed_tools: asked.allowed_tools.or(self.allowed_tools),
            disallowed_tools: asked.disallowed_tools.or(self.disallowed_tools),
        }
    }

    /// How these permissions read in a one-line summary, naming only what departs from plan mode
    /// and the read-only tool list.
    pub(crate) fn describe(&self) -> String {
        let mut parts = Vec::new();
        let mode = self.permission_mode();
        if mode != ClaudePermissionMode::Plan {
            parts.push(format!(" permission_mode={mode}"));
        }
        let lists = [
            ("tools", &self.tools),
            ("allowed_tools", &self.allowed_tools),
            ("disallowed_tools", &self.disallowed_tools),
        ];
        for (name, list) in lists {
            if let Some(list) = list {
                parts.push(format!(" {name}={}", join_tools(list)));
            }
        }
        parts.concat()
    }
}

/// A tool list the way the CLI's flags take it: comma-separated.
fn join_tools(tools: &[ClaudeTool]) -> String {
    tools
        .iter()
        .map(ClaudeTool::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

/// The empty MCP configuration handed to the Claude delegate.
///
/// Without it the delegate loads this machine's MCP servers, including agentmux itself, and a
/// consultation can recurse.
const EMPTY_MCP_CONFIG: &str = r#"{"mcpServers":{}}"#;

/// The built-in tools a Claude delegate may use, unless the consultation or the machine file's
/// `claude.defaults.tools` names others.
///
/// A consultation is a second opinion, not an edit, so `Edit`, `Write` and `NotebookEdit` are not
/// offered.
/// That is a real reduction but not a sandbox: `Bash` is on the list, and a delegate that wanted
/// to write a file could.
/// What actually holds the line is `--permission-mode plan`, which the vendor enforces and reports
/// in prose rather than structurally.
/// Treat a Claude delegate as read-only by cooperation, and a Codex delegate under
/// [`CodexSandbox::ReadOnly`] as read-only by enforcement.
const CLAUDE_READ_ONLY_TOOLS: &str = "Bash,Read,Grep,Glob,WebFetch,WebSearch";

/// Argument vector for the Claude CLI.
///
/// Every flag here is load-bearing:
///
/// - `--output-format stream-json` with `--verbose`: `text` and `json` return only the *last*
///   assistant message, which is not the same thing as the answer; `--verbose` is mandatory
///   alongside `stream-json` under `--print` or the CLI rejects it.
/// - `--setting-sources ""`, for an isolated delegate: loads no user, project or local settings,
///   so no `Stop` hook can reopen the finished turn.
///   This is structural — it does not depend on any hook cooperating.
///   It does not suppress `CLAUDE.md` discovery, so a delegate still sees the project's own
///   instructions.
/// - `--strict-mcp-config` with an empty config, for an isolated delegate: keeps it from
///   re-entering agentmux.
/// - `--permission-mode` and `--tools`, always: plan mode with an edit-free tool list unless the
///   consultation chose otherwise, passed explicitly so a delegate that loads the user's settings
///   does not take its permission mode or tools from them.
///   See [`CLAUDE_READ_ONLY_TOOLS`] for what that does and does not guarantee.
/// - `--allowedTools` and `--disallowedTools`, only when the consultation names rules: a
///   delegate that loads the user's settings otherwise keeps the user's own.
/// - `--add-dir`, once for each directory the turn must read outside its working directory: the
///   consultation a fresh one continues, whose transcript its first question points at.
/// - `--no-session-persistence` is deliberately **absent**.
///   It defeats `--resume`, and a consultation must stay open to a follow-up.
pub(crate) fn claude_args(
    model: &ModelId,
    effort: Option<&Effort>,
    permissions: &ClaudePermissions,
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
    args.push(permissions.permission_mode().as_str().to_owned());
    args.push("--tools".to_owned());
    args.push(
        permissions
            .tools
            .as_deref()
            .map_or_else(|| CLAUDE_READ_ONLY_TOOLS.to_owned(), join_tools),
    );
    // Each list goes as one comma-separated value, which a `ClaudeTool` cannot break: it holds no
    // comma.
    if let Some(allowed) = &permissions.allowed_tools {
        args.push("--allowedTools".to_owned());
        args.push(join_tools(allowed));
    }
    if let Some(disallowed) = &permissions.disallowed_tools {
        args.push("--disallowedTools".to_owned());
        args.push(join_tools(disallowed));
    }
    for dir in plan.read_dirs {
        args.push("--add-dir".to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
    args.push("--output-format".to_owned());
    args.push("stream-json".to_owned());
    args.push("--verbose".to_owned());
    // The permission mode and the tool list are not part of isolation and stay either way: what
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
