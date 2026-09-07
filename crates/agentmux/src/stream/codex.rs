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
//!   Only `turn.failed` and a top-level `error` end a turn.
//! - One failure arrives as up to three events — an `item.completed` error, a top-level `error`,
//!   and `turn.failed` — all carrying the same message.
//!   The first terminal event ends the fold, so they fold into one outcome.
//! - Unknown `item.type` values are counted, not skipped.
//!   If a future release renamed `agent_message`, a parser that filtered on the name alone would
//!   silently drop every report.

use serde::Deserialize;

use crate::delegate::SessionRef;
use crate::stream::{Fold, MALFORMED_LINE, classify, complete_lines, probe};
use crate::transcript::{Message, Outcome, Role, Turn, Usage};

/// `item.type` values that have been observed with the exact flags [`crate::delegate`] builds.
///
/// Only `agent_message` carries the answer.
/// `command_execution`, `mcp_tool_call` and `web_search` are the delegate narrating its own work,
/// and `error` is advisory.
/// The last two were added after a real `gpt-6-astra` run at `effort=xhigh` reported them as
/// drift, which is the counter working: a reviewing delegate searches the web and calls tools that
/// a one-line fixture never does.
/// The list is deliberately short: every name here is drift the counter will never report, so
/// naming a type on a guess is a hole in the guarantee.
/// A type that turns up later is counted, which is a loud note in `status` rather than a failure —
/// and if a future release renames `agent_message`, that count is the only thing standing between
/// a silently empty report and a caller who notices.
const KNOWN_ITEM_TYPES: &[&str] = &[
    "agent_message",
    "command_execution",
    "mcp_tool_call",
    "web_search",
    "error",
];

#[derive(Debug, Deserialize)]
struct ThreadStarted {
    thread_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ItemEvent {
    item: Item,
}

#[derive(Debug, Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TurnCompleted {
    #[serde(default)]
    usage: Option<CodexUsage>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "field names are Codex's own JSON keys; renaming them would need serde renames that \
              hide the wire format"
)]
struct CodexUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
}

impl From<&CodexUsage> for Usage {
    fn from(usage: &CodexUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_write_input_tokens: usage.cache_write_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ErrorEvent {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TurnFailed {
    #[serde(default)]
    error: Option<ErrorEvent>,
}

/// Fold a Codex `--json` stream into a turn.
#[must_use]
pub fn fold(index: u32, question: &str, events: &str) -> Fold {
    let mut turn = Turn::new(index, question);
    let mut session = None;

    for line in complete_lines(events) {
        let Some(kind) = probe(line) else {
            turn.unrecognised.record(MALFORMED_LINE);
            continue;
        };

        match kind.as_str() {
            "thread.started" => {
                if let Ok(event) = serde_json::from_str::<ThreadStarted>(line)
                    && let Some(id) = event.thread_id.as_deref()
                    && let Ok(parsed) = SessionRef::parse(id)
                {
                    session = Some(parsed);
                }
            }
            "turn.started" | "item.started" | "item.updated" => {}
            "item.completed" => {
                let Ok(event) = serde_json::from_str::<ItemEvent>(line) else {
                    turn.unrecognised.record("item.completed.<unparsable>");
                    continue;
                };
                if !KNOWN_ITEM_TYPES.contains(&event.item.kind.as_str()) {
                    turn.unrecognised
                        .record(format!("item.completed.{}", event.item.kind));
                    continue;
                }
                match event.item.kind.as_str() {
                    "agent_message" => match event.item.text {
                        Some(text) if !text.trim().is_empty() => {
                            turn.messages.push(Message::delegate(Role::Assistant, text));
                        }
                        // A reply with no `text` is not an empty reply; it means the payload
                        // field was renamed, and defaulting it away would drop the report.
                        None => turn
                            .unrecognised
                            .record("item.completed.agent_message.text"),
                        Some(_) => {}
                    },
                    // Advisory, not terminal.
                    // See the module docs.
                    "error" => {
                        if let Some(text) = event.item.message.filter(|t| !t.trim().is_empty()) {
                            turn.messages
                                .push(Message::synthetic(format!("delegate warning: {text}")));
                        }
                    }
                    _ => {}
                }
            }
            "turn.completed" => {
                let Ok(event) = serde_json::from_str::<TurnCompleted>(line) else {
                    turn.unrecognised.record("turn.completed.<unparsable>");
                    continue;
                };
                if !turn.outcome.is_terminal() {
                    turn.outcome = Outcome::Completed {
                        usage: event.usage.as_ref().map(Usage::from).unwrap_or_default(),
                        cost_usd: None,
                    };
                }
                // See the note in the Claude parser: a terminal event ends the fold so the
                // rendered transcript can only ever grow at its end.
                break;
            }
            "error" => {
                let message = serde_json::from_str::<ErrorEvent>(line)
                    .ok()
                    .and_then(|event| event.message)
                    .unwrap_or_else(|| "delegate reported an error".to_owned());
                turn.outcome = Outcome::Failed {
                    kind: classify(&message),
                    detail: message,
                };
                // Terminal, like `turn.failed` below: the `turn.failed` that follows carries the
                // same text, and reading on past a terminal event is what would let a later line
                // land above a footer already handed out.
                break;
            }
            "turn.failed" => {
                let message = serde_json::from_str::<TurnFailed>(line)
                    .ok()
                    .and_then(|event| event.error)
                    .and_then(|error| error.message)
                    .unwrap_or_else(|| "delegate reported an error".to_owned());
                turn.outcome = Outcome::Failed {
                    kind: classify(&message),
                    detail: message,
                };
                break;
            }
            other => turn.unrecognised.record(other),
        }
    }

    Fold { turn, session }
}
