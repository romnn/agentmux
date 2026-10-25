//! The shapes a caller fills in, and how they become core types.
//!
//! Changes when the tool arguments change.
//!
//! # Why the delegate is flat here and an enum inside
//!
//! [`agentmux::delegate::Delegate`] is an enum whose variants carry only what that vendor accepts,
//! so a Codex consultation with a Claude account does not compile.
//! A JSON Schema cannot express that without a discriminated union, and a discriminated union is
//! the shape language models fill in wrong most often.
//! So the wire shape is flat and [`DelegateParams::build`] is the parse step: one place, one error
//! message, and the enum's guarantee intact everywhere behind it.

use agentmux::delegate::{
    Billing, ClaudePermissionMode, ClaudeTool, CodexSandbox, Delegate, DelegateError, Isolation,
    PermissionOptions, Vendor,
};
use agentmux::run::{Retention, RunId, RunLabel};
use agentmux::transcript::TurnState;
use rmcp::ErrorData;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use strum::VariantArray;

/// Build the schema for a string-valued choice as a flat `enum`.
///
/// The derive would emit a `$ref` into `$defs` holding a `oneOf` of `const`s, and attach the
/// field's description as a sibling of that `$ref`.
/// Both halves are a problem here.
/// `oneOf`-of-`const` is the shape models fill in least reliably and that strict function-calling
/// modes rewrite, and hosts routinely drop `$ref` siblings — which on `delegate` would discard the
/// one sentence that tells a caller to pick the vendor they are not.
/// A flat `{"type": "string", "enum": [...]}` survives all of that.
///
/// The fields carry the core crate's own enums, and the values listed are those enums' variants
/// as serde spells them, so the schema offers exactly what the field deserializes.
fn string_enum_schema<T: VariantArray + Serialize>(description: &str) -> Schema {
    let values: Vec<serde_json::Value> = T::VARIANTS
        .iter()
        .filter_map(|variant| serde_json::to_value(variant).ok())
        .collect();
    json_schema!({
        "type": "string",
        "enum": values,
        "description": description,
    })
}

fn vendor_schema(_generator: &mut SchemaGenerator) -> Schema {
    string_enum_schema::<Vendor>("Which delegate CLI to run.")
}

fn billing_schema(_generator: &mut SchemaGenerator) -> Schema {
    string_enum_schema::<Billing>("The pricing to run on: `subscription` or `api`.")
}

fn sandbox_schema(_generator: &mut SchemaGenerator) -> Schema {
    string_enum_schema::<CodexSandbox>(
        "`codex` only: how much its commands may write. Codex's own sandbox modes.",
    )
}

fn state_schema(_generator: &mut SchemaGenerator) -> Schema {
    string_enum_schema::<TurnState>("Only consultations whose newest turn stands so.")
}

fn permission_mode_schema(_generator: &mut SchemaGenerator) -> Schema {
    string_enum_schema::<ClaudePermissionMode>(
        "`claude` only: the permission mode, as `claude --permission-mode` spells it.",
    )
}

/// Who to consult, and on what terms.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DelegateParams {
    /// Which CLI to run.
    /// Choose the vendor you are NOT: your own harness already spawns same-vendor subagents
    /// natively, so agentmux is for the cross-vendor second opinion.
    #[schemars(schema_with = "vendor_schema")]
    pub delegate: Vendor,

    /// Model identifier.
    /// agentmux keeps no built-in model roster, so a model released tomorrow works today; this
    /// machine's rewrites and blocks apply before launch, and the identifier that runs is passed
    /// to the delegate CLI verbatim.
    /// Prefer a family alias this machine's `agentmux.toml` defines, which the server
    /// instructions and `quota` list, and otherwise the vendor's own identifier; `opus` and `sol`
    /// show an alias's shape, not a list of them.
    /// When the vendor CLI refuses an identifier, its own error comes back, usually naming what it
    /// accepts: look a current identifier up there rather than copying one from an example, which
    /// goes stale with the next release.
    /// A rewritten model is named beside the one asked for in the result, and a blocked one is
    /// refused naming the model to ask for instead.
    pub model: String,

    /// Reasoning effort, passed verbatim: usually `high` or `xhigh`.
    /// Set it deliberately for anything that matters; a review left to a default is how it
    /// silently runs at the wrong depth.
    /// Omitted, the effort this machine's configuration names for the model that runs is used,
    /// and where it names none the CLI's own default runs; the result names what was used.
    #[serde(default)]
    #[schemars(with = "String")]
    pub effort: Option<String>,

    /// Which configured account to authenticate as, for either vendor.
    ///
    /// Omit it to use the account `agentmux.toml` names as the default, or the CLI's own login
    /// when it names none, which is right on a machine with one account of that vendor.
    /// Aliases are defined per machine in `agentmux.toml`; naming one that does not exist returns
    /// an error listing the ones that do, so guessing is cheap to recover from.
    #[serde(default)]
    #[schemars(with = "String")]
    pub account: Option<String>,

    /// Whether the delegate loads the user's configuration, and what it may do.
    #[serde(flatten)]
    #[schemars(flatten)]
    pub permissions: PermissionParams,

    /// Whether the delegate's environment may carry an API key; omit it and the account decides.
    ///
    /// No key from the environment reaches a delegate unless this machine's `agentmux.toml` marks
    /// its account `billing = "api"`, which the account list in these instructions shows.
    /// `api` is refused for any other account, so it cannot switch API pricing on; it only makes a
    /// consultation meant to use the account's key fail rather than run without it.
    /// `subscription` withholds the key even from an account that allows one, and is refused for
    /// one that authenticates only with a key.
    /// Follow-ups keep whatever the first turn was allowed.
    #[serde(default)]
    #[schemars(schema_with = "billing_schema")]
    pub billing: Option<Billing>,

    /// Set to `true` to run a Codex consultation with `codex exec` rather than over
    /// `codex app-server`; its follow-ups keep it.
    ///
    /// A consultation run this way cannot be steered.
    /// Use it when an earlier Codex consultation failed with a protocol error, which means the
    /// installed codex no longer speaks the app-server protocol this agentmux was built for.
    #[serde(rename = "codex_exec", default)]
    pub force_codex_exec: bool,
}

/// Whether the delegate loads the user's configuration, and what it may do.
///
/// Every field is optional.
/// On `start` and `ask` an omitted one takes this machine's default, which the server
/// instructions and `quota` list; on `follow_up` it keeps what the newest turn ran with.
/// A value beyond this machine's limits is refused, never lowered to fit, and the refusal names
/// what may be asked for instead.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct PermissionParams {
    /// Whether the delegate skips the user's own CLI configuration: settings, hooks and MCP
    /// servers.
    ///
    /// Omitted, this machine's default for the vendor decides, which the server instructions
    /// state; a project-selected account always runs with `true` unless you pass `false`.
    /// Pass `true` when the answer must not depend on this machine.
    /// With `false` a hook can reopen the delegate's finished turn, so its last message is then
    /// not the answer; the delegate gains the MCP servers the configuration names; and the same
    /// question asked elsewhere may answer differently.
    /// A `codex` consultation with `true` runs over `codex exec` and cannot be steered.
    #[serde(default)]
    #[schemars(with = "bool")]
    pub ignore_user_config: Option<bool>,

    /// The inverse of `ignore_user_config`, accepted for older callers; pass one or the other.
    #[serde(default)]
    #[schemars(with = "bool")]
    pub inherit_settings: Option<bool>,

    /// `codex` only.
    /// `read_only` is right for a review.
    /// Use `workspace_write` only when the delegate must write a file itself — a read-only
    /// delegate told to write its report completes the work, fails the write, and reports the
    /// failure instead of the findings.
    /// `danger_full_access` lifts the sandbox and is refused unless this machine allows it.
    #[serde(default)]
    #[schemars(schema_with = "sandbox_schema")]
    pub sandbox: Option<CodexSandbox>,

    /// `codex` only: whether the delegate's commands may reach the network, such as `cargo`
    /// fetching a crate or `gh` reading an issue.
    /// Omitted, this machine's default decides, or Codex's own configuration when it has none.
    #[serde(default)]
    #[schemars(with = "bool")]
    pub network_access: Option<bool>,

    /// `claude` only: the permission mode; `plan` unless this machine's default says otherwise.
    /// `bypassPermissions` is refused unless this machine allows it.
    #[serde(default)]
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: Option<ClaudePermissionMode>,

    /// `claude` only: the built-in tools offered at all, such as `["Read", "Grep", "WebFetch"]`;
    /// `[]` offers none.
    /// Omitted, a read-only list: `Bash`, `Read`, `Grep`, `Glob`, `WebFetch` and `WebSearch`.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub tools: Option<Vec<String>>,

    /// `claude` only: tool calls to preapprove, such as `["WebFetch", "Bash(cargo check *)"]`.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub allowed_tools: Option<Vec<String>>,

    /// `claude` only: tool calls to deny, such as `["Bash(git push *)"]`.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub disallowed_tools: Option<Vec<String>>,
}

impl PermissionParams {
    /// Parse the flat wire shape into the options the core applies.
    ///
    /// # Errors
    ///
    /// Returns an invalid-params error when the two isolation spellings disagree or a tool rule
    /// is not one the CLI can take.
    pub fn options(&self) -> Result<PermissionOptions, ErrorData> {
        let tools =
            |list: &Option<Vec<String>>| list.as_deref().map(ClaudeTool::parse_all).transpose();
        let parsed = (|| {
            Ok::<_, DelegateError>(PermissionOptions {
                isolation: Isolation::from_spellings(
                    self.ignore_user_config,
                    self.inherit_settings,
                )?,
                sandbox: self.sandbox,
                network_access: self.network_access,
                permission_mode: self.permission_mode,
                tools: tools(&self.tools)?,
                allowed_tools: tools(&self.allowed_tools)?,
                disallowed_tools: tools(&self.disallowed_tools)?,
            })
        })();
        parsed.map_err(|error| invalid_delegate(&error))
    }
}

/// An invalid-params error for a refused delegate argument, saying what shape would have been
/// accepted, because the caller is a model that will retry from this text alone.
pub(crate) fn invalid_delegate(error: &DelegateError) -> ErrorData {
    let hint = match error {
        DelegateError::Argument { field: "model", .. } => {
            " `model` is passed to the CLI as a single argument, so it must look like a model \
             identifier."
        }
        DelegateError::Argument {
            field: "effort", ..
        } => " Try `high` or `xhigh`.",
        DelegateError::Argument {
            field: "account", ..
        } => " An account alias is a name from `agentmux.toml`.",
        DelegateError::Argument { field: "tool", .. } => {
            " A tool is a name such as `Read`, or a rule such as `Bash(git diff *)`, with no comma."
        }
        DelegateError::NotForVendor { .. } => {
            " `sandbox` and `network_access` are Codex's; `permission_mode` and the tool lists are \
             Claude's."
        }
        _ => "",
    };
    ErrorData::invalid_params(format!("{error}.{hint}"), None)
}

impl DelegateParams {
    /// Parse the flat wire shape into the closed enum.
    ///
    /// Only shapes are checked here; whether an alias exists is a property of the machine, and
    /// answering that at the edge would mean loading configuration before the working directory
    /// that selects it is known.
    ///
    /// # Errors
    ///
    /// Returns an invalid-params error when the model, effort, alias or a tool rule is not
    /// argv-safe, when the two isolation spellings disagree, or when a vendor-specific field was
    /// set for the other vendor.
    pub fn build(&self) -> Result<Delegate, ErrorData> {
        Delegate::from_parts(
            self.delegate,
            &self.model,
            self.effort.as_deref(),
            self.account.as_deref(),
            self.permissions.options()?,
        )
        .map_err(|error| invalid_delegate(&error))
    }
}

/// What to ask, and where the delegate reads the project from.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct QuestionParams {
    /// The question, in full.
    /// The delegate starts with **no** conversation context and cannot see anything you have said,
    /// so restate everything it needs: the goal, the files or diff to read, the scope, and the
    /// output format you want back.
    /// A one-line question gets a one-line answer.
    pub question: String,

    /// Absolute path to the checkout the delegate reads; it is the only project directory it
    /// sees.
    /// Defaults to the directory agentmux was started in, which is **not** where you are working
    /// if you are in a git worktree or a nested checkout — pass it explicitly then, or the
    /// delegate returns a confident review of code that was never under review.
    #[serde(default)]
    #[schemars(with = "String")]
    pub cwd: Option<String>,

    /// Extra environment variables for the delegate process, as a flat string map.
    ///
    /// Only the names this machine's `agentmux.toml` lists under `request_env` are accepted,
    /// and by default it lists none: anything else is refused, because a request must not be able
    /// to decide which identity pays, which program runs, or what it loads before it reads a
    /// setting.
    /// Standing settings belong in `agentmux.toml`, which applies them to every launch.
    #[serde(default)]
    #[schemars(with = "std::collections::BTreeMap<String, String>")]
    pub env: Option<std::collections::BTreeMap<String, String>>,

    /// Keep the consultation indefinitely, so a follow-up can arrive at any time.
    /// Without this it is deleted once its delegate has written nothing for a week, or for the
    /// `retention` the machine's `agentmux.toml` sets.
    /// Set it only when you expect to ask a follow-up after that window, because nothing deletes
    /// a kept consultation automatically — a person runs `agentmux prune <run_id>`.
    #[serde(default)]
    #[schemars(with = "bool")]
    pub keep: Option<bool>,

    /// A short purpose for the consultation, such as `"REVIEW CRD G1"`: one line of up to 120
    /// characters.
    /// `status`, `list` and `wait` show it, and `list` finds consultations by it, so you need no
    /// note of your own of what each id is for.
    #[serde(default)]
    #[schemars(with = "String")]
    pub label: Option<String>,
}

impl QuestionParams {
    /// The question, rejected if empty.
    ///
    /// # Errors
    ///
    /// Returns an invalid-params error when the question has no content.
    pub fn text(&self) -> Result<String, ErrorData> {
        if self.question.trim().is_empty() {
            return Err(ErrorData::invalid_params(
                "`question` is empty. The delegate starts with no context, so the question has to \
                 carry everything it needs."
                    .to_owned(),
                None,
            ));
        }
        Ok(self.question.clone())
    }

    /// The environment the caller asked for, defaulting to none.
    #[must_use]
    pub fn environment(&self) -> std::collections::BTreeMap<String, String> {
        self.env.clone().unwrap_or_default()
    }

    /// The working directory, defaulting to the server's own.
    ///
    /// # Errors
    ///
    /// Returns an invalid-params error when `cwd` is not an existing directory.
    pub fn working_dir(&self) -> Result<std::path::PathBuf, ErrorData> {
        let Some(cwd) = &self.cwd else {
            return std::env::current_dir().map_err(|error| {
                ErrorData::internal_error(
                    format!(
                        "agentmux cannot determine its own working directory: {error}. Pass \
                             `cwd` explicitly."
                    ),
                    None,
                )
            });
        };
        let path = std::path::PathBuf::from(cwd);
        if !path.is_absolute() {
            return Err(ErrorData::invalid_params(
                format!(
                    "`cwd` must be an absolute path; you passed {cwd:?}. A relative path resolves \
                     against agentmux's own directory rather than yours, so it silently picks a \
                     checkout you did not choose. Pass the absolute path, or omit `cwd`."
                ),
                None,
            ));
        }
        if !path.is_dir() {
            return Err(ErrorData::invalid_params(
                format!(
                    "`cwd` {cwd} is not a directory. Pass an absolute path to the project the \
                         delegate should read, or omit it to use agentmux's own directory."
                ),
                None,
            ));
        }
        Ok(path)
    }

    /// The label, checked.
    ///
    /// # Errors
    ///
    /// Returns an invalid-params error for a label that is not one short line.
    pub fn label(&self) -> Result<Option<RunLabel>, ErrorData> {
        label(self.label.as_deref())
    }

    /// How long the consultation is kept.
    #[must_use]
    pub fn retention(&self) -> Retention {
        if self.keep.unwrap_or(false) {
            Retention::UntilReleased
        } else {
            Retention::Ttl
        }
    }
}

/// Parse a label supplied by a caller, when it supplied one.
///
/// # Errors
///
/// Returns an invalid-params error naming the accepted shape.
pub fn label(value: Option<&str>) -> Result<Option<RunLabel>, ErrorData> {
    value
        .map(RunLabel::parse)
        .transpose()
        .map_err(|error| ErrorData::invalid_params(format!("{error}."), None))
}

/// Parse a run id supplied by a caller.
///
/// # Errors
///
/// Returns an invalid-params error naming the accepted shape and how to recover a real id.
pub fn run_id(value: &str) -> Result<RunId, ErrorData> {
    RunId::parse(value).map_err(|error| {
        ErrorData::invalid_params(
            format!("{error}. Call `list` to see the ids of recent consultations."),
            None,
        )
    })
}

/// Arguments for `quota`: which vendor's accounts to ask about.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct QuotaParams {
    /// Ask only this vendor's accounts.
    /// Both are asked when omitted, which is usually what you want before choosing where to send
    /// a consultation.
    #[serde(default)]
    #[schemars(schema_with = "vendor_schema")]
    pub delegate: Option<Vendor>,
}

/// Arguments for `list`: how many consultations to show, and which.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Most consultations to list, newest first.
    /// Defaults to 20.
    #[serde(default)]
    #[schemars(with = "usize")]
    pub limit: Option<usize>,

    /// Only consultations whose label holds this text, ignoring case: the label `start` gave it,
    /// or the one its newest turn has.
    #[serde(default)]
    #[schemars(with = "String")]
    pub label: Option<String>,

    /// Only consultations whose newest turn stands so: `running`, `completed`, `failed` or
    /// `cancelled`.
    #[serde(default)]
    #[schemars(schema_with = "state_schema")]
    pub state: Option<TurnState>,
}
