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

use agentmux::delegate::{CodexSandbox, Delegate, DelegateError, Isolation, Vendor};
use agentmux::run::{Retention, RunId};
use rmcp::ErrorData;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::Deserialize;

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
/// The fields carry the core crate's own enums — their `snake_case` serde names are the wire
/// spellings — and only the *schema* is written by hand here, so the two cannot disagree about
/// which values exist.
fn string_choice(description: &str, values: &[&str]) -> Schema {
    json_schema!({
        "type": "string",
        "enum": values,
        "description": description,
    })
}

fn vendor_schema(_generator: &mut SchemaGenerator) -> Schema {
    string_choice("Which delegate CLI to run.", &["claude", "codex"])
}

fn sandbox_schema(_generator: &mut SchemaGenerator) -> Schema {
    string_choice(
        "How much of the working directory a Codex delegate may write.",
        &["read_only", "workspace_write"],
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

    /// Model identifier, passed to the delegate CLI verbatim and never checked against a list —
    /// a model released tomorrow works today.
    /// Use the vendor's full identifier, for example `claude-opus-5` or `claude-fable-5-1` for
    /// `claude`, `gpt-6-astra` or `gpt-5.6-sol` for `codex`.
    /// If the identifier is wrong the vendor's own error comes back and names what it accepts.
    pub model: String,

    /// Reasoning effort, passed verbatim: usually `high` or `xhigh`.
    /// Always set it deliberately; leaving it to the delegate's configured default is how a review
    /// silently runs at the wrong depth.
    pub effort: String,

    /// Which configured account to authenticate as, for either vendor.
    ///
    /// Omit it to use the account `agentmux.toml` names as the default, or the CLI's own login
    /// when it names none, which is right on a machine with one account of that vendor.
    /// Aliases are defined per machine in `agentmux.toml`; naming one that does not exist returns
    /// an error listing the ones that do, so guessing is cheap to recover from.
    #[serde(default)]
    #[schemars(with = "String")]
    pub account: Option<String>,

    /// Whether the delegate loads the account's own settings, hooks and MCP servers.
    ///
    /// Omit this and the account decides.
    /// Most accounts are isolated, but one may be configured to inherit, so the account list in
    /// these instructions is the only way to know which.
    /// Pass `false` to force isolation when you need an answer that does not depend on this
    /// machine.
    ///
    /// Pass `true` only when the point of the consultation is to exercise the tooling that
    /// configuration sets up.
    /// It costs three things: a hook can reopen the delegate's finished turn, so its last message
    /// is then not the answer; the delegate gains whatever MCP servers that account configures;
    /// and the same question asked elsewhere may answer differently.
    #[serde(default)]
    #[schemars(with = "bool")]
    pub inherit_settings: Option<bool>,

    /// `codex` only.
    /// `read_only` is right for a review.
    /// Use `workspace_write` only when the delegate must write a file itself — a read-only
    /// delegate told to write its report completes the work, fails the write, and reports the
    /// failure instead of the findings.
    #[serde(default)]
    #[schemars(schema_with = "sandbox_schema")]
    pub sandbox: Option<CodexSandbox>,
}

impl DelegateParams {
    /// The isolation the caller asked for, or `None` to let the account decide.
    fn isolation(&self) -> Option<Isolation> {
        self.inherit_settings.map(|inherit| {
            if inherit {
                Isolation::Inherit
            } else {
                Isolation::Isolated
            }
        })
    }

    /// Parse the flat wire shape into the closed enum.
    ///
    /// Only shapes are checked here; whether an alias exists is a property of the machine, and
    /// answering that at the edge would mean loading configuration before the working directory
    /// that selects it is known.
    ///
    /// # Errors
    ///
    /// Returns an invalid-params error when the model, effort or alias is not argv-safe, or when
    /// a vendor-specific field was set for the other vendor.
    pub fn build(&self) -> Result<Delegate, ErrorData> {
        Delegate::from_parts(
            self.delegate,
            &self.model,
            &self.effort,
            self.account.as_deref(),
            self.isolation(),
            self.sandbox,
        )
        .map_err(|error| {
            // Each refusal says what shape would have been accepted, because the caller is a
            // model that will retry from this text alone.
            let hint = match &error {
                DelegateError::Argument { field: "model", .. } => {
                    " `model` is passed to the CLI as a single argument, so it must look like a \
                     model identifier."
                }
                DelegateError::Argument {
                    field: "effort", ..
                } => " Try `high` or `xhigh`.",
                DelegateError::Argument {
                    field: "account", ..
                } => " An account alias is a name from `agentmux.toml`.",
                DelegateError::NotForVendor { .. } => {
                    " A `claude` delegate runs in plan mode and is offered no editing tools."
                }
                _ => "",
            };
            ErrorData::invalid_params(format!("{error}.{hint}"), None)
        })
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
    /// Without this it is deleted 24 hours after it started.
    /// Set it only when you expect to ask a follow-up after that window, because nothing deletes
    /// a kept consultation automatically — a person runs `agentmux prune <run_id>`.
    #[serde(default)]
    #[schemars(with = "bool")]
    pub keep: Option<bool>,
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
