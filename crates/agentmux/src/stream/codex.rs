//! Folds the Codex CLI's `--json` event stream into a turn.
//!
//! Changes when the Codex CLI changes its JSON.
//!
//! # What this parser knows that the CLI does not document
//!
//! - **An `item.completed` whose `item.type` is `error` is not necessarily fatal.**
//!   A resume against a model that differs from the recorded one emits exactly that shape as a
//!   *warning*, and the turn then completes normally.
//!   Treating it as terminal would fail a consultation that in fact succeeded, so it becomes a
//!   note in the transcript and nothing more.
//! - **Neither is a top-level `error`.**
//!   codex-cli 0.159.3 writes one for every error its server reports, including one it is about
//!   to retry, after which the turn goes on and may complete.
//!   It becomes a note where it arrived; only `turn.failed` ends a turn badly.
//! - One failure arrives as up to three events — an `item.completed` error, a top-level `error`,
//!   and `turn.failed` — all carrying the same message.
//!   The first two are notes, and the terminal one ends the fold with the outcome.
//! - Unknown event and `item.type` values are counted, not skipped.
//!   If a future release renamed `agent_message`, a parser that filtered on the name alone would
//!   silently drop every report.
//!
//! The events are read as [`codex_app_server_protocol::exec`] types, codex's own mirrored and
//! checked against codex at the pinned release, so the names this parser knows are exactly the
//! ones codex writes.
//!
//! # The same stream from `codex app-server`
//!
//! A turn driven over `codex app-server` is written in this same shape by
//! [`crate::appserver`], which translates the server's notifications as they arrive, so one parser
//! serves both launch modes.
//! It writes the same [`CaptureEvent`] this parser reads, and adds what `codex exec` never writes:
//! an instruction the caller steered into the running turn, and drift it could not read.

use codex_app_server_protocol::exec::{self, ThreadEvent, ThreadItemDetails};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delegate::SessionRef;
use crate::stream::{Fold, MALFORMED_LINE, classify, complete_lines};
use crate::transcript::{Message, Outcome, Role, Turn, Usage};

/// One line of a Codex capture: an event `codex exec --json` writes, or one agentmux adds.
///
/// agentmux's own are namespaced, so that no event codex writes can be read as one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CaptureEvent {
    /// An instruction the caller steered into the running turn, recorded the moment the server
    /// accepted it.
    #[serde(rename = "agentmux.steer")]
    Steer {
        /// The instruction.
        text: String,
    },
    /// Something `codex app-server` said that the generated types could not read, kept with
    /// what it said.
    #[serde(rename = "agentmux.undecoded")]
    Undecoded(UndecodedServerOutput),
    /// An event as `codex exec --json` writes it.
    #[serde(untagged)]
    Exec(Box<ThreadEvent>),
}

impl From<ThreadEvent> for CaptureEvent {
    fn from(event: ThreadEvent) -> Self {
        Self::Exec(Box::new(event))
    }
}

/// What the server said that could not be read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UndecodedServerOutput {
    /// A message whose payload did not match the generated types, or whose method the pinned
    /// schema never had.
    Message {
        /// The method.
        method: String,
        /// Why it could not be read.
        reason: String,
        /// The payload, verbatim.
        payload: Value,
    },
    /// A line that was no JSON-RPC message at all.
    Line {
        /// Why it could not be read.
        reason: String,
        /// The line, verbatim.
        line: String,
    },
}

/// Enough of a line to name it in the drift count: its `type`, and its item's.
#[derive(Debug, Deserialize)]
struct EventTypeProbe {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    item: Option<ItemTypeProbe>,
}

#[derive(Debug, Deserialize)]
struct ItemTypeProbe {
    #[serde(rename = "type")]
    kind: String,
}

/// What the drift count calls a line it cannot read: its `type`.
fn unparsable_drift_label(line: &Value) -> String {
    match EventTypeProbe::deserialize(line) {
        Ok(EventTypeProbe { kind, .. }) => format!("{kind}.<unparsable>"),
        Err(_) => MALFORMED_LINE.to_owned(),
    }
}

/// What the drift count calls a line: its `type`, and its item's when it carries one.
fn drift_label(line: &Value) -> String {
    match EventTypeProbe::deserialize(line) {
        Ok(EventTypeProbe {
            kind,
            item: Some(item),
        }) => format!("{kind}.{}", item.kind),
        Ok(EventTypeProbe { kind, item: None }) => kind,
        Err(_) => MALFORMED_LINE.to_owned(),
    }
}

impl From<&exec::Usage> for Usage {
    fn from(usage: &exec::Usage) -> Self {
        let to_unsigned_token_count =
            |tokens: Option<i64>| tokens.and_then(|tokens| u64::try_from(tokens).ok());
        Self {
            input_tokens: to_unsigned_token_count(usage.input_tokens),
            cached_input_tokens: to_unsigned_token_count(usage.cached_input_tokens),
            cache_write_input_tokens: to_unsigned_token_count(usage.cache_write_input_tokens),
            output_tokens: to_unsigned_token_count(usage.output_tokens),
            reasoning_output_tokens: to_unsigned_token_count(usage.reasoning_output_tokens),
        }
    }
}

/// Fold a Codex `--json` stream into a turn.
#[must_use]
pub fn fold(index: u32, question: &str, events: &str) -> Fold {
    let mut turn = Turn::new(index, question);
    let mut session = None;
    let mut turn_started = false;

    for line in complete_lines(events) {
        // Read as a JSON value first, which settles a key codex writes twice; see
        // `exec::ThreadItem`.
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            turn.unrecognised.record(MALFORMED_LINE);
            continue;
        };
        let event = match CaptureEvent::deserialize(&value) {
            Ok(CaptureEvent::Exec(event)) => *event,
            Ok(CaptureEvent::Steer { text }) => {
                turn.messages.push(Message::steer(text));
                continue;
            }
            // Drift the driver recorded, counted as such.
            Ok(CaptureEvent::Undecoded(_)) => {
                turn.unrecognised.record(drift_label(&value));
                turn.has_undecoded_app_server_messages = true;
                continue;
            }
            // An event this parser names, in a shape it does not.
            Err(_) => {
                turn.unrecognised.record(unparsable_drift_label(&value));
                continue;
            }
        };

        match event {
            ThreadEvent::ThreadStarted(started) => {
                if let Some(parsed) = started
                    .thread_id
                    .as_deref()
                    .and_then(|id| SessionRef::parse(id).ok())
                {
                    session = Some(parsed);
                }
            }
            ThreadEvent::TurnStarted(_) => turn_started = true,
            ThreadEvent::ItemStarted(_) | ThreadEvent::ItemUpdated(_) => {}
            ThreadEvent::ItemCompleted(completed) => match completed.item.details {
                ThreadItemDetails::AgentMessage(message) => match message.text {
                    Some(text) if !text.trim().is_empty() => {
                        turn.messages.push(Message::delegate(Role::Assistant, text));
                    }
                    // A reply with no `text` is not an empty reply; it means the payload field
                    // was renamed, and defaulting it away would drop the report.
                    None => turn.unrecognised.record(drift_label(&value) + ".text"),
                    Some(_) => {}
                },
                // Advisory, not terminal.
                // See the module docs.
                ThreadItemDetails::Error(warning) => {
                    if let Some(text) = warning.message.filter(|text| !text.trim().is_empty()) {
                        turn.messages
                            .push(Message::synthetic(format!("delegate warning: {text}")));
                    }
                }
                // Counted rather than skipped: if a release renamed `agent_message`, this count
                // would be all that stands between a silently empty report and a caller who
                // notices.
                ThreadItemDetails::Unknown => turn.unrecognised.record(drift_label(&value)),
                // The delegate narrating its own work: kept in the capture and, like all
                // narration, not rendered.
                ThreadItemDetails::Reasoning(_)
                | ThreadItemDetails::CommandExecution(_)
                | ThreadItemDetails::FileChange(_)
                | ThreadItemDetails::McpToolCall(_)
                | ThreadItemDetails::CollabToolCall(_)
                | ThreadItemDetails::WebSearch(_)
                | ThreadItemDetails::TodoList(_) => {}
            },
            ThreadEvent::TurnCompleted(completed) => {
                if !turn.outcome.is_terminal() {
                    turn.outcome = Outcome::Completed {
                        usage: completed
                            .usage
                            .as_ref()
                            .map(Usage::from)
                            .unwrap_or_default(),
                        cost_usd: None,
                    };
                }
                // See the note in the Claude parser: a terminal event ends the fold so the
                // rendered transcript can only ever grow at its end.
                break;
            }
            // Advisory, not terminal.
            // See the module docs.
            ThreadEvent::Error(error) => {
                let message = error
                    .message
                    .filter(|message| !message.trim().is_empty())
                    .unwrap_or_else(|| "delegate reported an error".to_owned());
                turn.messages
                    .push(Message::synthetic(format!("delegate error: {message}")));
            }
            ThreadEvent::TurnFailed(failed) => {
                let message = failed
                    .error
                    .and_then(|error| error.message)
                    .unwrap_or_else(|| "delegate reported an error".to_owned());
                turn.outcome = Outcome::Failed {
                    kind: classify(&message),
                    detail: message,
                };
                break;
            }
            ThreadEvent::Unknown => turn.unrecognised.record(drift_label(&value)),
        }
    }

    Fold {
        turn,
        session,
        turn_started,
    }
}
