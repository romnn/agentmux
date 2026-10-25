//! The eleven tools.
//!
//! Changes when the tool API changes.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use agentmux::delegate::Vendor;
use agentmux::run::{ListFilter, RunId, RunStore, SettledTurn, StartRequest, TurnRef};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProgressNotificationParam, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::params::{
    DelegateParams, ListParams, PermissionParams, QuestionParams, QuotaParams, label, run_id,
};
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
///
/// Held under the smallest host budget agentmux is used from — Claude Code's twenty-five
/// thousand tokens — so that a page cannot be cut short by the host while its header advertises
/// a cursor past the bytes the caller never saw; a caller following that cursor would skip them.
const MAX_SLICE_BYTES: usize = 64_000;

/// The agentmux MCP server.
#[derive(Clone)]
pub struct AgentMux {
    store: Arc<RunStore>,
    /// The host-facing instructions, including this machine's accounts.
    instructions: String,
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
    /// Pass `0` to start at the beginning, or omit it with `from_end`; after that, pass the
    /// `next_cursor` of the previous call.
    /// Cursors only move forward, so a stale one is safe.
    #[serde(default)]
    #[schemars(with = "u64")]
    pub cursor: Option<u64>,

    /// Set to `true` to read the last `max_bytes` of the transcript instead of from a cursor:
    /// what the delegate wrote most recently, however long it has been running.
    /// Its `next_cursor` follows on from there; pass `cursor` or `from_end`, not both.
    #[serde(default)]
    pub from_end: bool,

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

    /// Byte offset to read from.
    ///
    /// Defaults to the end of the turn before this one, so the reply carries the new turn alone:
    /// the earlier turns are already in your context, and re-sending them spends it twice.
    /// Pass `0` for the whole consultation from the top.
    #[serde(default)]
    #[schemars(with = "u64")]
    pub offset: Option<u64>,

    /// Most bytes to return.
    /// Defaults to 40000.
    /// Read the transcript file for the whole thing.
    #[serde(default)]
    #[schemars(with = "usize")]
    pub max_bytes: Option<usize>,

    /// A new short purpose for this turn and the ones after it, such as
    /// `"RE-REVIEW after rework"`: one line of up to 120 characters.
    /// Omitted, the turn keeps the consultation's current label.
    #[serde(default)]
    #[schemars(with = "String")]
    pub label: Option<String>,

    /// Changes to the consultation's isolation or permissions for this turn and the ones after
    /// it; whatever is omitted keeps what the newest turn ran with.
    #[serde(flatten)]
    #[schemars(flatten)]
    pub permissions: PermissionParams,
}

/// Arguments for `wait`: which turn to wait for, and which page of the transcript to return.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitParams {
    /// The consultation id, from `ask`, `start` or `list`.
    pub run_id: String,

    /// The turn to wait for, counted from 1 as the transcript's turn headings count.
    /// Defaults to the newest turn when the call arrives, and stays that turn: a follow-up started
    /// meanwhile is not returned in its place.
    #[serde(default)]
    #[schemars(with = "u32")]
    pub turn: Option<u32>,

    /// Byte offset to read from, in the same coordinates as `result`.
    /// Defaults to 0 for the first turn, and to the start of the turn for a later one, so a
    /// follow-up's reply carries that turn alone.
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

/// Arguments for `steer`: which running consultation, and what to tell it.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SteerParams {
    /// The consultation id, from `ask`, `start` or `list`.
    /// Its turn must still be running; a finished consultation takes `follow_up` instead.
    pub run_id: String,

    /// The instruction to add to the running turn, as self-contained as a follow-up question.
    /// The delegate reads it before its next model call and carries on from where it is, so say
    /// what to change — a correction, a narrower scope, "stop and report what you have so far" —
    /// rather than restating the brief.
    pub message: String,
}

/// Wrap rendered prose as a tool result.
///
/// Plain text rather than `structuredContent`: Claude Code's handling of structured results is
/// unreliable, and the reader is a model that will act on the prose either way.
fn text(body: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(body)])
}

/// The consultation `ask` and `start` begin.
fn start_request(
    delegate: &DelegateParams,
    question: &QuestionParams,
) -> Result<StartRequest, ErrorData> {
    Ok(StartRequest {
        delegate: delegate.build()?,
        question: question.text()?,
        cwd: question.working_dir()?,
        retention: question.retention(),
        env: question.environment(),
        billing: delegate.billing,
        force_codex_exec: delegate.force_codex_exec,
        label: question.label()?,
    })
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
        // Built once, at server start: a host injects instructions into the caller's context one
        // time, and the machine's accounts are what a caller most needs and can least guess.
        // Resolved against the store's own captured environment, so the roster describes the
        // machine a consultation will actually run on, and from the directory the host started
        // the server in, which is where a consultation that names no `cwd` will read.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let instructions = instructions(store.host_env(), &cwd, store.denied_vendors());
        Self {
            tool_router: crate::tool_router(store.denied_vendors()),
            store,
            instructions,
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
            .start(&start_request(&params.delegate, &params.question)?)
            .map_err(|error| run_error(&error))?;

        self.store
            .wait_until_terminal(
                &status.run_id,
                wait(params.wait_seconds, DEFAULT_WAIT_SECONDS),
            )
            .await
            .map_err(|error| run_error(&error))?;
        self.answer(&status.run_id, 0, DEFAULT_RESULT_BYTES)
    }

    #[tool(
        description = "Begin a consultation and return its id immediately, without waiting. Use \
                       this for work that will take many minutes — a deep review, a whole-repo \
                       analysis — then collect it with `wait`, which returns once the turn has \
                       settled, and watch it with `tail` meanwhile if you like. The delegate runs \
                       detached and survives an agentmux restart."
    )]
    fn start(
        &self,
        Parameters(params): Parameters<StartParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let status = self
            .store
            .start(&start_request(&params.delegate, &params.question)?)
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
                       you want to see the work rather than the state; `from_end` shows the \
                       latest output without paging there. This shows a slice, and the findings \
                       are usually not in it: once the state is terminal, read `result` from the \
                       top."
    )]
    fn tail(
        &self,
        Parameters(params): Parameters<TailParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = run_id(&params.run_id)?;
        let max_bytes = slice(params.max_bytes, DEFAULT_TAIL_BYTES);
        let (status, page) = match (params.from_end, params.cursor) {
            (true, Some(_)) => {
                return Err(ErrorData::invalid_params(
                    "pass `cursor` or `from_end`, not both: `from_end` reads the last `max_bytes` \
                     of the transcript, and a cursor reads on from where an earlier call ended."
                        .to_owned(),
                    None,
                ));
            }
            (true, None) => self.store.view_end(&id, max_bytes),
            (false, cursor) => self.store.view(&id, cursor.unwrap_or(0), max_bytes),
        }
        .map_err(|error| run_error(&error))?;
        Ok(text(render::tail(&status, &page)))
    }

    #[tool(
        description = "Return a consultation's complete transcript, in order, including anything \
                       a hook injected, clearly marked. With `wait_seconds` it blocks up to that \
                       long for the consultation to finish first; coming back still running is an \
                       observation, not a completion, and `wait` is what returns only once the \
                       turn has settled. Always also names the transcript file, which holds the \
                       whole thing when the reply is truncated."
    )]
    async fn result(
        &self,
        Parameters(params): Parameters<ResultParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = run_id(&params.run_id)?;
        if let Some(seconds) = params.wait_seconds.filter(|seconds| *seconds > 0) {
            self.store
                .wait_until_terminal(&id, wait(Some(seconds), 0))
                .await
                .map_err(|error| run_error(&error))?;
        }
        let (status, page) = self
            .store
            .view(
                &id,
                params.offset.unwrap_or(0),
                slice(params.max_bytes, DEFAULT_RESULT_BYTES),
            )
            .map_err(|error| run_error(&error))?;
        Ok(text(render::transcript(&status, &page)))
    }

    #[tool(
        description = "Block until a consultation's turn has settled, then return its \
                       transcript. Settled means finished and final: the delegate has exited, \
                       or a cancellation froze the turn although the delegate would not stop, \
                       and the transcript through that turn is published and will never change. It \
                       never returns early with a running state, and it has no time limit of its \
                       own, however long the review takes; while it waits it sends a progress \
                       notification every 30 seconds to a caller that asked for progress. Use it \
                       after `start` instead of polling. A host may still end a long call: Codex \
                       stops one after its `tool_timeout_sec`, and progress does not extend that, \
                       so there wait with the `agentmux wait` command in a shell instead. \
                       Cancelling the call stops the wait, never the consultation."
    )]
    async fn wait(
        &self,
        Parameters(params): Parameters<WaitParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = run_id(&params.run_id)?;
        let target = self
            .store
            .pin_turn(&id, params.turn)
            .map_err(|error| run_error(&error))?;
        let settled = wait_reporting_progress(&self.store, &target, &context).await?;
        let page = settled
            .page(
                params.offset.unwrap_or_else(|| settled.section_offset()),
                slice(params.max_bytes, DEFAULT_RESULT_BYTES),
            )
            .map_err(|error| run_error(&error))?;
        // Rendered from the settlement and what never changes; the newest turn's number is an
        // aside that a later turn's records cannot fail the call over.
        let meta = self.store.meta(&id).map_err(|error| run_error(&error))?;
        // As the settled turn ran, which `status` and `list` say of it while it is the newest.
        let delegate = self
            .store
            .turn_delegate(&meta, &settled)
            .map_err(|error| run_error(&error))?;
        let turn_label = self.store.turn_label(&settled);
        let newest = self
            .store
            .pin_turn(&id, None)
            .ok()
            .map(|newest| newest.turn_number());
        // Decided before the settlement was committed, so it is there for a turn that has one.
        let auto_resume = self
            .store
            .auto_resume(&id, settled.turn_number())
            .map_err(|error| run_error(&error))?;
        Ok(text(render::settled(
            &settled,
            &meta,
            &delegate,
            turn_label.as_ref(),
            newest,
            auto_resume.as_ref(),
            &page,
        )))
    }

    #[tool(
        description = "Ask a finished consultation one more question. It resumes the delegate's \
                       own session, so the delegate still has everything from the earlier turns \
                       and you do not restate the brief. The consultation id does not change; the \
                       new turn is appended to the same transcript, and the reply carries that \
                       turn alone — pass `offset: 0` for the whole thing."
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
        let overrides = params.permissions.options()?;
        let label = label(params.label.as_deref())?;
        // Read before the turn is claimed, so it is the length through the previous turn exactly.
        // The rendered transcript is append-only, which is what makes that a turn boundary and
        // not a cut through the middle of one.
        let before = self
            .store
            .status(&id)
            .map_err(|error| run_error(&error))?
            .transcript_bytes;
        let status = self
            .store
            .follow_up_with(&id, &params.question, overrides, label.as_ref())
            .map_err(|error| run_error(&error))?;
        self.store
            .wait_until_terminal(
                &status.run_id,
                wait(params.wait_seconds, DEFAULT_WAIT_SECONDS),
            )
            .await
            .map_err(|error| run_error(&error))?;
        self.answer(
            &status.run_id,
            params.offset.unwrap_or(before),
            slice(params.max_bytes, DEFAULT_RESULT_BYTES),
        )
    }

    #[tool(
        description = "Add an instruction to a consultation whose turn is still running — a \
                       correction, a narrower scope, \"stop and report what you have\" — which \
                       the delegate sees before its next model call, without waiting for the turn \
                       to end. Only a turn launched over Codex's app-server takes one, which \
                       `status` shows as `steerable: yes`; anything else is refused with the \
                       reason, and a finished consultation takes `follow_up` instead. Returns \
                       once the delegate has accepted the message, which the transcript then \
                       records as a `### user (steer)` section where it arrived."
    )]
    async fn steer(
        &self,
        Parameters(params): Parameters<SteerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let status = self
            .store
            .steer(&run_id(&params.run_id)?, &params.message)
            .await
            .map_err(|error| run_error(&error))?;
        Ok(text(render::steer_confirmation(&status)))
    }

    #[tool(
        description = "Stop a running consultation. Everything the delegate said before the stop \
                       is kept and still readable with `result`. Returns once the delegate has \
                       actually stopped: a few seconds, or up to about twenty for a turn driven \
                       over Codex's app-server, which is first interrupted over its protocol. \
                       On a consultation whose last turn failed because the model was overloaded, \
                       it stops the automatic resume agentmux has scheduled, and the turn stays \
                       failed. Calling it on a consultation that has otherwise finished changes \
                       nothing and simply reports its state."
    )]
    async fn cancel(
        &self,
        Parameters(params): Parameters<RunParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let status = self
            .store
            .cancel(&run_id(&params.run_id)?)
            .await
            .map_err(|error| run_error(&error))?;
        Ok(text(render::status(&status)))
    }

    #[tool(
        description = "List recent consultations with their ids, labels, delegates, states and \
                       questions, newest first. Use it to recover an id you did not keep, or with \
                       `label` and `state` to find the consultations you started for one purpose \
                       and see which are still running or failed."
    )]
    fn list(
        &self,
        Parameters(params): Parameters<ListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let filtered = params.label.is_some() || params.state.is_some();
        let filter = ListFilter {
            label: params.label,
            state: params.state,
        };
        let runs = self
            .store
            .list(params.limit.unwrap_or(20).clamp(1, 200), &filter)
            .map_err(|error| run_error(&error))?;
        Ok(text(render::list(&runs, filtered)))
    }

    #[tool(
        description = "Report what each configured account has left of its usage windows, for \
                       both vendors, in the vendor's own numbers. Free and fast: it spends no \
                       tokens. Use it to choose an `account` before a long consultation, or after \
                       a rate-limited failure to find one that can answer now. Read `severity` \
                       and the per-model windows yourself — agentmux does not rank accounts, \
                       because a percentage means nothing without the plan behind it. An account \
                       reported as unavailable is NOT an idle account. It also lists this \
                       machine's model aliases and blocked models as its file says them now."
    )]
    fn quota(
        &self,
        Parameters(params): Parameters<QuotaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let reported = self
            .store
            .quota(params.delegate)
            .map_err(|error| run_error(&error))?;
        Ok(text(render::quota(&reported)))
    }
}

/// How often a waiting call tells a caller that asked for progress that it is still waiting.
///
/// Claude Code ends a stdio call that says nothing for thirty minutes, and resets that clock on
/// progress; thirty seconds is far inside it without flooding the channel.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// Longest a progress notification may take to send before it is given up, so a host that stops
/// reading cannot hold back the wait it is about.
const PROGRESS_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Wait for `target` to settle, sending progress while it does when the request asked for it.
///
/// Returns the settlement, or an error when the wait fails or the caller cancels the request; the
/// consultation itself is never touched.
async fn wait_reporting_progress(
    store: &RunStore,
    target: &TurnRef,
    context: &RequestContext<RoleServer>,
) -> Result<SettledTurn, ErrorData> {
    let progress_token = context.meta.get_progress_token();
    let started = tokio::time::Instant::now();
    let mut ticks = tokio::time::interval_at(started + PROGRESS_INTERVAL, PROGRESS_INTERVAL);
    let waiting = store.wait_for_settlement(target);
    tokio::pin!(waiting);
    loop {
        tokio::select! {
            settled = &mut waiting => return settled.map_err(|error| run_error(&error)),
            () = context.ct.cancelled() => {
                return Err(ErrorData::internal_error(
                    format!(
                        "the wait for consultation {} turn {} was cancelled; the consultation \
                         carries on",
                        target.run_id(),
                        target.turn_number()
                    ),
                    None,
                ));
            }
            _ = ticks.tick(), if progress_token.is_some() => {
                let Some(token) = progress_token.clone() else {
                    continue;
                };
                let elapsed = started.elapsed().as_secs();
                let notification = ProgressNotificationParam::new(
                    token,
                    f64::from(u32::try_from(elapsed).unwrap_or(u32::MAX)),
                )
                .with_message(format!(
                    "consultation {} turn {} has not settled after {elapsed}s; still waiting",
                    target.run_id(),
                    target.turn_number()
                ));
                match tokio::time::timeout(
                    PROGRESS_SEND_TIMEOUT,
                    context.peer.notify_progress(notification),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => tracing::debug!(?err, "a progress notification was not sent"),
                    Err(_) => tracing::debug!("a progress notification timed out"),
                }
            }
        }
    }
}

impl AgentMux {
    /// The transcript of a consultation once a wait has ended, or its state if it is still going.
    ///
    /// One reading of the files serves both, so the state and the page agree.
    fn answer(
        &self,
        id: &RunId,
        offset: u64,
        max_bytes: usize,
    ) -> Result<CallToolResult, ErrorData> {
        let (status, page) = self
            .store
            .view(id, offset, max_bytes)
            .map_err(|error| run_error(&error))?;
        if !status.is_terminal() {
            return Ok(text(render::status(&status)));
        }
        Ok(text(render::transcript(&status, &page)))
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
        // Whether a steer was refused, withdrawn or left unconfirmed, the message says what the
        // caller can do next.
        | RunError::Steer(_)
        | RunError::TurnAlreadyClaimed { .. }
        | RunError::LegacyRunStillRunning { .. }
        | RunError::StillRunning { .. }
        | RunError::NoSuchTurn { .. }
        | RunError::Delegate(_)
        // The caller is a delegate that inherited its account's MCP servers and found agentmux
        // among them; the remedy is to answer, not to retry.
        | RunError::Recursive
        // The operator denied the vendor, and the message names the subagent to use instead.
        | RunError::VendorDenied { .. }
        // The operator blocked the model, and the message names the one to ask for instead.
        | RunError::ModelBlocked { .. } => ErrorData::invalid_params(error.to_string(), None),
        // A configuration file is read on every launch whatever the call names, so no argument
        // works around it; the message names the file and what to change in it.
        RunError::Config(inner) => ErrorData::invalid_params(
            format!(
                "{inner}\nThis is the machine's agentmux configuration, which its operator \
                 fixes; retrying with other arguments meets the same error."
            ),
            None,
        ),
        RunError::Launch(inner) => ErrorData::internal_error(
            format!(
                "{inner}\nagentmux runs the vendor's own CLI, so that CLI must be installed and \
                 logged in on this machine."
            ),
            None,
        ),
        // A turn that cannot settle, or a settlement that does not hold, is the store's to repair;
        // the message names the files to look at.
        RunError::Corrupt { .. }
        | RunError::CorruptRecord { .. }
        | RunError::InvalidSettlement { .. }
        | RunError::UncertainStart { .. }
        | RunError::Io { .. }
        | RunError::NoStateDir => ErrorData::internal_error(error.to_string(), None),
    }
}

/// What the instructions say about a vendor this server will not launch, if any.
///
/// The reason is worth stating, not just the refusal.
/// A caller told only that a delegate is unavailable retries it with another model or another
/// account; one told the delegation is redundant reaches for the subagent its own harness already
/// gives it, which is the outcome the operator asked for by denying the vendor.
fn denial(denied: &BTreeSet<Vendor>) -> String {
    if denied.is_empty() {
        return String::new();
    }
    let allowed = named(
        Vendor::ALL
            .into_iter()
            .filter(|vendor| !denied.contains(vendor)),
    );
    // The binary refuses a server that denies every vendor, but this library is not the only
    // thing that can build a store.
    let here = if allowed.is_empty() {
        String::new()
    } else {
        format!(" Here, `delegate` is {allowed}.")
    };
    format!(
        "\n\nThis server launches no {} delegates: the harness it serves already spawns those \
         subagents natively, so use its own for that side.{here}",
        named(denied.iter().copied())
    )
}

/// A vendor list as prose, each name backticked and any two joined with "and".
fn named(vendors: impl IntoIterator<Item = Vendor>) -> String {
    vendors
        .into_iter()
        .map(|vendor| format!("`{vendor}`"))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// What a host injects into the calling agent's context, plus this machine's own accounts.
///
/// The roster is appended rather than baked into the schema: a tool schema is sent once and cached
/// by the host, while accounts are machine state, and an enum of them would be wrong the moment the
/// operator edits a file.
/// agentmux still keeps no roster of its own — the machine does, and this only reads it.
fn instructions(
    host_env: &std::collections::BTreeMap<String, String>,
    cwd: &std::path::Path,
    denied: &BTreeSet<Vendor>,
) -> String {
    let denial = denial(denied);
    let machine = match agentmux::config::Config::load_machine(host_env) {
        Ok(machine) => machine,
        Err(error) => {
            // Said out loud: a server that starts fine but names no accounts looks like a machine
            // with none, and the operator has no other signal that their file was rejected.
            tracing::warn!(%error, "the machine configuration could not be read; instructions omit the roster");
            return format!("{INSTRUCTIONS}{denial}");
        }
    };

    // A paragraph of its own, after the accounts, because it answers the next question a caller
    // has: which `model` to name.
    // Read from the machine file alone, the only file that may set a rule, so a checkout's
    // broken file does not hide them; and only for the vendors this server launches, while
    // `quota` reports the machine as a whole.
    let rules: Vec<_> = Vendor::ALL
        .into_iter()
        .filter(|vendor| !denied.contains(vendor))
        .map(|vendor| machine.model_rules(vendor))
        .collect();
    let models = render::model_rules(&rules).map_or_else(String::new, |rules| {
        indoc::formatdoc! {"

            {rules} That list is a snapshot from server start: every launch reads the current file,
            and `quota` re-reads it and lists it again on every call. A blocked model is refused
            with the model to ask for instead, never swapped in for you; a follow-up keeps the
            model its consultation started with.
        "}
    });

    let config = match machine.clone().with_project(host_env, cwd) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "the project configuration could not be read; instructions omit the roster");
            return indoc::formatdoc! {"
                {INSTRUCTIONS}{denial}
                {models}"};
        }
    };
    let roster = roster(&config);
    let permissions = permissions(&machine, denied);

    indoc::formatdoc! {"
        {INSTRUCTIONS}{denial}

        {roster} That was read when this server started, and a host delivers these instructions
        only once, so `quota` is the authority if the two disagree: it re-reads the machine's file
        on every call, as every launch does. It reports what each account has left and costs
        nothing: call it before a long consultation, and again after a rate-limited failure to
        find one that can answer now.
        {models}
        {permissions}

        No API key or endpoint override from the environment reaches a delegate unless its account
        is marked above as one that may use an API key: agentmux removes them from its environment
        and refuses one passed in `env`. Pass `billing` as `api` to insist on the account's key,
        which only such an account accepts, or as `subscription` to withhold it even there;
        follow-ups keep what the first turn ran on.

        Those marks are the machine file's settings. An account a checkout's own agentmux.toml
        selects as its default, in place of the machine's, runs isolated and on subscription
        pricing even when named explicitly: `ignore_user_config: false` still overrides the
        isolation, `billing: api` is refused, and such an account that configures an API
        credential or endpoint is refused.
    "}
}

/// The paragraph saying what a delegate loads and may do: each launched vendor's defaults and
/// limits, read from the machine file alone, which is the only file that may set them.
fn permissions(machine: &agentmux::config::Config, denied: &BTreeSet<Vendor>) -> String {
    let rules: String = Vendor::ALL
        .into_iter()
        .filter(|vendor| !denied.contains(vendor))
        .map(|vendor| {
            indoc::formatdoc! {"
                - {vendor}: {}
            ", machine.describe_permissions(vendor)}
        })
        .collect();
    indoc::formatdoc! {"
        What a delegate loads and may do, as this machine's file said at server start:
        {rules}A delegate with `ignore_user_config=false` loads this machine's own CLI
        configuration: its settings, hooks and MCP servers, so a hook can reopen its finished turn
        and its answer can depend on this machine. Pass `ignore_user_config: true` to skip that
        configuration (a `codex` delegate still applies this machine's exec-policy rules), `false` when the point is to exercise that tooling, and `env`
        to set variables that tooling reads. Within the limits, `sandbox` and `network_access`
        choose what a `codex` delegate's commands may do, and `permission_mode`, `tools`,
        `allowed_tools` and `disallowed_tools` what a `claude` delegate may do; omitted, the
        default above applies. A value beyond a limit is refused, naming the limit, and never
        lowered to fit. A limit is a ceiling on what agentmux requests, not on what this machine's
        own Codex `.rules` allow, which can run a matching command outside the sandbox. `follow_up` takes the same fields to change them for the next turn, and
        keeps what the newest turn ran with for any it omits; the limits are rechecked on every
        turn, and `quota` reports them as they are now."
    }
}

/// The paragraph naming this machine's accounts, and which one an omitted `account` uses.
fn roster(config: &agentmux::config::Config) -> String {
    let mut described = Vec::new();
    for vendor in Vendor::ALL {
        let accounts = config.accounts(vendor);
        if accounts.is_empty() {
            continue;
        }
        let names = accounts
            .iter()
            .map(|(alias, account)| {
                let mut notes = Vec::new();
                if let Some(description) = &account.description {
                    notes.push(description.clone());
                }
                // The one property a caller cannot discover and must not be surprised by.
                match account.inherit_settings {
                    Some(true) => notes
                        .push("LOADS this machine's settings, hooks and MCP servers".to_owned()),
                    Some(false) => {
                        notes.push("runs ISOLATED from this machine's settings".to_owned());
                    }
                    None => {}
                }
                // The other property a caller must not be surprised by: whether a key reaches it.
                if account.billing == Some(agentmux::delegate::Billing::Api) {
                    notes.push("may use an API key".to_owned());
                }
                if notes.is_empty() {
                    format!("`{alias}`")
                } else {
                    format!("`{alias}` ({})", notes.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let default = config.default_account(vendor).map_or_else(
            || format!("the {vendor} CLI's own login"),
            |default| format!("`{}`", default.alias),
        );
        let omitted = format!(
            "omitting `account` uses {default} unless the checkout's own agentmux.toml selects \
             another"
        );
        described.push(format!("{vendor}: {names} — {omitted}"));
    }

    if described.is_empty() {
        "No named accounts were defined when this server started, so omit `account`.".to_owned()
    } else {
        format!(
            "This machine defines these accounts — {}. Pass one as `account`, or omit it for the \
             default; the `delegate:` line of every result names the account that actually ran.",
            described.join("; ")
        )
    }
}

/// The part of the instructions that is the same on every machine.
///
/// [`instructions`] appends the accounts this one has.
const INSTRUCTIONS: &str = r#"agentmux runs a question past the OTHER vendor's coding agent — `codex` when you are Claude Code,
`claude` when you are Codex — and keeps the whole transcript. Your own harness already spawns
same-vendor subagents natively and more cheaply; agentmux exists to cross the vendor line, which is
where a disagreement is worth paying for.

A consultation is slow and durable. `start` returns a run id immediately and the delegate runs
detached, surviving a restart of this server; a review at high effort takes five to forty minutes.
Collect it by waiting for its turn to settle rather than by polling. Settled means final: the
delegate has exited, or a cancellation froze the turn although the delegate would not stop, and
the transcript through that turn is published and will never change. Two
waits return exactly then, never earlier, and have no time limit of their own:

- The `wait` tool blocks the call until the turn settles, and sends a progress notification every
  30 seconds to a caller that asked for progress. Under Claude Code a `wait` the host moved to the
  background reports its completion when the turn has truly finished.
- The `agentmux wait <run_id> --turn <n>` command does the same from a shell, then prints the turn
  and exits 0 if it completed, 10 if it failed and 11 if it was cancelled. Under Claude Code, run
  it with `run_in_background`, and its exit is the completion notice. Under Codex, which ends a
  tool call after its `tool_timeout_sec` however much progress the call reports, run it as a shell
  command and wait for that command to finish. Run it with the `AGENTMUX_STATE_DIR` this server
  was given, if it was given one.

A bounded wait that comes back "still running" — `result` with `wait_seconds`, or `ask` — has
observed the consultation, not seen it finish. It has lost nothing; wait for the turn instead of
calling again. Use `status` for a cheap look that carries no transcript text, `tail` when you want
to see what the delegate is actually doing — with `from_end` for its latest output — `result` to
page the transcript, and `cancel` to stop one while keeping what it collected. `ask` is `start`
plus a 45-second wait, for a question you expect answered quickly; a review is not that. `list` recovers an id you did not keep.
`follow_up` continues the delegate's own session, so ask only the new thing. `steer` adds an
instruction to a turn that is still running, which the delegate sees before its next model call;
only a turn `status` reports as `steerable: yes` takes one, which today means a `codex`
consultation that inherits its account's settings, because only those are launched over Codex's
app-server.

A turn that fails because the vendor's model is overloaded is resumed by agentmux itself: after a
short wait it sends "continue" in the same session as the next turn, waiting longer each time the
model is still overloaded, and gives up after a dozen tries. The failed turn says when, and which
turn to wait for next; `cancel` stops it.

The transcript is the deliverable and the last message is not the answer. No tool returns "the
answer", deliberately: a delegate's report is routinely followed by wrap-up, a warning, or a turn
that a hook reopened after it had finished, and the findings are above all of that. Read `result`
from the top, or open the file at `transcript:` with your own file tools — the inline text is
bounded by your host's result limit, the file never is.

Choose a delegate by naming `delegate` (`claude` or `codex`), a `model` and an `effort`. For the
model, prefer a family alias this machine defines — they are listed below when it defines any, and
`quota` lists them too — and otherwise name the vendor's own identifier; `opus` and `sol` show an
alias's shape, not a list of them. agentmux keeps no built-in model roster; this machine's rewrites
and blocks apply before launch, and the model and effort that run are forwarded verbatim. When the
CLI refuses one, its own refusal comes back, usually naming what it accepts: look a current
identifier up there rather than copying one from an example, which goes stale with the next release.
A result whose model was rewritten names both. Pin the model every time — inheriting a default is
how an expensive question reaches a cheap model. Pin the effort too for anything that matters: left
out, the machine's configured default for that model is used, or the CLI's own when it has none, and
the result says which effort ran.

Write the question for a stranger. The delegate shares no conversation with you and cannot ask you
anything: name the paths it should read and the shape of the answer you want. It reads your project
from `cwd`, is offered no editing tools unless its permissions say otherwise, and never inherits
your session's identity. Unless this machine's agentmux.toml or your request says otherwise, it
does load this machine's own CLI settings, hooks and MCP servers; `quota` reports what this machine
sets."#;

// The generated `list_tools`/`get_tool` are async by the trait's shape and await nothing, which
// the macro cannot change.
#[allow(
    clippy::unused_async_trait_impl,
    reason = "the tool_handler macro generates these; the trait requires them to be async"
)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentMux {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentmux", env!("CARGO_PKG_VERSION")))
            .with_instructions(self.instructions.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use agentmux::appserver::DriverPlan;
    use agentmux::appserver::mailbox::{Mailbox, SteerReply};
    use agentmux::delegate::{
        AccountChoice, CodexPermissions, CodexSandbox, Delegate, Effort, Isolation, ModelId,
    };
    use agentmux::run::{Retention, RunStore, StartRequest};
    use agentmux::testing::{Script, ScriptedLauncher};
    use googletest::prelude::*;
    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::model::ErrorCode;

    use super::{AgentMux, SteerParams};
    use crate::params::QuotaParams;

    /// A machine file defining aliases for both vendors and blocking two Codex models.
    const MODEL_RULES: &str = indoc::indoc! {r#"
        [models.claude]
        "opus" = "claude-opus-5-5[1m]"

        [models.codex]
        "sol" = "gpt-6.1-sol"

        [blocked.codex]
        "gpt-5.6-sol" = "sol"
        "gpt-6-sol" = { use = "sol", reason = "gpt-6.1-sol is much cheaper" }
    "#};

    /// A server over an empty store whose machine file can be rewritten while it runs.
    struct ConfiguredServer {
        server: AgentMux,
        launcher: Arc<ScriptedLauncher>,
        /// Where the machine file lives, and the directory a consultation reads.
        home: tempfile::TempDir,
        #[expect(
            dead_code,
            reason = "holds the store's temporary directory until the test ends; never read"
        )]
        state_directory: tempfile::TempDir,
    }

    impl ConfiguredServer {
        /// Replace the machine file, as an operator editing it under a running server would.
        fn write_config(&self, config: &str) -> Result<()> {
            agentmux::testing::write_config_file(self.home.path().join("agentmux.toml"), config)
                .or_fail()?;
            Ok(())
        }

        /// The text of a `quota` call, for both vendors or one.
        fn quota_text(&self, delegate: Option<agentmux::delegate::Vendor>) -> Result<String> {
            let reported = self
                .server
                .quota(Parameters(QuotaParams { delegate }))
                .or_fail()?;
            let text = reported
                .content
                .first()
                .and_then(|block| block.as_text())
                .or_fail()?;
            Ok(text.text.clone())
        }
    }

    /// A server whose machine file says `config`, launching every vendor but `denied`.
    fn configured_server(
        config: &str,
        denied: &[agentmux::delegate::Vendor],
    ) -> Result<ConfiguredServer> {
        let home = tempfile::tempdir().or_fail()?;
        agentmux::testing::write_config_file(home.path().join("agentmux.toml"), config)
            .or_fail()?;
        let host_env = [
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            (
                "HOME".to_owned(),
                home.path().to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .collect();
        let state_directory = tempfile::tempdir().or_fail()?;
        let launcher = Arc::new(ScriptedLauncher::new([]));
        let store = RunStore::open(
            state_directory.path(),
            Arc::<ScriptedLauncher>::clone(&launcher),
            host_env,
        )
        .or_fail()?
        .with_denied_vendors(denied.iter().copied());
        Ok(ConfiguredServer {
            server: AgentMux::new(Arc::new(store)),
            launcher,
            home,
            state_directory,
        })
    }

    /// The instructions a host delivers once name this machine's aliases and blocked models, and
    /// say where a fresher reading comes from.
    #[gtest]
    fn the_instructions_list_the_machines_aliases_and_blocked_models() -> Result<()> {
        use rmcp::ServerHandler as _;

        let configured = configured_server(MODEL_RULES, &[])?;

        let instructions = configured.server.get_info().instructions.or_fail()?;

        assert_that!(
            instructions,
            contains_substring(
                "Model aliases on this machine — claude: `opus` → `claude-opus-5-5[1m]`; codex: \
                 `sol` → `gpt-6.1-sol`."
            )
        );
        assert_that!(
            instructions,
            contains_substring(
                "Blocked models, refused when a new consultation would launch them — codex \
                 `gpt-5.6-sol`: use model \"sol\" instead; codex `gpt-6-sol`: use model \"sol\" \
                 instead (gpt-6.1-sol is much cheaper)."
            )
        );
        // A snapshot, and the caller is told which call re-reads it.
        assert_that!(
            instructions,
            contains_substring("`quota` re-reads it and lists it again on every call")
        );
        // Set apart from the accounts above and the isolation paragraph below.
        assert_that!(
            instructions,
            contains_substring(indoc::indoc! {"
                answer now.

                Model aliases on this machine"})
        );
        assert_that!(
            instructions,
            contains_substring(indoc::indoc! {"
                started with.

                What a delegate loads"})
        );
        Ok(())
    }

    /// A machine that defines no aliases and blocks nothing adds no model paragraph, and leaves
    /// the paragraphs around it as they were.
    #[gtest]
    fn the_instructions_omit_model_rules_when_there_are_none() -> Result<()> {
        use rmcp::ServerHandler as _;

        let configured = configured_server(
            indoc::indoc! {r#"
                [accounts.claude.work]
                config_dir = "/tmp"
            "#},
            &[],
        )?;

        let instructions = configured.server.get_info().instructions.or_fail()?;

        assert_that!(instructions, not(contains_substring("Model aliases on")));
        assert_that!(instructions, not(contains_substring("Blocked models")));
        assert_that!(
            instructions,
            contains_substring(indoc::indoc! {"
                answer now.

                What a delegate loads"})
        );
        Ok(())
    }

    /// A server that launches only one vendor lists only that vendor's rules in its instructions,
    /// while `quota` still reports the machine's rules for both, as it reports both vendors'
    /// accounts.
    #[gtest]
    fn a_denied_vendors_rules_are_left_out_of_the_instructions_only() -> Result<()> {
        use agentmux::delegate::Vendor;
        use rmcp::ServerHandler as _;

        let configured = configured_server(MODEL_RULES, &[Vendor::Claude])?;

        let instructions = configured.server.get_info().instructions.or_fail()?;
        assert_that!(
            instructions,
            contains_substring("Model aliases on this machine — codex: `sol` → `gpt-6.1-sol`.")
        );
        assert_that!(instructions, not(contains_substring("claude-opus-5-5[1m]")));
        assert_that!(
            instructions,
            contains_substring("codex `gpt-5.6-sol`: use model \"sol\" instead")
        );

        let quota = configured.quota_text(None)?;
        assert_that!(quota, contains_substring("`opus` → `claude-opus-5-5[1m]`"));
        assert_that!(quota, contains_substring("`sol` → `gpt-6.1-sol`"));
        Ok(())
    }

    /// `quota` lists the aliases and blocked models as the machine file says them now, for the
    /// vendors it was asked about.
    ///
    /// A host delivers the instructions once, so a long-running server's snapshot goes stale;
    /// this is the call a caller makes to see the current rules.
    #[gtest]
    fn quota_lists_the_current_aliases_and_blocked_models() -> Result<()> {
        let configured = configured_server(
            indoc::indoc! {r#"
                [models.claude]
                "opus" = "claude-opus-5-5[1m]"
            "#},
            &[],
        )?;
        // Edited after the server started, so only a fresh reading can know it.
        configured.write_config(MODEL_RULES)?;

        let both = configured.quota_text(None)?;
        assert_that!(both, contains_substring("`opus` → `claude-opus-5-5[1m]`"));
        assert_that!(both, contains_substring("`sol` → `gpt-6.1-sol`"));
        assert_that!(
            both,
            contains_substring(
                "codex `gpt-6-sol`: use model \"sol\" instead (gpt-6.1-sol is much cheaper)"
            )
        );

        // Asked about one vendor, it lists that vendor's rules alone, and says it blocks none.
        let claude_only = configured.quota_text(Some(agentmux::delegate::Vendor::Claude))?;
        assert_that!(
            claude_only,
            contains_substring("`opus` → `claude-opus-5-5[1m]`")
        );
        assert_that!(
            claude_only,
            contains_substring("No blocked models for claude.")
        );
        assert_that!(claude_only, not(contains_substring("gpt-5.6-sol")));
        Ok(())
    }

    /// Rules removed after the server started are reported as gone, not left unsaid.
    ///
    /// The instructions still advertise what the file said at start, and silence in `quota` would
    /// leave that snapshot standing; saying there is none is what corrects it.
    #[gtest]
    fn quota_says_when_rules_have_been_removed_since_start() -> Result<()> {
        let configured = configured_server(MODEL_RULES, &[])?;

        // One category removed
        configured.write_config(indoc::indoc! {r#"
            [models.codex]
            "sol" = "gpt-6.1-sol"
        "#})?;
        let blocks_removed = configured.quota_text(None)?;
        assert_that!(blocks_removed, contains_substring("`sol` → `gpt-6.1-sol`"));
        assert_that!(blocks_removed, contains_substring("No blocked models."));
        assert_that!(blocks_removed, not(contains_substring("gpt-5.6-sol")));

        // Both categories cleared
        configured.write_config(indoc::indoc! {r#"
            [accounts.claude.work]
            config_dir = "/tmp"
        "#})?;
        let cleared = configured.quota_text(None)?;
        assert_that!(
            cleared,
            contains_substring("No model aliases on this machine. No blocked models.")
        );
        assert_that!(cleared, not(contains_substring("gpt-6.1-sol")));
        Ok(())
    }

    /// A blocked model is refused through `start` and through `ask` as invalid parameters the
    /// caller can act on, naming the model to ask for instead, and nothing is launched.
    #[gtest]
    #[tokio::test]
    async fn the_start_and_ask_tools_refuse_a_blocked_model() -> Result<()> {
        let configured = configured_server(MODEL_RULES, &[])?;
        let arguments = serde_json::json!({
            "delegate": "codex",
            "model": "gpt-6-sol",
            "effort": "high",
            "question": "Review the diff.",
            "cwd": configured.home.path(),
        });

        let started = configured.server.start(Parameters(
            serde_json::from_value(arguments.clone()).or_fail()?,
        ));
        let asked = configured
            .server
            .ask(Parameters(serde_json::from_value(arguments).or_fail()?))
            .await;

        for refused in [started, asked] {
            let refusal = refused.err().or_fail()?;
            assert_that!(refusal.code, eq(ErrorCode::INVALID_PARAMS));
            // The store's own tests own the wording; this proves it reaches the caller.
            assert_that!(
                refusal.message.as_ref(),
                contains_substring("use model \"sol\"")
            );
        }
        assert_that!(configured.launcher.launches(), is_empty());
        Ok(())
    }

    /// The text of a tool's result.
    fn result_text(result: &rmcp::model::CallToolResult) -> Result<String> {
        let text = result
            .content
            .first()
            .and_then(|block| block.as_text())
            .or_fail()?;
        Ok(text.text.clone())
    }

    /// A label given to `start` is shown by `status` and `list`, `list` finds the consultation by
    /// it and by its state, and a label that is not one short line is refused before anything
    /// launches.
    #[gtest]
    fn the_start_and_list_tools_carry_a_label() -> Result<()> {
        let state_directory = tempfile::tempdir().or_fail()?;
        let launcher = Arc::new(ScriptedLauncher::new([
            Script::completed(agentmux::testing::fixtures::CODEX_HAPPY),
            Script::completed(agentmux::testing::fixtures::CODEX_HAPPY),
        ]));
        let host_env = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        let store = RunStore::open(state_directory.path(), launcher.clone(), host_env).or_fail()?;
        let server = AgentMux::new(Arc::new(store));
        let start = |label: Option<&str>| -> Result<std::result::Result<_, rmcp::ErrorData>> {
            let mut arguments = serde_json::json!({
                "delegate": "codex",
                "model": "gpt-6-astra",
                "effort": "high",
                "question": "Review the diff.",
                "cwd": state_directory.path(),
            });
            if let (Some(label), Some(object)) = (label, arguments.as_object_mut()) {
                object.insert("label".to_owned(), label.into());
            }
            let params = serde_json::from_value(arguments).or_fail()?;
            Ok(server.start(Parameters(params)))
        };

        let started = result_text(&start(Some(" REVIEW CRD G1 "))?.or_fail()?)?;
        assert_that!(started, contains_substring("label:      REVIEW CRD G1"));
        start(None)?.or_fail()?;
        let refusal = start(Some("two\nlines"))?.err().or_fail()?;
        assert_that!(refusal.code, eq(ErrorCode::INVALID_PARAMS));
        assert_that!(launcher.launches().len(), eq(2));

        let list = |arguments: serde_json::Value| -> Result<String> {
            let params = serde_json::from_value(arguments).or_fail()?;
            result_text(&server.list(Parameters(params)).or_fail()?)
        };
        let found = list(serde_json::json!({ "label": "crd", "state": "completed" }))?;
        assert_that!(found, contains_substring("    label: REVIEW CRD G1"));
        assert_that!(found.matches(" turn(s) ").count(), eq(1));
        let none = list(serde_json::json!({ "state": "running" }))?;
        assert_that!(none, starts_with("no consultations match"));
        Ok(())
    }

    /// A steerable consultation behind an MCP server, and the thread playing its driver.
    struct SteerTestContext {
        server: AgentMux,
        run_id: String,
        driver_thread: std::thread::JoinHandle<std::io::Result<Option<String>>>,
        #[expect(
            dead_code,
            reason = "holds the store's temporary directory until the test ends; never read"
        )]
        state_directory: tempfile::TempDir,
    }

    /// A server over a store whose one Codex turn is running, driven over the app-server, with
    /// a thread playing the driver: it answers the first steer with `reply`, and hands back the
    /// text it claimed.
    fn start_steerable_run(reply: SteerReply) -> Result<SteerTestContext> {
        let state_directory = tempfile::tempdir().or_fail()?;
        let launcher = Arc::new(ScriptedLauncher::new([Script::running(indoc::indoc! {r#"
            {"type":"thread.started","thread_id":"th"}
            {"type":"turn.started"}
        "#})]));
        let host_env = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        let store = RunStore::open(state_directory.path(), launcher.clone(), host_env)
            .or_fail()?
            .with_codex_app_server("/opt/agentmux");
        let run_status = store.start(&StartRequest {
            delegate: Delegate::Codex {
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                effort: Some(Effort::parse("high").or_fail()?),
                permissions: CodexPermissions {
                    sandbox: Some(CodexSandbox::ReadOnly),
                    network_access: None,
                },
                account: AccountChoice::Default,
                isolation: Some(Isolation::Inherit),
            },
            question: "q".to_owned(),
            cwd: state_directory.path().to_path_buf(),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })?;
        // The mailbox the driver was pointed at, from the plan it was launched with.
        let launch = launcher.launches().into_iter().next().or_fail()?;
        let plan = DriverPlan::read(Path::new(launch.args.get(1).or_fail()?)).or_fail()?;
        let mailbox = Mailbox::new(plan.mailbox_dir());
        let driver_thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Some(claim) = mailbox.claim_all().into_iter().next() {
                    mailbox.write_reply(&claim, &reply)?;
                    return Ok(Some(claim.text));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None)
        });
        Ok(SteerTestContext {
            server: AgentMux::new(Arc::new(store)),
            run_id: run_status.run_id.to_string(),
            driver_thread,
            state_directory,
        })
    }

    fn steer_params(run_id: &str, message: &str) -> Parameters<SteerParams> {
        Parameters(SteerParams {
            run_id: run_id.to_owned(),
            message: message.to_owned(),
        })
    }

    /// The tool hands the driver the caller's message exactly, returns once the delegate
    /// accepted it, and says where it was recorded.
    #[gtest]
    #[tokio::test]
    async fn the_steer_tool_delivers_the_whole_message() -> Result<()> {
        let context = start_steerable_run(SteerReply::Accepted)?;
        let message = indoc::indoc! {"
            Narrow it to «auth»,
            and leave the tests alone."};

        let steer_result = context
            .server
            .steer(steer_params(&context.run_id, message))
            .await;

        let claimed_message = context.driver_thread.join().ok().or_fail()?.or_fail()?;
        assert_that!(claimed_message, some(eq(message)));
        let response_debug = format!("{:?}", steer_result.or_fail()?.content);
        assert_that!(response_debug, contains_substring("### user (steer)"));
        Ok(())
    }

    /// A refusal is the caller's to act on, so it comes back as invalid parameters with the
    /// store's own reason, and a malformed id is refused before anything is sent.
    #[gtest]
    #[tokio::test]
    async fn the_steer_tool_reports_a_refusal_as_invalid_parameters() -> Result<()> {
        let context = start_steerable_run(SteerReply::Rejected {
            reason: "no active turn".to_owned(),
        })?;
        let server = &context.server;

        let steer_result = server
            .steer(steer_params(&context.run_id, "too late"))
            .await;
        let rejection_error = steer_result.err().or_fail()?;
        assert_that!(rejection_error.code, eq(ErrorCode::INVALID_PARAMS));
        assert_that!(
            rejection_error.message.as_ref(),
            contains_substring("no active turn")
        );

        let invalid_id_result = server.steer(steer_params("not-an-id", "x")).await;
        assert_that!(
            invalid_id_result.err().map(|err| err.code),
            some(eq(ErrorCode::INVALID_PARAMS))
        );
        Ok(())
    }
}
