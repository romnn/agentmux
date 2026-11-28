//! Folds a pi turn's capture, as the [driver](super::driver) writes it, into a turn.
//!
//! Changes when pi changes the events `pi --mode rpc` writes.
//!
//! The capture is every event pi wrote during the turn, except its answers to the driver's
//! commands, and three records of the driver's own: `session`, once pi has saved the session a
//! follow-up resumes, `agentmux.steer`, for each steer pi took, and `agentmux.warning`.
//!
//! # What this parser knows that pi does not document
//!
//! - **`agent_settled` alone ends a turn.**
//!   `agent_end` comes once per attempt, and pi goes on after it to retry, compact, or take a
//!   steer.
//!   The outcome is the last assistant message's `stopReason`: `agent_settled` says only whether
//!   the turn was aborted.
//! - **A failed provider call does not fail the stream.**
//!   It is an assistant message with `stopReason: "error"` and an `errorMessage`, which pi retries
//!   on its own, saying so in `auto_retry_start`.
//! - **What pi spent is reported per model call**, in each assistant `message_end`, and also by a
//!   compaction and by a cache refresh, which the turn's spend counts too.

use serde::Deserialize;
use serde_json::Value;

use crate::delegate::SessionRef;
use crate::stream::{Fold, MALFORMED_LINE, TurnInput, classify_reported, complete_lines};
use crate::transcript::{FailureKind, Message, Outcome, Role, SpendReport, Turn, Usage};

/// One line of the capture, by its `type`.
///
/// Every event pi 1.6.0 writes in RPC mode is named, the ones with nothing to fold included;
/// anything else reads as `Unknown` and is counted.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    /// The driver's: the session pi saved, by the id in its header.
    Session {
        id: String,
    },
    /// The driver's: a steer pi took into the turn.
    #[serde(rename = "agentmux.steer")]
    Steer {
        text: String,
    },
    /// The driver's: something the turn ran with that differs from what was asked.
    #[serde(rename = "agentmux.warning")]
    Warning {
        message: String,
    },
    AgentStart {},
    AgentSettled {
        #[serde(default)]
        aborted: bool,
    },
    MessageStart {
        message: MessageRole,
    },
    MessageUpdate {
        #[serde(rename = "assistantMessageEvent")]
        update: MessageUpdate,
    },
    MessageEnd {
        message: PiMessage,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionEnd {
        tool_name: String,
        #[serde(default)]
        result: Value,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(rename_all = "camelCase")]
    AutoRetryStart {
        attempt: u64,
        max_attempts: u64,
        #[serde(default)]
        error_message: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    AutoRetryEnd {
        success: bool,
        #[serde(default)]
        final_error: Option<String>,
    },
    CompactionEnd {
        #[serde(default)]
        result: Option<Reported>,
    },
    EntryAppended {
        entry: Entry,
    },
    /// The events with nothing to fold.
    #[serde(
        rename = "agent_end",
        alias = "turn_start",
        alias = "turn_end",
        alias = "tool_execution_start",
        alias = "tool_execution_update",
        alias = "queue_update",
        alias = "compaction_start",
        alias = "session_info_changed",
        alias = "thinking_level_changed",
        alias = "summarization_retry_scheduled",
        alias = "summarization_retry_attempt_start",
        alias = "summarization_retry_finished",
        alias = "extension_error"
    )]
    Narration {},
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct MessageRole {
    role: String,
}

/// One `message_update`, of which only the answer's text matters here.
#[derive(Debug, Deserialize)]
struct MessageUpdate {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
}

/// A message as pi completed it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiMessage {
    role: String,
    #[serde(default)]
    content: Value,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    usage: Option<PiUsage>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// A session entry pi appended, which carries usage when it is a cache refresh.
#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    usage: Option<PiUsage>,
}

/// Something pi reported with the usage it cost.
#[derive(Debug, Deserialize)]
struct Reported {
    #[serde(default)]
    usage: Option<PiUsage>,
}

/// pi's token figures, which count cache reads and writes beside `input`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiUsage {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
}

impl PiUsage {
    /// Add these figures to `total`, which is then known, keeping a figure unknown only while no
    /// report named it.
    fn add_to(&self, total: &mut Option<Usage>) {
        let total = total.get_or_insert_default();
        let add = |sum: &mut Option<u64>, figure: Option<u64>| {
            if let Some(figure) = figure {
                *sum = Some(sum.unwrap_or(0).saturating_add(figure));
            }
        };
        add(&mut total.input_tokens, self.input);
        add(&mut total.cached_input_tokens, self.cache_read);
        add(&mut total.cache_write_input_tokens, self.cache_write);
        add(&mut total.output_tokens, self.output);
        add(&mut total.reasoning_output_tokens, self.reasoning);
    }
}

/// The last assistant message, which decides how a settled turn ended.
struct LastAnswer {
    stop_reason: Option<String>,
    error_message: Option<String>,
}

/// Fold a pi turn's capture into a turn.
#[must_use]
pub fn fold(index: u32, question: &str, events: &str) -> Fold {
    let mut turn = Turn::new(index, question);
    let mut session = None;
    let mut input = TurnInput::Pending;
    // What pi reported spending, once it reported anything.
    let mut usage: Option<Usage> = None;
    let mut last_answer: Option<LastAnswer> = None;
    // The text of the assistant message under way, until pi completes it.
    let mut streaming: Option<String> = None;

    for line in complete_lines(events) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            turn.unrecognised.record(MALFORMED_LINE);
            continue;
        };
        let Ok(event) = Event::deserialize(&value) else {
            turn.unrecognised
                .record(drift_label(&value, "<unparsable>"));
            continue;
        };
        match event {
            Event::Session { id } => session = SessionRef::parse(&id).ok().or(session),
            Event::Steer { text } => turn.messages.push(Message::steer(text)),
            Event::Warning { message } => turn.messages.push(Message::synthetic(message)),
            Event::AgentStart {} => {
                if input == TurnInput::Pending {
                    input = TurnInput::Open;
                }
            }
            Event::MessageStart { message } => {
                if message.role == "assistant" {
                    streaming = Some(String::new());
                }
            }
            Event::MessageUpdate { update } => {
                if update.kind == "text_delta"
                    && let (Some(text), Some(delta)) = (streaming.as_mut(), update.delta)
                {
                    text.push_str(&delta);
                }
            }
            Event::MessageEnd { message } => {
                if message.role == "assistant" {
                    streaming = None;
                    fold_answer(&mut turn, &message);
                    if let Some(reported) = &message.usage {
                        reported.add_to(&mut usage);
                    }
                    last_answer = Some(LastAnswer {
                        stop_reason: message.stop_reason,
                        error_message: message.error_message,
                    });
                }
            }
            event @ (Event::ToolExecutionEnd { .. }
            | Event::AutoRetryStart { .. }
            | Event::AutoRetryEnd { .. }) => {
                turn.messages
                    .extend(note_of(&event).map(Message::synthetic));
            }
            Event::CompactionEnd {
                result: Some(Reported { usage: Some(spent) }),
            } => spent.add_to(&mut usage),
            Event::EntryAppended {
                entry:
                    Entry {
                        kind,
                        usage: Some(spent),
                    },
            } if kind == "usage" => spent.add_to(&mut usage),
            Event::AgentSettled { aborted } => {
                input = TurnInput::Closed;
                turn.outcome =
                    outcome_of(aborted, last_answer.as_ref(), &usage.unwrap_or_default());
                // Nothing may follow a terminal event, so the rendered transcript only grows at
                // its end.
                break;
            }
            Event::CompactionEnd { .. } | Event::EntryAppended { .. } | Event::Narration {} => {}
            Event::Unknown => turn.unrecognised.record(drift_label(&value, "")),
        }
    }

    turn.spend_report = usage.map(|usage| SpendReport {
        usage,
        cost_usd: None,
        cli_version: None,
    });
    Fold {
        turn,
        session,
        input,
        last_result: None,
        steers_taken: Vec::new(),
        unfinished: streaming.filter(|text| !text.trim().is_empty()),
    }
}

/// The note a failed tool call or one of pi's retries leaves in the turn, if any.
fn note_of(event: &Event) -> Option<String> {
    let reason =
        |text: &Option<String>| text.clone().unwrap_or_else(|| "no reason given".to_owned());
    match event {
        Event::ToolExecutionEnd {
            tool_name,
            result,
            is_error: true,
        } => Some(format!(
            "the delegate's `{tool_name}` call failed: {}",
            text_of(result.get("content").unwrap_or(&Value::Null))
        )),
        Event::AutoRetryStart {
            attempt,
            max_attempts,
            error_message,
        } => Some(format!(
            "delegate warning: pi retries, attempt {attempt} of {max_attempts}, after: {}",
            reason(error_message)
        )),
        Event::AutoRetryEnd {
            success: false,
            final_error,
        } => Some(format!(
            "delegate warning: pi gave up retrying: {}",
            reason(final_error)
        )),
        _ => None,
    }
}

/// What the drift count calls a line: its `type`, with `suffix`.
fn drift_label(value: &Value, suffix: &str) -> String {
    match value.get("type").and_then(Value::as_str) {
        Some(kind) if suffix.is_empty() => kind.to_owned(),
        Some(kind) => format!("{kind}.{suffix}"),
        None => MALFORMED_LINE.to_owned(),
    }
}

/// Keep what a completed assistant message said, and the model that said it.
fn fold_answer(turn: &mut Turn, message: &PiMessage) {
    if turn.resolved_model.is_none()
        && let (Some(provider), Some(model)) = (&message.provider, &message.model)
    {
        turn.resolved_model = Some(format!("{provider}/{model}"));
    }
    let text = text_of(&message.content);
    if !text.trim().is_empty() {
        turn.messages.push(Message::delegate(Role::Assistant, text));
    }
}

/// The text blocks of a message's content, joined; a content that is a string is its own text.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// How a settled turn ended: its last answer's `stopReason` decides, unless pi aborted it.
fn outcome_of(aborted: bool, last_answer: Option<&LastAnswer>, usage: &Usage) -> Outcome {
    let failed = |detail: String| Outcome::Failed {
        kind: classify_reported(&detail),
        detail,
    };
    if aborted {
        return Outcome::Failed {
            kind: FailureKind::Unclassified,
            detail: "pi aborted the turn".to_owned(),
        };
    }
    let Some(LastAnswer {
        stop_reason,
        error_message,
    }) = last_answer
    else {
        return failed("pi settled the turn without an answer".to_owned());
    };
    match stop_reason.as_deref() {
        Some("stop") => Outcome::Completed {
            usage: *usage,
            cost_usd: None,
        },
        reason => failed(error_message.clone().unwrap_or_else(|| {
            format!(
                "pi stopped the answer with `{}`",
                reason.unwrap_or("no stop reason")
            )
        })),
    }
}
