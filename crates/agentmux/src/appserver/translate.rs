//! Turns `codex app-server` notifications into the events `codex exec --json` writes.
//!
//! The capture a turn leaves is folded by [`crate::stream::codex`] whichever way the turn ran, so
//! this is the only place that knows the two shapes differ.
//! Notifications are decoded into the types generated from codex's own schema; the events written
//! are the `exec` stream's, which is agentmux's capture format.
//!
//! The reference is `codex exec` itself, which in codex-cli 0.159.3 is a client of this same
//! protocol: its JSON output (`EventProcessorWithJsonOutput` in `codex-rs/exec`) maps the same
//! notifications, and this module follows that mapping wherever a reader could tell the
//! difference.
//! What is translated, and into what:
//!
//! - `turn/started` becomes `turn.started`, once.
//! - `item/completed` becomes `item.completed` for exactly the item kinds `exec` reports:
//!   `agentMessage`, `reasoning` (its summary), `commandExecution`, `fileChange`, `mcpToolCall`,
//!   `collabAgentToolCall` and `webSearch`, under their `exec` names and with `exec`'s
//!   snake-case values.
//!   Every other kind is left out, as `exec` leaves it out; a `plan` item is remembered, because
//!   `exec` closes a turn whose only answer is a plan with that plan's text.
//! - A `warning`, a `configWarning`, a `deprecationNotice` and a `model/rerouted` become an
//!   advisory `error` item, which the fold renders as a note.
//! - An `error` becomes `exec`'s top-level `error`, where it arrived, whether or not the server
//!   will retry, so a server that dies before the turn ends still leaves it in the transcript.
//!   The last one is also the failure of a failed turn that names none of its own, as in `exec`.
//! - `thread/tokenUsage/updated` is remembered, and its running total becomes the usage on
//!   `turn.completed`, as `exec` reports the total since its process started, and zero when there
//!   was none.
//! - `turn/completed` becomes `turn.completed` or `turn.failed`; an interrupted turn the caller
//!   cancelled becomes nothing, so the store's own record of the cancellation decides it.
//! - Any other notification the pinned schema names is left out, as `exec` leaves it out.
//!
//! A notification that does not decode, whose method the pinned schema never had, or that carries
//! a type, status or kind the pinned schema does not name, is written as
//! [`CaptureEvent::Undecoded`] with its payload, which the fold counts as drift: a codex whose
//! protocol moved away from the generated types is loud rather than lossy, the raw capture still
//! holds what it said, a reply is still kept, and a turn that ended is still ended.
//!
//! What is written is [`CaptureEvent`], the one model the fold reads too, and its `exec` events
//! are the [`exec`] mirror of codex's own types.

use codex_app_server_protocol::exec::{self, ThreadEvent as Exec, ThreadItemDetails as Details};
use codex_app_server_protocol::methods::{self as m, AnyServerNotification as Any};
use codex_app_server_protocol::{Method as _, Payload, types as wire};
use serde_json::Value;

use super::protocol::{ThreadId, TurnId};
use crate::stream::codex::{CaptureEvent, Undecoded};

/// How the turn ended, as far as the stream says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Finish {
    /// The turn completed; a terminal event was written.
    Completed,
    /// The turn failed; a terminal event was written.
    Failed,
    /// The turn was interrupted because the caller cancelled it; nothing terminal was written.
    Interrupted,
}

/// What one notification became.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Translated {
    /// Events to append to the capture, in order.
    pub(super) lines: Vec<CaptureEvent>,
    /// Set when the notification closed the turn.
    pub(super) finish: Option<Finish>,
}

/// The state one turn's translation carries between notifications.
#[derive(Debug, Default)]
pub(super) struct Translator {
    thread_id: Option<ThreadId>,
    turn_id: Option<TurnId>,
    announced: bool,
    usage: Option<wire::TokenUsageBreakdown>,
    last_message: Option<String>,
    last_plan: Option<String>,
    last_error: Option<String>,
    /// How many items agentmux has written of its own, which numbers the next one.
    written: u32,
}

/// An instruction the server accepted into the running turn.
pub(super) fn steer(text: &str) -> CaptureEvent {
    CaptureEvent::Steer {
        text: text.to_owned(),
    }
}

/// A message codex sent that the generated types could not read, with what it said.
pub(super) fn undecoded(method: &str, reason: &str, payload: &Value) -> CaptureEvent {
    CaptureEvent::Undecoded(Undecoded::Message {
        method: method.to_owned(),
        reason: reason.to_owned(),
        payload: payload.clone(),
    })
}

/// What a line from the server that is no JSON-RPC message becomes, so the capture keeps it.
pub(super) fn unreadable(line: &str, reason: &str) -> CaptureEvent {
    CaptureEvent::Undecoded(Undecoded::Line {
        reason: reason.to_owned(),
        line: line.to_owned(),
    })
}

/// The terminal event for a turn the server ended badly.
fn failed(message: String) -> CaptureEvent {
    CaptureEvent::from(Exec::TurnFailed(exec::TurnFailedEvent {
        error: Some(exec::ThreadErrorEvent {
            message: Some(message),
        }),
    }))
}

/// One `exec` item, as `item.completed` carries it.
fn completed_item(id: Option<String>, details: Details) -> CaptureEvent {
    CaptureEvent::from(Exec::ItemCompleted(exec::ItemCompletedEvent {
        item: exec::ThreadItem { id, details },
    }))
}

/// A summary and its details, as `exec` joins them.
fn with_details(summary: String, details: Option<String>) -> String {
    match details.filter(|details| !details.is_empty()) {
        Some(details) => format!("{summary} ({details})"),
        None => summary,
    }
}

/// A value the generated types read but the pinned schema does not name, which a newer codex may
/// send.
///
/// `exec`'s vocabulary has no word for it, so the message that carried it is written as drift with
/// its whole payload, as one that does not decode is.
#[derive(Debug)]
struct Unrecognised {
    /// What the value was meant to be, such as "item" or "command status".
    what: &'static str,
    /// A string value as codex sent it; an object is not repeated, since the payload holds it.
    value: Option<String>,
}

impl Unrecognised {
    /// A string value, such as a status.
    fn value(what: &'static str, value: String) -> Self {
        Self {
            what,
            value: Some(value),
        }
    }

    /// An object, such as an item, whose tag the pinned schema does not name.
    fn object(what: &'static str) -> Self {
        Self { what, value: None }
    }
}

impl std::fmt::Display for Unrecognised {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.value {
            Some(value) => write!(f, "the pinned schema names no {} {value:?}", self.what),
            None => write!(f, "the pinned schema names no such {}", self.what),
        }
    }
}

/// The capture of a turn that ended in a way agentmux could not read.
fn ended_unreadably(out: &mut Translated, reason: &str, params: &Value) {
    // The turn is over either way; how it ended is the part that could not be read.
    out.lines
        .push(undecoded(m::TurnCompleted::METHOD, reason, params));
    out.lines.push(failed(format!(
        "codex ended the turn, but agentmux could not read how: {reason}"
    )));
    out.finish = Some(Finish::Failed);
}

impl Translator {
    /// An id for an item agentmux writes of its own, which codex never names.
    fn own_id(&mut self) -> String {
        self.written += 1;
        format!("agentmux_{}", self.written)
    }

    /// A reply, as `exec` writes the delegate's own message.
    pub(super) fn reply(&mut self, text: &str) -> CaptureEvent {
        let id = self.own_id();
        completed_item(
            Some(id),
            Details::AgentMessage(exec::AgentMessageItem {
                text: Some(text.to_owned()),
            }),
        )
    }

    /// An advisory note, in the shape `exec` uses for a warning that does not end the turn.
    pub(super) fn note(&mut self, text: &str) -> CaptureEvent {
        let id = self.own_id();
        completed_item(
            Some(id),
            Details::Error(exec::ErrorItem {
                message: Some(text.to_owned()),
            }),
        )
    }

    /// The turn this translation is for, once the server has named it.
    pub(super) fn turn_id(&self) -> Option<&TurnId> {
        self.turn_id.as_ref()
    }

    /// Learn the thread the consultation runs in, and return `exec`'s announcement of it.
    pub(super) fn thread_began(&mut self, id: &ThreadId) -> CaptureEvent {
        self.thread_id = Some(id.clone());
        CaptureEvent::from(Exec::ThreadStarted(exec::ThreadStartedEvent {
            thread_id: Some(id.as_str().to_owned()),
        }))
    }

    /// Whether a notification is about this consultation's own thread and turn, as `exec`
    /// decides it (`should_process_notification`).
    ///
    /// A delegate that starts agents of its own has their threads' notifications sent to this
    /// client too, and their replies, errors and usage are not the consultation's.
    /// A notification that names no thread or turn is kept, so one that has lost a field it
    /// should carry is still recorded as drift, and so is everything said before the ids are
    /// known.
    fn concerns_this_turn(&self, params: &Value) -> bool {
        let named = |pointer| params.pointer(pointer).and_then(Value::as_str);
        let matches = |named: Option<&str>, ours: Option<&str>| match (named, ours) {
            (Some(named), Some(ours)) => named == ours,
            _ => true,
        };
        matches(
            named("/threadId"),
            self.thread_id.as_ref().map(ThreadId::as_str),
        ) && matches(
            named("/turnId").or_else(|| named("/turn/id")),
            self.turn_id.as_ref().map(TurnId::as_str),
        )
    }

    /// Learn the turn's id, from `turn/started` or from the answer to `turn/start`, whichever
    /// arrives first.
    ///
    /// Returns `exec`'s `turn.started` the first time, for the capture.
    /// The store reads that event as the driver following the turn, which is when it delivers a
    /// steer, so it is written as soon as either source names the turn and never twice.
    pub(super) fn turn_began(&mut self, id: &TurnId) -> Option<CaptureEvent> {
        if self.turn_id.is_none() {
            self.turn_id = Some(id.clone());
        }
        (!std::mem::replace(&mut self.announced, true)).then_some(CaptureEvent::from(
            Exec::TurnStarted(exec::TurnStartedEvent {}),
        ))
    }

    /// The turn's closing message, for the closing-message file: its last reply, or, for a turn
    /// that answered only with a plan, that plan.
    pub(super) fn last_message(&self) -> Option<&str> {
        self.last_message.as_deref().or(self.last_plan.as_deref())
    }

    /// The closing message when it is a plan, which `exec` leaves out of the capture.
    pub(super) fn uncaptured_last_message(&self) -> Option<&str> {
        self.last_message
            .is_none()
            .then_some(self.last_plan.as_deref())
            .flatten()
    }

    /// Translate one notification.
    ///
    /// `cancelling` says whether the caller asked for the turn to stop, which is what makes an
    /// interrupted turn a cancellation rather than a failure.
    pub(super) fn notification(
        &mut self,
        method: &str,
        params: &Value,
        cancelling: bool,
    ) -> Translated {
        let mut out = Translated::default();
        let notification = Any::classify(method, params);
        // Configuration warnings concern the whole server, and `exec` reports them whoever they
        // name.
        let global = matches!(
            notification,
            Some(Any::ConfigWarning(_) | Any::DeprecationNotice(_))
        );
        if !global && !self.concerns_this_turn(params) {
            return out;
        }
        let Some(notification) = notification else {
            out.lines.push(undecoded(
                method,
                "the codex this driver was generated from sends no such notification",
                params,
            ));
            return out;
        };
        let result = match notification {
            Any::TurnStarted(started) => started.decode().map(|started| {
                out.lines
                    .extend(self.turn_began(&TurnId::new(started.turn.id)));
            }),
            Any::ItemCompleted(completed) => {
                self.item_completed(completed, &mut out);
                Ok(())
            }
            Any::Error(error) => error.decode().map(|error| {
                let message = with_details(error.error.message, error.error.additional_details);
                out.lines
                    .push(CaptureEvent::from(Exec::Error(exec::ThreadErrorEvent {
                        message: Some(message.clone()),
                    })));
                self.last_error = Some(message);
            }),
            Any::Warning(warning) => warning
                .decode()
                .map(|warning| out.lines.push(self.note(&warning.message))),
            Any::ConfigWarning(warning) => warning.decode().map(|warning| {
                let note = self.note(&with_details(warning.summary, warning.details));
                out.lines.push(note);
            }),
            Any::DeprecationNotice(notice) => notice.decode().map(|notice| {
                let note = self.note(&with_details(notice.summary, notice.details));
                out.lines.push(note);
            }),
            Any::ModelRerouted(reroute) => reroute.decode().map(|reroute| {
                out.lines.push(self.note(&format!(
                    "model rerouted: {} -> {} ({})",
                    reroute.from_model,
                    reroute.to_model,
                    reroute.reason.as_str()
                )));
            }),
            Any::ThreadTokenUsageUpdated(update) => update
                .decode()
                .map(|update| self.usage = Some(update.token_usage.total)),
            Any::TurnCompleted(completed) => return self.turn_completed(completed, cancelling),
            // Everything else is either a delta of what `item/completed` delivers whole, or
            // something `codex exec --json` does not report; it is not decoded, so a payload
            // codex reshaped there is no drift worth recording.
            _ => Ok(()),
        };
        if let Err(reason) = result {
            out.lines.push(undecoded(
                notification.method(),
                &reason.to_string(),
                params,
            ));
        }
        out
    }

    /// Translate `item/completed`, keeping a reply even when the item does not decode.
    fn item_completed(&mut self, completed: Payload<'_, m::ItemCompleted>, out: &mut Translated) {
        let params = completed.raw();
        match completed.decode() {
            Ok(completed) => match self.item(completed.item) {
                Ok(line) => out.lines.extend(line),
                Err(unrecognised) => out.lines.push(undecoded(
                    m::ItemCompleted::METHOD,
                    &unrecognised.to_string(),
                    params,
                )),
            },
            Err(reason) => {
                // Said as drift first, with the whole payload; then a reply still readable is
                // written as one, so the transcript keeps the report.
                let reason = reason.to_string();
                out.lines
                    .push(undecoded(m::ItemCompleted::METHOD, &reason, params));
                let raw = params.get("item").unwrap_or(&Value::Null);
                if raw.get("type").and_then(Value::as_str) == Some("agentMessage")
                    && let Some(text) = raw.get("text").and_then(Value::as_str)
                {
                    if !text.trim().is_empty() {
                        self.last_message = Some(text.to_owned());
                    }
                    let id = raw.get("id").and_then(Value::as_str).map(ToOwned::to_owned);
                    out.lines.push(completed_item(
                        id,
                        Details::AgentMessage(exec::AgentMessageItem {
                            text: Some(text.to_owned()),
                        }),
                    ));
                }
            }
        }
    }

    /// Translate `turn/completed`, which ends the turn whether or not it decodes.
    fn turn_completed(
        &mut self,
        completed: Payload<'_, m::TurnCompleted>,
        cancelling: bool,
    ) -> Translated {
        let mut out = Translated::default();
        let params = completed.raw();
        let completed = match completed.decode() {
            Ok(completed) => completed,
            Err(reason) => {
                ended_unreadably(&mut out, &reason.to_string(), params);
                return out;
            }
        };
        let turn = completed.turn;
        let reason = turn
            .error
            .map(|error| with_details(error.message, error.additional_details))
            .or_else(|| self.last_error.take());
        match turn.status {
            wire::TurnStatus::Completed => {
                // The turn's own record of its items wins, as it does for `exec`'s closing message.
                self.close_from_items(&turn.items);
                // Zero when the server reported none, as `exec` writes it.
                let usage = self.usage.clone().unwrap_or_default();
                out.lines.push(CaptureEvent::from(Exec::TurnCompleted(
                    exec::TurnCompletedEvent {
                        usage: Some(exec::Usage {
                            input_tokens: Some(usage.input_tokens),
                            cached_input_tokens: Some(usage.cached_input_tokens),
                            cache_write_input_tokens: Some(
                                usage.cache_write_input_tokens.unwrap_or(0),
                            ),
                            output_tokens: Some(usage.output_tokens),
                            reasoning_output_tokens: Some(usage.reasoning_output_tokens),
                        }),
                    },
                )));
                out.finish = Some(Finish::Completed);
            }
            wire::TurnStatus::Interrupted if cancelling => out.finish = Some(Finish::Interrupted),
            wire::TurnStatus::Interrupted => {
                out.lines.push(failed(reason.unwrap_or_else(|| {
                    "the turn was interrupted before it finished".to_owned()
                })));
                out.finish = Some(Finish::Failed);
            }
            wire::TurnStatus::Failed => {
                out.lines.push(failed(
                    // `exec`'s words, for a failure nothing named.
                    reason.unwrap_or_else(|| "turn failed".to_owned()),
                ));
                out.finish = Some(Finish::Failed);
            }
            // Still running, as far as the server says: the turn has not visibly ended.
            wire::TurnStatus::InProgress => {}
            wire::TurnStatus::Unknown(status) => {
                let reason = Unrecognised::value("turn status", status).to_string();
                ended_unreadably(&mut out, &reason, params);
            }
        }
        out
    }

    /// Take the closing message from a completed turn's items: its last reply, or its last plan.
    fn close_from_items(&mut self, items: &[wire::ThreadItem]) {
        let text = |wanted: fn(&wire::ThreadItem) -> Option<&str>| {
            items.iter().rev().find_map(wanted).map(ToOwned::to_owned)
        };
        if let Some(reply) = text(|item| match item {
            wire::ThreadItem::AgentMessage(message) => Some(&message.text),
            _ => None,
        }) {
            self.last_message = Some(reply);
        } else if let Some(plan) = text(|item| match item {
            wire::ThreadItem::Plan(plan) => Some(&plan.text),
            _ => None,
        }) {
            self.last_plan = Some(plan);
        }
    }

    /// The `exec` item for one completed item, or `None` for a kind `exec` does not report.
    ///
    /// Every kind is named, so a kind the schema gains is a compile error here once the types are
    /// regenerated, rather than a silent default; each mapping follows `exec`'s own.
    ///
    /// # Errors
    ///
    /// A kind, status or action the pinned schema does not name is [`Unrecognised`].
    fn item(&mut self, item: wire::ThreadItem) -> Result<Option<CaptureEvent>, Unrecognised> {
        use wire::ThreadItem as Item;
        let (id, details) = match item {
            Item::AgentMessage(message) => {
                if !message.text.trim().is_empty() {
                    self.last_message = Some(message.text.clone());
                }
                (
                    message.id,
                    Details::AgentMessage(exec::AgentMessageItem {
                        text: Some(message.text),
                    }),
                )
            }
            Item::Reasoning(reasoning) => {
                let text = reasoning.summary.unwrap_or_default().join("\n");
                if text.trim().is_empty() {
                    return Ok(None);
                }
                (
                    reasoning.id,
                    Details::Reasoning(exec::ReasoningItem { text }),
                )
            }
            Item::CommandExecution(command) => (command.id.clone(), command_execution(*command)?),
            Item::FileChange(change) => (change.id.clone(), file_change(*change)?),
            Item::McpToolCall(call) => (call.id.clone(), mcp_tool_call(*call)?),
            Item::CollabAgentToolCall(call) => {
                let id = call.id.clone();
                let Some(details) = collab_tool_call(*call)? else {
                    return Ok(None);
                };
                (id, details)
            }
            Item::WebSearch(search) => (
                search.id,
                Details::WebSearch(exec::WebSearchItem {
                    query: search.query,
                    action: web_search_action(search.action)?,
                    results: search.results,
                }),
            ),
            Item::Plan(plan) => {
                self.last_plan = Some(plan.text);
                return Ok(None);
            }
            Item::UserMessage(_)
            | Item::ContextCompaction(_)
            | Item::HookPrompt(_)
            | Item::FunctionCallOutput(_)
            | Item::DynamicToolCall(_)
            | Item::SubAgentActivity(_)
            | Item::ImageView(_)
            | Item::Sleep(_)
            | Item::ImageGeneration(_)
            | Item::EnteredReviewMode(_)
            | Item::ExitedReviewMode(_) => return Ok(None),
            Item::Unknown(_) => return Err(Unrecognised::object("item")),
        };
        Ok(Some(completed_item(Some(id), details)))
    }
}

/// A command, as `exec`'s `command_execution`.
fn command_execution(command: wire::CommandExecutionThreadItem) -> Result<Details, Unrecognised> {
    Ok(Details::CommandExecution(exec::CommandExecutionItem {
        command: command.command,
        aggregated_output: command.aggregated_output.unwrap_or_default(),
        exit_code: command.exit_code,
        status: match command.status {
            wire::CommandExecutionStatus::InProgress => exec::CommandExecutionStatus::InProgress,
            wire::CommandExecutionStatus::Completed => exec::CommandExecutionStatus::Completed,
            wire::CommandExecutionStatus::Failed => exec::CommandExecutionStatus::Failed,
            wire::CommandExecutionStatus::Declined => exec::CommandExecutionStatus::Declined,
            wire::CommandExecutionStatus::Unknown(status) => {
                return Err(Unrecognised::value("command status", status));
            }
        },
    }))
}

/// A patch, as `exec`'s `file_change`.
fn file_change(change: wire::FileChangeThreadItem) -> Result<Details, Unrecognised> {
    Ok(Details::FileChange(exec::FileChangeItem {
        changes: change
            .changes
            .into_iter()
            .map(|file| {
                let kind = match file.kind {
                    wire::PatchChangeKind::Add => exec::PatchChangeKind::Add,
                    wire::PatchChangeKind::Delete => exec::PatchChangeKind::Delete,
                    wire::PatchChangeKind::Update(_) => exec::PatchChangeKind::Update,
                    wire::PatchChangeKind::Unknown(_) => {
                        return Err(Unrecognised::object("file change kind"));
                    }
                };
                Ok(exec::FileUpdateChange {
                    path: file.path,
                    kind,
                })
            })
            .collect::<Result<_, _>>()?,
        // `exec` has no `declined`; a declined patch is one that failed to apply.
        status: match change.status {
            wire::PatchApplyStatus::InProgress => exec::PatchApplyStatus::InProgress,
            wire::PatchApplyStatus::Completed => exec::PatchApplyStatus::Completed,
            wire::PatchApplyStatus::Failed | wire::PatchApplyStatus::Declined => {
                exec::PatchApplyStatus::Failed
            }
            wire::PatchApplyStatus::Unknown(status) => {
                return Err(Unrecognised::value("file change status", status));
            }
        },
    }))
}

/// An MCP tool call, as `exec`'s `mcp_tool_call`.
fn mcp_tool_call(call: wire::McpToolCallThreadItem) -> Result<Details, Unrecognised> {
    Ok(Details::McpToolCall(exec::McpToolCallItem {
        server: call.server,
        tool: call.tool,
        arguments: call.arguments,
        result: call.result.map(|result| exec::McpToolCallItemResult {
            content: result.content,
            meta: result.meta,
            structured_content: result.structured_content,
        }),
        error: call.error.map(|error| exec::McpToolCallItemError {
            message: error.message,
        }),
        status: match call.status {
            wire::McpToolCallStatus::InProgress => exec::McpToolCallStatus::InProgress,
            wire::McpToolCallStatus::Completed => exec::McpToolCallStatus::Completed,
            wire::McpToolCallStatus::Failed => exec::McpToolCallStatus::Failed,
            wire::McpToolCallStatus::Unknown(status) => {
                return Err(Unrecognised::value("MCP tool call status", status));
            }
        },
    }))
}

/// What a web search did, as `exec` names it; one that says nothing is `Other`, as in `exec`.
fn web_search_action(
    action: Option<wire::WebSearchAction>,
) -> Result<exec::WebSearchAction, Unrecognised> {
    Ok(match action {
        Some(wire::WebSearchAction::Search(action)) => {
            let wire::SearchWebSearchAction { query, queries } = *action;
            exec::WebSearchAction::Search { query, queries }
        }
        Some(wire::WebSearchAction::OpenPage(action)) => {
            exec::WebSearchAction::OpenPage { url: action.url }
        }
        Some(wire::WebSearchAction::FindInPage(action)) => {
            let wire::FindInPageWebSearchAction { url, pattern } = *action;
            exec::WebSearchAction::FindInPage { url, pattern }
        }
        Some(wire::WebSearchAction::Other) | None => exec::WebSearchAction::Other,
        Some(wire::WebSearchAction::Unknown(_)) => {
            return Err(Unrecognised::object("web search action"));
        }
    })
}

/// A sub-agent tool call, as `exec`'s `collab_tool_call`, or `None` for one `exec` leaves out.
///
/// `exec` names four tools and folds `resumeAgent` into `wait`; it reports neither the other tools
/// nor an interrupted call.
fn collab_tool_call(
    call: wire::CollabAgentToolCallThreadItem,
) -> Result<Option<Details>, Unrecognised> {
    use wire::CollabAgentTool as Tool;
    let tool = match call.tool {
        Tool::SpawnAgent => exec::CollabTool::SpawnAgent,
        Tool::SendInput => exec::CollabTool::SendInput,
        Tool::ResumeAgent | Tool::Wait => exec::CollabTool::Wait,
        Tool::CloseAgent => exec::CollabTool::CloseAgent,
        Tool::SendMessage | Tool::FollowupTask | Tool::InterruptAgent | Tool::ListAgents => {
            return Ok(None);
        }
        Tool::Unknown(tool) => return Err(Unrecognised::value("sub-agent tool", tool)),
    };
    let status = match call.status {
        wire::CollabAgentToolCallStatus::InProgress => exec::CollabToolCallStatus::InProgress,
        wire::CollabAgentToolCallStatus::Completed => exec::CollabToolCallStatus::Completed,
        wire::CollabAgentToolCallStatus::Failed => exec::CollabToolCallStatus::Failed,
        wire::CollabAgentToolCallStatus::Interrupted => return Ok(None),
        wire::CollabAgentToolCallStatus::Unknown(status) => {
            return Err(Unrecognised::value("sub-agent call status", status));
        }
    };
    let agents_states = call
        .agents_states
        .into_iter()
        .map(|(thread, state)| {
            let status = match state.status {
                wire::CollabAgentStatus::PendingInit => exec::CollabAgentStatus::PendingInit,
                wire::CollabAgentStatus::Running => exec::CollabAgentStatus::Running,
                wire::CollabAgentStatus::Interrupted => exec::CollabAgentStatus::Interrupted,
                wire::CollabAgentStatus::Completed => exec::CollabAgentStatus::Completed,
                wire::CollabAgentStatus::Errored => exec::CollabAgentStatus::Errored,
                wire::CollabAgentStatus::Shutdown => exec::CollabAgentStatus::Shutdown,
                wire::CollabAgentStatus::NotFound => exec::CollabAgentStatus::NotFound,
                wire::CollabAgentStatus::Unknown(status) => {
                    return Err(Unrecognised::value("sub-agent status", status));
                }
            };
            let message = state.message;
            Ok((thread, exec::CollabAgentState { status, message }))
        })
        .collect::<Result<_, _>>()?;
    Ok(Some(Details::CollabToolCall(exec::CollabToolCallItem {
        tool,
        sender_thread_id: call.sender_thread_id,
        receiver_thread_ids: call.receiver_thread_ids,
        prompt: call.prompt,
        agents_states,
        status,
    })))
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use serde_json::{Value, json};

    use super::{Finish, Translator, TurnId};
    use crate::stream::codex::CaptureEvent;

    fn written(events: &[CaptureEvent]) -> Vec<Value> {
        events
            .iter()
            .filter_map(|event| serde_json::to_value(event).ok())
            .collect()
    }

    fn lines(translator: &mut Translator, method: &str, params: &Value) -> Vec<Value> {
        written(&translator.notification(method, params, false).lines)
    }

    fn turn(id: &str, status: &str) -> Value {
        json!({"threadId": "th", "turn": {"id": id, "status": status, "items": []}})
    }

    fn completed(item: &Value) -> Value {
        json!({"threadId": "th", "turnId": "tu", "completedAtMs": 1, "item": item})
    }

    /// A reply arrives as `exec`'s `agent_message` and becomes the closing message.
    #[gtest]
    fn an_agent_message_becomes_an_exec_agent_message() {
        let mut t = Translator::default();
        let params = completed(&json!({"id": "i1", "type": "agentMessage", "text": "FINDINGS"}));

        assert_that!(
            lines(&mut t, "item/completed", &params),
            elements_are![eq(&json!({"type": "item.completed",
                "item": {"id": "i1", "type": "agent_message", "text": "FINDINGS"}}))]
        );
        assert_that!(t.last_message(), some(eq("FINDINGS")));
    }

    /// A sub-agent tool call is written as the `collab_tool_call` `codex exec --json` writes,
    /// with `exec`'s tool name and status spellings.
    #[gtest]
    fn a_sub_agent_tool_call_becomes_execs_collab_tool_call() {
        let mut t = Translator::default();
        let params = completed(&json!({
            "id": "i7", "type": "collabAgentToolCall", "tool": "resumeAgent",
            "senderThreadId": "parent", "receiverThreadIds": ["child"], "prompt": null,
            "agentsStates": {"child": {"status": "pendingInit", "message": null}},
            "status": "inProgress",
        }));

        assert_that!(
            lines(&mut t, "item/completed", &params),
            elements_are![eq(&json!({"type": "item.completed", "item": {
                "id": "i7", "type": "collab_tool_call", "tool": "wait",
                "sender_thread_id": "parent", "receiver_thread_ids": ["child"], "prompt": null,
                "agents_states": {"child": {"status": "pending_init", "message": null}},
                "status": "in_progress",
            }}))]
        );

        // The tools and the state `exec` leaves out produce nothing.
        let listed = completed(&json!({
            "id": "i8", "type": "collabAgentToolCall", "tool": "listAgents",
            "senderThreadId": "parent", "receiverThreadIds": [], "prompt": null,
            "agentsStates": {}, "status": "completed",
        }));
        assert_that!(lines(&mut t, "item/completed", &listed), is_empty());
    }

    /// A completed turn reports the running token total under `exec`'s field names.
    #[gtest]
    fn a_completed_turn_carries_the_token_total() {
        let mut t = Translator::default();
        t.turn_began(&TurnId::new("tu"));
        let usage = lines(
            &mut t,
            "thread/tokenUsage/updated",
            &json!({"threadId": "th", "turnId": "tu", "tokenUsage": {
                "last": {"inputTokens": 1, "cachedInputTokens": 0, "outputTokens": 1,
                         "reasoningOutputTokens": 0, "totalTokens": 2},
                "total": {"inputTokens": 120, "cachedInputTokens": 40, "outputTokens": 7,
                          "reasoningOutputTokens": 3, "totalTokens": 127}}}),
        );
        assert_that!(usage, is_empty());
        let ended = t.notification("turn/completed", &turn("tu", "completed"), false);

        assert_that!(ended.finish, some(eq(Finish::Completed)));
        let event = written(&ended.lines).into_iter().next().unwrap_or_default();
        assert_that!(event["type"], eq(&json!("turn.completed")));
        assert_that!(event["usage"]["input_tokens"], eq(&json!(120)));
        assert_that!(event["usage"]["output_tokens"], eq(&json!(7)));
        assert_that!(event["usage"]["reasoning_output_tokens"], eq(&json!(3)));
    }

    /// A turn that answered only with a plan closes with the plan, as `exec` closes it.
    #[gtest]
    fn a_turn_that_answered_with_a_plan_closes_with_it() {
        let mut t = Translator::default();
        let plan = completed(&json!({"id": "p1", "type": "plan", "text": "1. read 2. fix"}));
        assert_that!(lines(&mut t, "item/completed", &plan), is_empty());
        assert_that!(t.last_message(), some(eq("1. read 2. fix")));

        // A reply, where there is one, is the closing message instead.
        let reply = completed(&json!({"id": "i1", "type": "agentMessage", "text": "DONE"}));
        lines(&mut t, "item/completed", &reply);
        assert_that!(t.last_message(), some(eq("DONE")));
    }

    /// Every error is said where it arrived, as `exec` says it, whether or not the server will
    /// retry; the last one is the failure of a failed turn that names none of its own.
    #[gtest]
    fn an_error_is_said_where_it_arrived_and_names_a_failure_without_one() {
        let mut t = Translator::default();
        for (will_retry, message) in [(true, "reconnecting"), (false, "model not supported")] {
            let out = t.notification(
                "error",
                &json!({"threadId": "th", "turnId": "tu", "willRetry": will_retry,
                        "error": {"message": message}}),
                false,
            );
            assert_that!(out.finish, none());
            assert_that!(
                written(&out.lines),
                elements_are![eq(&json!({"type": "error", "message": message}))]
            );
        }

        let ended = t.notification("turn/completed", &turn("tu", "failed"), false);
        assert_that!(ended.finish, some(eq(Finish::Failed)));
        assert_that!(
            written(&ended.lines),
            elements_are![eq(&json!({"type": "turn.failed",
                "error": {"message": "model not supported"}}))]
        );
    }

    /// An interrupt the caller asked for writes nothing terminal; one nobody asked for is a
    /// failure.
    #[gtest]
    fn an_interrupt_is_a_cancellation_only_when_one_was_asked_for() {
        let mut cancelled = Translator::default();
        let out = cancelled.notification("turn/completed", &turn("tu", "interrupted"), true);
        assert_that!(out.finish, some(eq(Finish::Interrupted)));
        assert_that!(out.lines, is_empty());

        let mut unasked = Translator::default();
        let out = unasked.notification("turn/completed", &turn("tu", "interrupted"), false);
        assert_that!(out.finish, some(eq(Finish::Failed)));
        assert_that!(
            written(&out.lines)
                .iter()
                .map(|event| event["type"].clone())
                .collect::<Vec<_>>(),
            elements_are![eq(&json!("turn.failed"))]
        );
    }

    /// The turn is announced once, by whichever names it first: the answer to `turn/start` or the
    /// `turn/started` notification.
    #[gtest]
    fn the_turn_is_announced_once_whichever_source_names_it_first() {
        let started = json!({"type": "turn.started"});

        let mut t = Translator::default();
        assert_that!(
            written(
                &t.turn_began(&TurnId::new("tu"))
                    .into_iter()
                    .collect::<Vec<_>>()
            ),
            elements_are![eq(&started)]
        );
        assert_that!(
            lines(&mut t, "turn/started", &turn("tu", "inProgress")),
            is_empty()
        );

        let mut t = Translator::default();
        assert_that!(
            lines(&mut t, "turn/started", &turn("tu", "inProgress")),
            elements_are![eq(&started)]
        );
        assert_that!(t.turn_began(&TurnId::new("tu")), none());
        assert_that!(t.turn_id().map(TurnId::as_str), some(eq("tu")));
    }

    /// Another turn's completion does not end this one.
    #[gtest]
    fn another_turns_completion_is_ignored() {
        let mut t = Translator::default();
        t.turn_began(&TurnId::new("tu-1"));
        let out = t.notification("turn/completed", &turn("tu-0", "completed"), false);
        assert_that!(out, eq(&super::Translated::default()));
    }

    /// Kinds and notifications `exec` does not report produce nothing; a method the pinned
    /// schema never had is drift, with its payload.
    #[gtest]
    fn what_exec_leaves_out_is_left_out_and_an_unknown_method_is_drift() {
        let mut t = Translator::default();
        let hook = completed(&json!({"id": "i9", "type": "hookPrompt", "fragments": []}));
        assert_that!(lines(&mut t, "item/completed", &hook), is_empty());
        let diff = json!({"threadId": "th", "turnId": "tu", "diff": ""});
        assert_that!(lines(&mut t, "turn/diff/updated", &diff), is_empty());

        let novel = json!({"threadId": "th", "said": "something new"});
        let drift = lines(&mut t, "thread/somethingNew", &novel);
        assert_that!(drift.len(), eq(1));
        assert_that!(drift[0]["type"], eq(&json!("agentmux.undecoded")));
        assert_that!(drift[0]["payload"], eq(&novel));
    }

    /// A value a newer codex sends that the pinned schema does not name is drift with its whole
    /// payload, as a payload that does not decode is, and a turn that ended on one still ends.
    #[gtest]
    fn a_value_the_pinned_schema_does_not_name_is_drift() {
        let mut t = Translator::default();
        let command = completed(&json!({
            "id": "i3", "type": "commandExecution", "command": "ls", "cwd": "/work",
            "commandActions": [], "status": "paused",
        }));
        let novel_item = completed(&json!({"id": "i4", "type": "hologram", "beams": 3}));

        // Neither item is written as an `exec` item; each is drift that keeps what codex said.
        for params in [&command, &novel_item] {
            let drift = lines(&mut t, "item/completed", params);
            assert_that!(drift.len(), eq(1));
            assert_that!(drift[0]["type"], eq(&json!("agentmux.undecoded")));
            assert_that!(drift[0]["payload"], eq(params));
        }

        // A turn that ended with a status nobody named is failed, and says why.
        let out = t.notification("turn/completed", &turn("tu", "suspended"), false);
        assert_that!(out.finish, some(eq(Finish::Failed)));
        assert_that!(
            written(&out.lines)
                .iter()
                .map(|event| event["type"].clone())
                .collect::<Vec<_>>(),
            elements_are![eq(&json!("agentmux.undecoded")), eq(&json!("turn.failed"))]
        );
    }

    /// A notification that no longer matches the generated types is drift that keeps its payload,
    /// and a reply inside it still survives.
    #[gtest]
    fn an_undecodable_notification_is_drift_and_keeps_its_reply() {
        let mut t = Translator::default();
        // `completedAtMs` is required by the schema the types were generated from.
        let params = json!({"threadId": "th", "turnId": "tu",
            "item": {"id": "i1", "type": "agentMessage", "text": "STILL HERE"}});
        let out = lines(&mut t, "item/completed", &params);
        assert_that!(
            out.iter()
                .filter_map(|event| event["type"].as_str().map(str::to_owned))
                .collect::<Vec<_>>(),
            elements_are![eq("agentmux.undecoded"), eq("item.completed")]
        );
        assert_that!(out[0]["payload"], eq(&params));
        assert_that!(out[1]["item"]["text"], eq(&json!("STILL HERE")));
        assert_that!(t.last_message(), some(eq("STILL HERE")));

        // A turn whose end cannot be read still ends.
        let ended = t.notification("turn/completed", &json!({"threadId": "th"}), false);
        assert_that!(ended.finish, some(eq(Finish::Failed)));
    }
}
