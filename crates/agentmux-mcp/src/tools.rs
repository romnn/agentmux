//! The eight tools.
//!
//! Changes when the tool API changes.

use std::sync::Arc;
use std::time::Duration;

use agentmux::run::{RunStore, StartRequest};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::params::{DelegateParams, QuestionParams, run_id};
use crate::render;

/// Default seconds a blocking tool waits.
///
/// Codex's `tool_timeout_sec` defaults to sixty seconds, so a default of sixty would race the
/// host's own kill on every call.
/// Forty-five leaves room for the request and the reply.
const DEFAULT_WAIT_SECONDS: u64 = 45;

/// Longest wait any tool will honour, safely inside Claude Code's ten-minute default.
pub(crate) const MAX_WAIT_SECONDS: u64 = 540;

/// Default bytes of transcript returned inline by `result`.
///
/// Claude Code caps tool results, and a truncated result is worse than a pointer to the file, so
/// the default is well under the cap and the path is always given.
const DEFAULT_RESULT_BYTES: usize = 40_000;

/// Default bytes returned by `tail`.
///
/// Small on purpose.
/// A caller watching a forty-minute review pulls this much text every poll, for a report it will
/// read again in full through `result`; at twenty thousand bytes a ten-poll watch costs more
/// context than the answer.
const DEFAULT_TAIL_BYTES: usize = 4_000;

/// Largest slice any tool will return inline, whatever was asked for.
const MAX_SLICE_BYTES: usize = 400_000;

/// The agentmux MCP server.
#[derive(Clone)]
pub struct AgentMux {
    store: Arc<RunStore>,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for AgentMux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentMux")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Naming a consultation.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunParams {
    /// The consultation id, exactly as `ask`, `start` or `list` reported it.
    /// If you no longer have it, call `list` — it shows recent consultations with their ids and
    /// questions.
    pub run_id: String,
}

/// Arguments for `ask`: who to consult, what to ask, and how long to wait.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskParams {
    /// Who to consult.
    #[serde(flatten)]
    #[schemars(flatten)]
    pub delegate: DelegateParams,
    /// What to ask.
    #[serde(flatten)]
    #[schemars(flatten)]
    pub question: QuestionParams,

    /// Seconds to wait for the answer before returning the consultation id instead.
    /// Defaults to 45, which stays inside Codex's 60-second tool timeout; capped at 540.
    /// Running out of time is not a failure and never discards the consultation — collect it later
    /// with `result`.
    #[serde(default)]
    #[schemars(with = "u64")]
    pub wait_seconds: Option<u64>,
}

/// Arguments for `start`: who to consult and what to ask.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartParams {
    /// Who to consult.
    #[serde(flatten)]
    #[schemars(flatten)]
    pub delegate: DelegateParams,
    /// What to ask.
    #[serde(flatten)]
    #[schemars(flatten)]
    pub question: QuestionParams,
}

/// Arguments for `tail`: which consultation to follow, and from where.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TailParams {
    /// The consultation id, from `ask`, `start` or `list`.
    pub run_id: String,

    /// Byte offset to read from.
    /// Pass `0` the first time, then the `next_cursor` of the previous call.
    /// Cursors only move forward, so a stale one is safe.
    #[serde(default)]
    #[schemars(with = "u64")]
    pub cursor: Option<u64>,

    /// Most bytes to return.
    /// Defaults to 4000, which is deliberately small: this is for watching, not for reading.
    #[serde(default)]
    #[schemars(with = "usize")]
    pub max_bytes: Option<usize>,
}

/// Arguments for `result`: which consultation to collect, and which page of it.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResultParams {
    /// The consultation id, from `ask`, `start` or `list`.
    pub run_id: String,

    /// Seconds to block for the consultation to finish before returning what exists.
    /// Defaults to 0, which returns immediately; capped at 540.
    #[serde(default)]
    #[schemars(with = "u64")]
    pub wait_seconds: Option<u64>,

    /// Byte offset to read from, for paging a long transcript.
    /// Same coordinates as `tail`.
    #[serde(default)]
    #[schemars(with = "u64")]
    pub offset: Option<u64>,

    /// Most bytes to return.
    /// Defaults to 40000.
    /// Read the transcript file for the whole thing.
    #[serde(default)]
    #[schemars(with = "usize")]
    pub max_bytes: Option<usize>,
}

/// Arguments for `follow_up`: which consultation to continue, and with what.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FollowUpParams {
    /// The consultation id to continue.
    /// It does not change: a consultation is one conversation, and the new turn is appended to the
    /// same transcript.
    pub run_id: String,

    /// The follow-up question.
    /// The delegate still has the previous turns, so ask only the new thing — do not restate the
    /// brief.
    pub question: String,

    /// Seconds to wait for the answer.
    /// Defaults to 45; capped at 540.
    #[serde(default)]
    #[schemars(with = "u64")]
    pub wait_seconds: Option<u64>,
}

/// Arguments for `list`: how many consultations to show.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Most consultations to list, newest first.
    /// Defaults to 20.
    #[serde(default)]
    #[schemars(with = "usize")]
    pub limit: Option<usize>,
}

/// Wrap rendered prose as a tool result.
///
/// Plain text rather than `structuredContent`: Claude Code's handling of structured results is
/// unreliable, and the reader is a model that will act on the prose either way.
fn text(body: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(body)])
}

fn wait(seconds: Option<u64>, default: u64) -> Duration {
    Duration::from_secs(seconds.unwrap_or(default).min(MAX_WAIT_SECONDS))
}

fn slice(bytes: Option<usize>, default: usize) -> usize {
    bytes.unwrap_or(default).clamp(1, MAX_SLICE_BYTES)
}

// The generated constructor stays crate-private and `crate::tool_router` documents
// and re-exports it, because the macro emits no doc comment of its own.
#[tool_router(router = tool_router, vis = "pub(crate)")]
impl AgentMux {
    /// Build a server over an already-open run store.
    #[must_use]
    pub fn new(store: Arc<RunStore>) -> Self {
        Self {
            store,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Put a question to another vendor's coding agent and wait for the answer. \
                       Use it for a question you expect answered in under a minute — a factual \
                       cross-check, an opinion on one file. A review takes far longer than any \
                       host lets a tool block, so a review belongs in `start`. Returns the \
                       delegate's complete transcript, never just its last message. If the answer \
                       has not arrived it returns the consultation id and keeps working; nothing \
                       is lost and `result` collects it later. Do not call `ask` again for the \
                       same question — that starts a second consultation and pays twice."
    )]
    async fn ask(
        &self,
        Parameters(params): Parameters<AskParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let status = self
            .store
            .start(&StartRequest {
                delegate: params.delegate.build()?,
                question: params.question.text()?,
                cwd: params.question.working_dir()?,
                retention: params.question.retention(),
            })
            .map_err(|error| run_error(&error))?;

        let status = self
            .store
            .wait_until_terminal(
                &status.run_id,
                wait(params.wait_seconds, DEFAULT_WAIT_SECONDS),
            )
            .await
            .map_err(|error| run_error(&error))?;

        if !status.is_terminal() {
            return Ok(text(render::status(&status)));
        }
        let page = self
            .store
            .read_transcript(&status.run_id, 0, DEFAULT_RESULT_BYTES)
            .map_err(|error| run_error(&error))?;
        Ok(text(render::transcript(&status, &page)))
    }

    #[tool(
        description = "Begin a consultation and return its id immediately, without waiting. Use \
                       this for work that will take many minutes — a deep review, a whole-repo \
                       analysis — then watch it with `tail` or block for the answer with \
                       `result`. The delegate runs detached and survives an agentmux restart."
    )]
    fn start(
        &self,
        Parameters(params): Parameters<StartParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let status = self
            .store
            .start(&StartRequest {
                delegate: params.delegate.build()?,
                question: params.question.text()?,
                cwd: params.question.working_dir()?,
                retention: params.question.retention(),
            })
            .map_err(|error| run_error(&error))?;
        Ok(text(render::status(&status)))
    }

    #[tool(
        description = "Report how a consultation is doing: state, elapsed time, message count, \
                       token usage and cost, and whether anything happened that makes the \
                       transcript need careful reading. Cheap; safe to poll."
    )]
    fn status(
        &self,
        Parameters(params): Parameters<RunParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let status = self
            .store
            .status(&run_id(&params.run_id)?)
            .map_err(|error| run_error(&error))?;
        Ok(text(render::status(&status)))
    }

    #[tool(
        description = "Show the transcript written since a cursor, so you can see what the \
                       delegate is actually doing while it works. Prefer `status` for a polling \
                       loop — it is cheaper and carries no transcript text; reach for `tail` when \
                       you want to see the work rather than the state. This shows a slice from \
                       your cursor, and the findings are usually not in it: once the state is \
                       terminal, read `result` from the top."
    )]
    fn tail(
        &self,
        Parameters(params): Parameters<TailParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = run_id(&params.run_id)?;
        let status = self.store.status(&id).map_err(|error| run_error(&error))?;
        let page = self
            .store
            .read_transcript(
                &id,
                params.cursor.unwrap_or(0),
                slice(params.max_bytes, DEFAULT_TAIL_BYTES),
            )
            .map_err(|error| run_error(&error))?;
        Ok(text(render::tail(&status, &page)))
    }

    #[tool(
        description = "Return a consultation's complete transcript, in order, including anything \
                       a hook injected, clearly marked. With `wait_seconds` it blocks up to that \
                       long for the consultation to finish first. Always also names the \
                       transcript file, which holds the whole thing when the reply is truncated."
    )]
    async fn result(
        &self,
        Parameters(params): Parameters<ResultParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = run_id(&params.run_id)?;
        let status = match params.wait_seconds {
            Some(seconds) if seconds > 0 => self
                .store
                .wait_until_terminal(&id, wait(Some(seconds), 0))
                .await
                .map_err(|error| run_error(&error))?,
            _ => self.store.status(&id).map_err(|error| run_error(&error))?,
        };
        let page = self
            .store
            .read_transcript(
                &id,
                params.offset.unwrap_or(0),
                slice(params.max_bytes, DEFAULT_RESULT_BYTES),
            )
            .map_err(|error| run_error(&error))?;
        Ok(text(render::transcript(&status, &page)))
    }

    #[tool(
        description = "Ask a finished consultation one more question. It resumes the delegate's \
                       own session, so the delegate still has everything from the earlier turns \
                       and you do not restate the brief. The consultation id does not change; the \
                       new turn is appended to the same transcript."
    )]
    async fn follow_up(
        &self,
        Parameters(params): Parameters<FollowUpParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = run_id(&params.run_id)?;
        if params.question.trim().is_empty() {
            return Err(ErrorData::invalid_params(
                "`question` is empty. A follow-up needs something to ask.".to_owned(),
                None,
            ));
        }
        let status = self
            .store
            .follow_up(&id, &params.question)
            .map_err(|error| run_error(&error))?;
        let status = self
            .store
            .wait_until_terminal(
                &status.run_id,
                wait(params.wait_seconds, DEFAULT_WAIT_SECONDS),
            )
            .await
            .map_err(|error| run_error(&error))?;

        if !status.is_terminal() {
            return Ok(text(render::status(&status)));
        }
        let page = self
            .store
            .read_transcript(&status.run_id, 0, DEFAULT_RESULT_BYTES)
            .map_err(|error| run_error(&error))?;
        Ok(text(render::transcript(&status, &page)))
    }

    #[tool(
        description = "Stop a running consultation. Everything the delegate said before the stop \
                       is kept and still readable with `result`. Calling it on a consultation \
                       that has already finished changes nothing and simply reports its state."
    )]
    fn cancel(
        &self,
        Parameters(params): Parameters<RunParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let status = self
            .store
            .cancel(&run_id(&params.run_id)?)
            .map_err(|error| run_error(&error))?;
        Ok(text(render::status(&status)))
    }

    #[tool(
        description = "List recent consultations with their ids, delegates, states and questions, \
                       newest first. Use it to recover an id you did not keep."
    )]
    fn list(
        &self,
        Parameters(params): Parameters<ListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let runs = self
            .store
            .list(params.limit.unwrap_or(20).clamp(1, 200))
            .map_err(|error| run_error(&error))?;
        Ok(text(render::list(&runs)))
    }
}

/// Turn a store error into something the calling agent can act on.
///
/// The reader is a model that will decide what to do next from this string alone, so every arm
/// says what happened and what to try instead.
fn run_error(error: &agentmux::run::RunError) -> ErrorData {
    use agentmux::run::RunError;
    match error {
        // Something the caller supplied, or a state it can wait out.
        // Its own message already says what to do next.
        RunError::NotFound(_)
        | RunError::BadRunId(_)
        | RunError::NotResumable(_)
        | RunError::TurnAlreadyClaimed { .. }
        | RunError::Delegate(_) => ErrorData::invalid_params(error.to_string(), None),
        RunError::Launch(inner) => ErrorData::internal_error(
            format!(
                "{inner}\nagentmux runs the vendor's own CLI, so that CLI must be installed and \
                 logged in on this machine."
            ),
            None,
        ),
        RunError::Corrupt { .. } | RunError::Io { .. } | RunError::NoStateDir => {
            ErrorData::internal_error(error.to_string(), None)
        }
    }
}

/// What a host injects into the calling agent's context, once.
const INSTRUCTIONS: &str = r#"agentmux runs a question past the OTHER vendor's coding agent — `codex` when you are Claude Code,
`claude` when you are Codex — and keeps the whole transcript. Your own harness already spawns
same-vendor subagents natively and more cheaply; agentmux exists to cross the vendor line, which is
where a disagreement is worth paying for.

A consultation is slow and durable. `start` returns a run id immediately and the delegate runs
detached, surviving a restart of this server. Then poll `status` — it is cheap and carries no
transcript text — about once a minute; a review at high effort takes five to forty minutes. Use
`tail` when you want to see what the delegate is actually doing, `result` to collect the answer,
and `cancel` to stop one while keeping what it collected. `ask` is `start` plus a 45-second wait,
for a question you expect answered quickly; a review is not that. A call that comes back "still
running" has lost nothing, and `list` recovers an id you did not keep. `follow_up` continues the
delegate's own session, so ask only the new thing.

The transcript is the deliverable and the last message is not the answer. No tool returns "the
answer", deliberately: a delegate's report is routinely followed by wrap-up, a warning, or a turn
that a hook reopened after it had finished, and the findings are above all of that. Read `result`
from the top, or open the file at `transcript:` with your own file tools — the inline text is
bounded by your host's result limit, the file never is.

Choose a delegate by naming `delegate` (`claude` or `codex`), the exact `model` identifier that
vendor's CLI spells, and an `effort`. agentmux keeps no model list and checks neither: it forwards
both verbatim and hands you the CLI's own refusal, which names what it does accept. Pin them every
time — inheriting a default is how an expensive question reaches a cheap model.

Write the question for a stranger. The delegate shares no conversation with you and cannot ask you
anything: name the paths it should read and the shape of the answer you want. It reads your project
from `cwd`, is offered no editing tools, and never inherits your session's identity, settings,
hooks or MCP servers."#;

// The generated `list_tools`/`get_tool` are async by the trait's shape and await nothing, which
// the macro cannot change.
#[allow(
    clippy::unused_async_trait_impl,
    reason = "the tool_handler macro generates these; the trait requires them to be async"
)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentMux {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentmux", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
