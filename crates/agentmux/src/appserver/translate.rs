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
//! A notification that does not decode, or whose method the pinned schema never had, is written
//! as [`UNDECODED_EVENT`] with its payload, which the fold counts as drift: a codex whose protocol
//! moved away from the generated types is loud rather than lossy, the raw capture still holds what
//! it said, a reply is still kept, and a turn that ended is still ended.

use serde_json::{Value, json};

use super::protocol::{decode, wire};
use super::schema::SERVER_NOTIFICATIONS;
use crate::stream::codex::STEER_EVENT;

/// The event written in place of a notification that does not match the generated types.
pub const UNDECODED_EVENT: &str = "agentmux.undecoded";

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
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Translated {
    /// Lines to append to the capture, in order.
    pub(super) lines: Vec<String>,
    /// Set when the notification closed the turn.
    pub(super) finish: Option<Finish>,
}

/// The state one turn's translation carries between notifications.
#[derive(Debug, Default)]
pub(super) struct Translator {
    thread_id: Option<String>,
    turn_id: Option<String>,
    announced: bool,
    usage: Option<wire::TokenUsageBreakdown>,
    last_message: Option<String>,
    last_plan: Option<String>,
    last_error: Option<String>,
}

/// An instruction the server accepted into the running turn.
pub(super) fn steer(text: &str) -> String {
    json!({"type": STEER_EVENT, "text": text}).to_string()
}

/// A reply, as `exec` writes the delegate's own message.
pub(super) fn reply(text: &str) -> String {
    json!({"type": "item.completed", "item": {"type": "agent_message", "text": text}}).to_string()
}

/// An advisory note, in the shape `exec` uses for a warning that does not end the turn.
pub(super) fn note(text: &str) -> String {
    json!({"type": "item.completed", "item": {"type": "error", "message": text}}).to_string()
}

/// A message codex sent that the generated types could not read, with what it said.
pub(super) fn undecoded(method: &str, reason: &str, payload: &Value) -> String {
    json!({"type": UNDECODED_EVENT, "method": method, "reason": reason, "payload": payload})
        .to_string()
}

/// The terminal event for a turn the server ended badly.
fn failed(message: &str) -> String {
    json!({"type": "turn.failed", "error": {"message": message}}).to_string()
}

/// One `exec` item, as `item.completed` carries it.
fn completed_item(item: &Value) -> String {
    json!({"type": "item.completed", "item": item}).to_string()
}

/// A summary and its details, as `exec` joins them.
fn with_details(summary: String, details: Option<String>) -> String {
    match details.filter(|details| !details.is_empty()) {
        Some(details) => format!("{summary} ({details})"),
        None => summary,
    }
}

/// A wire value such as `inProgress`, spelled as `exec` spells it (`in_progress`).
fn snake(wire: &str) -> String {
    let mut out = String::with_capacity(wire.len() + 4);
    for c in wire.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

impl Translator {
    /// The turn this translation is for, once the server has named it.
    pub(super) fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    /// Learn the thread the consultation runs in, and return `exec`'s announcement of it.
    pub(super) fn thread_began(&mut self, id: &str) -> String {
        self.thread_id = Some(id.to_owned());
        json!({"type": "thread.started", "thread_id": id}).to_string()
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
        matches(named("/threadId"), self.thread_id.as_deref())
            && matches(
                named("/turnId").or_else(|| named("/turn/id")),
                self.turn_id.as_deref(),
            )
    }

    /// Learn the turn's id, from `turn/started` or from the answer to `turn/start`, whichever
    /// arrives first.
    ///
    /// Returns `exec`'s `turn.started` the first time, for the capture.
    /// The store reads that event as the driver following the turn, which is when it delivers a
    /// steer, so it is written as soon as either source names the turn and never twice.
    pub(super) fn turn_began(&mut self, id: &str) -> Option<String> {
        if self.turn_id.is_none() {
            self.turn_id = Some(id.to_owned());
        }
        (!std::mem::replace(&mut self.announced, true))
            .then(|| json!({"type": "turn.started"}).to_string())
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
        // Configuration warnings concern the whole server, and `exec` reports them whoever they
        // name.
        let global = matches!(method, "configWarning" | "deprecationNotice");
        if !global && !self.concerns_this_turn(params) {
            return out;
        }
        let result = match method {
            "turn/started" => decode::<wire::TurnStartedNotification>(params).map(|started| {
                out.lines.extend(self.turn_began(&started.turn.id));
            }),
            "item/completed" => {
                self.item_completed(params, &mut out);
                Ok(())
            }
            "error" => decode::<wire::ErrorNotification>(params).map(|error| {
                let message = with_details(error.error.message, error.error.additional_details);
                out.lines
                    .push(json!({"type": "error", "message": message}).to_string());
                self.last_error = Some(message);
            }),
            "warning" => decode::<wire::WarningNotification>(params)
                .map(|warning| out.lines.push(note(&warning.message))),
            "configWarning" => decode::<wire::ConfigWarningNotification>(params).map(|warning| {
                out.lines
                    .push(note(&with_details(warning.summary, warning.details)));
            }),
            "deprecationNotice" => {
                decode::<wire::DeprecationNoticeNotification>(params).map(|notice| {
                    out.lines
                        .push(note(&with_details(notice.summary, notice.details)));
                })
            }
            "model/rerouted" => decode::<wire::ModelReroutedNotification>(params).map(|reroute| {
                out.lines.push(note(&format!(
                    "model rerouted: {} -> {} ({})",
                    reroute.from_model,
                    reroute.to_model,
                    reroute.reason.as_str()
                )));
            }),
            "thread/tokenUsage/updated" => {
                decode::<wire::ThreadTokenUsageUpdatedNotification>(params)
                    .map(|update| self.usage = Some(update.token_usage.total))
            }
            "turn/completed" => return self.turn_completed(params, cancelling),
            known if SERVER_NOTIFICATIONS.contains(&known) => Ok(()),
            _ => Err(
                "the codex this driver was generated from sends no such notification".to_owned(),
            ),
        };
        if let Err(reason) = result {
            out.lines.push(undecoded(method, &reason, params));
        }
        out
    }

    /// Translate `item/completed`, keeping a reply even when the item does not decode.
    fn item_completed(&mut self, params: &Value, out: &mut Translated) {
        match decode::<wire::ItemCompletedNotification>(params) {
            Ok(completed) => {
                if let Some(item) = self.item(completed.item) {
                    out.lines.push(completed_item(&item));
                }
            }
            Err(reason) => {
                // Said as drift first, with the whole payload; then a reply still readable is
                // written as one, so the transcript keeps the report.
                out.lines.push(undecoded("item/completed", &reason, params));
                let raw = params.get("item").unwrap_or(&Value::Null);
                if raw.get("type").and_then(Value::as_str) == Some("agentMessage")
                    && let Some(text) = raw.get("text").and_then(Value::as_str)
                {
                    if !text.trim().is_empty() {
                        self.last_message = Some(text.to_owned());
                    }
                    out.lines.push(completed_item(&json!({
                        "id": raw.get("id"),
                        "type": "agent_message",
                        "text": text,
                    })));
                }
            }
        }
    }

    /// Translate `turn/completed`, which ends the turn whether or not it decodes.
    fn turn_completed(&mut self, params: &Value, cancelling: bool) -> Translated {
        let mut out = Translated::default();
        let completed = match decode::<wire::TurnCompletedNotification>(params) {
            Ok(completed) => completed,
            Err(reason) => {
                // The turn is over either way; how it ended is the part that could not be read.
                out.lines.push(undecoded("turn/completed", &reason, params));
                out.lines.push(failed(&format!(
                    "codex ended the turn, but agentmux could not read how: {reason}"
                )));
                out.finish = Some(Finish::Failed);
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
                out.lines.push(
                    json!({"type": "turn.completed", "usage": {
                        "input_tokens": usage.input_tokens,
                        "cached_input_tokens": usage.cached_input_tokens,
                        "cache_write_input_tokens": usage.cache_write_input_tokens.unwrap_or(0),
                        "output_tokens": usage.output_tokens,
                        "reasoning_output_tokens": usage.reasoning_output_tokens,
                    }})
                    .to_string(),
                );
                out.finish = Some(Finish::Completed);
            }
            wire::TurnStatus::Interrupted if cancelling => out.finish = Some(Finish::Interrupted),
            wire::TurnStatus::Interrupted => {
                out.lines.push(failed(&reason.unwrap_or_else(|| {
                    "the turn was interrupted before it finished".to_owned()
                })));
                out.finish = Some(Finish::Failed);
            }
            wire::TurnStatus::Failed => {
                out.lines.push(failed(
                    // `exec`'s words, for a failure nothing named.
                    &reason.unwrap_or_else(|| "turn failed".to_owned()),
                ));
                out.finish = Some(Finish::Failed);
            }
            // Still running, as far as the server says: the turn has not visibly ended.
            wire::TurnStatus::InProgress => {}
        }
        out
    }

    /// Take the closing message from a completed turn's items: its last reply, or its last plan.
    fn close_from_items(&mut self, items: &[wire::ThreadItem]) {
        let text = |wanted: fn(&wire::ThreadItem) -> Option<&str>| {
            items.iter().rev().find_map(wanted).map(ToOwned::to_owned)
        };
        if let Some(reply) = text(|item| match item {
            wire::ThreadItem::AgentMessageThreadItem(message) => Some(&message.text),
            _ => None,
        }) {
            self.last_message = Some(reply);
        } else if let Some(plan) = text(|item| match item {
            wire::ThreadItem::PlanThreadItem(plan) => Some(&plan.text),
            _ => None,
        }) {
            self.last_plan = Some(plan);
        }
    }

    /// The `exec` item for one completed item, or `None` for a kind `exec` does not report.
    ///
    /// Every kind is named, so a kind the schema gains is a compile error here once the types are
    /// regenerated, rather than a silent default.
    fn item(&mut self, item: wire::ThreadItem) -> Option<Value> {
        use wire::ThreadItem as Item;
        Some(match item {
            Item::AgentMessageThreadItem(message) => {
                if !message.text.trim().is_empty() {
                    self.last_message = Some(message.text.clone());
                }
                json!({"id": message.id, "type": "agent_message", "text": message.text})
            }
            Item::ReasoningThreadItem(reasoning) => {
                let text = reasoning.summary.unwrap_or_default().join("\n");
                if text.trim().is_empty() {
                    return None;
                }
                json!({"id": reasoning.id, "type": "reasoning", "text": text})
            }
            Item::CommandExecutionThreadItem(command) => json!({
                "id": command.id,
                "type": "command_execution",
                "command": command.command,
                "aggregated_output": command.aggregated_output.unwrap_or_default(),
                "exit_code": command.exit_code,
                "status": snake(command.status.as_str()),
            }),
            Item::FileChangeThreadItem(change) => json!({
                "id": change.id,
                "type": "file_change",
                "changes": change
                    .changes
                    .iter()
                    .map(|file| json!({"path": file.path, "kind": match &file.kind {
                        wire::PatchChangeKind::AddPatchChangeKind(_) => "add",
                        wire::PatchChangeKind::DeletePatchChangeKind(_) => "delete",
                        wire::PatchChangeKind::UpdatePatchChangeKind(_) => "update",
                    }}))
                    .collect::<Vec<_>>(),
                // `exec` has no `declined`; a declined patch is one that failed to apply.
                "status": match change.status {
                    wire::PatchApplyStatus::Declined => "failed".to_owned(),
                    other => snake(other.as_str()),
                },
            }),
            Item::McpToolCallThreadItem(call) => json!({
                "id": call.id,
                "type": "mcp_tool_call",
                "server": call.server,
                "tool": call.tool,
                "arguments": call.arguments,
                "result": call.result.map(|result| json!({
                    "content": result.content,
                    "structured_content": result.structured_content,
                })),
                "error": call.error.map(|error| json!({"message": error.message})),
                "status": snake(call.status.as_str()),
            }),
            Item::CollabAgentToolCallThreadItem(call) => collab_tool_call(*call)?,
            Item::WebSearchThreadItem(search) => json!({
                "id": search.id,
                "type": "web_search",
                "query": search.query,
                "results": search.results,
            }),
            Item::PlanThreadItem(plan) => {
                self.last_plan = Some(plan.text);
                return None;
            }
            Item::UserMessageThreadItem(_)
            | Item::ContextCompactionThreadItem(_)
            | Item::HookPromptThreadItem(_)
            | Item::FunctionCallOutputThreadItem(_)
            | Item::DynamicToolCallThreadItem(_)
            | Item::SubAgentActivityThreadItem(_)
            | Item::ImageViewThreadItem(_)
            | Item::SleepThreadItem(_)
            | Item::ImageGenerationThreadItem(_)
            | Item::EnteredReviewModeThreadItem(_)
            | Item::ExitedReviewModeThreadItem(_) => return None,
        })
    }
}

/// A sub-agent tool call, as `exec`'s `collab_tool_call`, or `None` for one `exec` leaves out.
///
/// `exec` names four tools and folds `resumeAgent` into `wait`; it reports neither the other tools
/// nor an interrupted call.
fn collab_tool_call(call: wire::CollabAgentToolCallThreadItem) -> Option<Value> {
    use wire::CollabAgentTool as Tool;
    let tool = match call.tool {
        Tool::SpawnAgent => "spawn_agent",
        Tool::SendInput => "send_input",
        Tool::ResumeAgent | Tool::Wait => "wait",
        Tool::CloseAgent => "close_agent",
        Tool::SendMessage | Tool::FollowupTask | Tool::InterruptAgent | Tool::ListAgents => {
            return None;
        }
    };
    if call.status == wire::CollabAgentToolCallStatus::Interrupted {
        return None;
    }
    let states: serde_json::Map<String, Value> = call
        .agents_states
        .into_iter()
        .map(|(thread, state)| {
            let status = snake(state.status.as_str());
            (thread, json!({"status": status, "message": state.message}))
        })
        .collect();
    Some(json!({
        "id": call.id,
        "type": "collab_tool_call",
        "tool": tool,
        "sender_thread_id": call.sender_thread_id,
        "receiver_thread_ids": call.receiver_thread_ids,
        "prompt": call.prompt,
        "agents_states": states,
        "status": snake(call.status.as_str()),
    }))
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use serde_json::{Value, json};

    use super::{Finish, Translator, UNDECODED_EVENT};

    fn lines(translator: &mut Translator, method: &str, params: &Value) -> Vec<Value> {
        translator
            .notification(method, params, false)
            .lines
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
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
        t.turn_began("tu");
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
        let event: Value = serde_json::from_str(ended.lines.first().map_or("", String::as_str))
            .unwrap_or_default();
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
            let written: Vec<Value> = out
                .lines
                .iter()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            assert_that!(
                written,
                elements_are![eq(&json!({"type": "error", "message": message}))]
            );
        }

        let ended = t.notification("turn/completed", &turn("tu", "failed"), false);
        assert_that!(ended.finish, some(eq(Finish::Failed)));
        assert_that!(
            ended.lines,
            elements_are![eq(&json!({"type": "turn.failed",
                "error": {"message": "model not supported"}})
            .to_string())]
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
        assert_that!(out.lines, elements_are![contains_substring("turn.failed")]);
    }

    /// The turn is announced once, by whichever names it first: the answer to `turn/start` or the
    /// `turn/started` notification.
    #[gtest]
    fn the_turn_is_announced_once_whichever_source_names_it_first() {
        let started = json!({"type": "turn.started"});

        let mut t = Translator::default();
        assert_that!(t.turn_began("tu"), some(eq(&started.to_string())));
        assert_that!(
            lines(&mut t, "turn/started", &turn("tu", "inProgress")),
            is_empty()
        );

        let mut t = Translator::default();
        assert_that!(
            lines(&mut t, "turn/started", &turn("tu", "inProgress")),
            elements_are![eq(&started)]
        );
        assert_that!(t.turn_began("tu"), none());
        assert_that!(t.turn_id(), some(eq("tu")));
    }

    /// Another turn's completion does not end this one.
    #[gtest]
    fn another_turns_completion_is_ignored() {
        let mut t = Translator::default();
        t.turn_began("tu-1");
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
        assert_that!(drift[0]["type"], eq(&json!(UNDECODED_EVENT)));
        assert_that!(drift[0]["payload"], eq(&novel));
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
                .filter_map(|e| e["type"].as_str().map(str::to_owned))
                .collect::<Vec<_>>(),
            elements_are![eq(UNDECODED_EVENT), eq("item.completed")]
        );
        assert_that!(out[0]["payload"], eq(&params));
        assert_that!(out[1]["item"]["text"], eq(&json!("STILL HERE")));
        assert_that!(t.last_message(), some(eq("STILL HERE")));

        // A turn whose end cannot be read still ends.
        let ended = t.notification("turn/completed", &json!({"threadId": "th"}), false);
        assert_that!(ended.finish, some(eq(Finish::Failed)));
    }
}
