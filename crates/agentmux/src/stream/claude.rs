//! Folds the Claude CLI's `stream-json` event stream into a turn.
//!
//! Changes when the Claude CLI changes its JSON.
//!
//! # What this parser knows that the CLI does not document
//!
//! - `result.result` is **not** the answer.
//!   When a `Stop` hook blocks, the CLI reopens the finished turn and `result.result` becomes
//!   whatever the reopened turn produced — in a measured run, the empty string, while the real
//!   report sat two messages further up.
//!   Every `assistant` message is therefore captured and `result.result` is never read as the
//!   deliverable.
//! - `result.subtype` is unreliable.
//!   An unknown model produces `subtype: "success"` alongside `is_error: true` and
//!   `terminal_reason: "api_error"`.
//!   Branch on `is_error`.
//! - Assistant events with empty text are routine (a thinking-only step) and are dropped.
//! - `parent_tool_use_id` is non-null for subagent chatter, which is not part of the answer.
//! - Tool results arrive as `user` events whose content blocks are `tool_result`, never `text`.
//!   That is what makes the reopening test below structural: in `--print` mode the *only* way a
//!   `user` message carrying a `text` block reaches the main conversation is an injection.

use serde::Deserialize;

use crate::delegate::SessionRef;
use crate::stream::{Fold, MALFORMED_LINE, classify, complete_lines, probe};
use crate::transcript::{FailureKind, Message, Outcome, Role, Turn, Usage};

/// The prefix the CLI puts on a `user` message a blocking hook injected.
///
/// Used only to *name* the injection in the transcript.
/// Detection does not depend on it: see [`is_reopening`].
/// If the CLI rewords this, the injection is still caught and still marked, and only the wording
/// of the note changes.
const HOOK_FEEDBACK_MARKER: &str = "hook feedback:";

/// `system` subtypes that have been observed with the exact flags [`crate::delegate`] builds.
///
/// Deliberately short.
/// Every name here is an event the drift counter will never report, so a name added on a guess is
/// a hole in the guarantee.
/// A new subtype shows up as a counted note in `status`, which is loud and harmless; the
/// alternative is silence.
const KNOWN_SYSTEM_SUBTYPES: &[&str] = &[
    "init",
    "thinking_tokens",
    "notification",
    "hook_started",
    "hook_response",
];

#[derive(Debug, Deserialize)]
struct SystemEvent {
    subtype: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageEvent {
    message: MessageBody,
    /// Non-null marks a message from inside a subagent rather than the main conversation.
    #[serde(default)]
    parent_tool_use_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    #[serde(default)]
    content: Content,
}

/// `content` is a bare string on some events and a block list on others.
///
/// There is deliberately no catch-all variant: a third shape would fail the whole event and be
/// counted as drift, which is the loud outcome.
/// Unknown *block* types are handled — they parse and are filtered by `kind` — so only a genuine
/// change to `content` itself trips this.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl Default for Content {
    fn default() -> Self {
        Self::Blocks(Vec::new())
    }
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

/// Content block types that carry no prose and are dropped on purpose.
///
/// Named so that a block type outside this list is counted rather than filtered.
/// A renamed text block — `output_text` in place of `text`, say — would otherwise parse cleanly,
/// be dropped by the filter, and take the report with it without a single diagnostic.
const KNOWN_SILENT_BLOCKS: &[&str] = &["tool_use", "tool_result", "thinking", "image"];

impl Content {
    /// The prose of a message, plus any block type the parser does not account for.
    fn texts(&self) -> (Vec<&str>, Vec<&str>) {
        match self {
            Self::Text(text) => (vec![text.as_str()], Vec::new()),
            Self::Blocks(blocks) => {
                let mut texts = Vec::new();
                let mut unknown = Vec::new();
                for block in blocks {
                    if block.kind == "text" {
                        texts.push(block.text.as_str());
                    } else if !KNOWN_SILENT_BLOCKS.contains(&block.kind.as_str()) {
                        unknown.push(block.kind.as_str());
                    }
                }
                (texts, unknown)
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResultEvent {
    /// The authoritative success flag.
    ///
    /// The event also carries a `subtype`, which is deliberately not modelled: it reads
    /// `"success"` on a failed turn, so it is useless for the decision and misleading in a
    /// message.
    /// Branch on this field alone.
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    terminal_reason: Option<String>,
    #[serde(default)]
    api_error_status: Option<i64>,
    /// The reopened turn's text when a hook blocked.
    /// Read only as failure detail, never as the answer.
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    usage: Option<ClaudeUsage>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokenDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct OutputTokenDetails {
    thinking_tokens: Option<u64>,
}

impl From<&ClaudeUsage> for Usage {
    fn from(usage: &ClaudeUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cache_read_input_tokens,
            cache_write_input_tokens: usage.cache_creation_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage
                .output_tokens_details
                .as_ref()
                .and_then(|details| details.thinking_tokens),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RateLimitEvent {
    rate_limit_info: RateLimitInfo,
}

#[derive(Debug, Deserialize)]
struct RateLimitInfo {
    #[serde(default)]
    status: Option<String>,
}

/// Fold a Claude `stream-json` stream into a turn.
#[must_use]
pub fn fold(index: u32, question: &str, events: &str) -> Fold {
    let mut turn = Turn::new(index, question);
    let mut session = None;
    let mut reopened = false;
    // Whether the delegate has said anything yet.
    // A `user` text message before it speaks is the prompt; one after it has spoken is something
    // reopening a turn it had finished.
    let mut spoken = false;

    for line in complete_lines(events) {
        let Some(kind) = probe(line) else {
            turn.unrecognised.record(MALFORMED_LINE);
            continue;
        };

        match kind.as_str() {
            "system" => match serde_json::from_str::<SystemEvent>(line) {
                Ok(event) => {
                    let subtype = event.subtype.as_deref().unwrap_or("");
                    if subtype == "init"
                        && let Some(id) = event.session_id.as_deref()
                        && let Ok(parsed) = SessionRef::parse(id)
                    {
                        session = Some(parsed);
                    }
                    if !KNOWN_SYSTEM_SUBTYPES.contains(&subtype) {
                        turn.unrecognised.record(format!("system.{subtype}"));
                    }
                }
                Err(_) => turn.unrecognised.record("system.<unparsable>"),
            },
            "assistant" => {
                let Ok(event) = serde_json::from_str::<MessageEvent>(line) else {
                    turn.unrecognised.record("assistant.<unparsable>");
                    continue;
                };
                if event.parent_tool_use_id.is_some() {
                    continue;
                }
                let (texts, unknown) = event.message.content.texts();
                for kind in unknown {
                    turn.unrecognised
                        .record(format!("assistant.content.{kind}"));
                }
                for text in texts {
                    if text.trim().is_empty() {
                        continue;
                    }
                    spoken = true;
                    turn.messages.push(Message::delegate(Role::Assistant, text));
                }
            }
            "user" => {
                let Ok(event) = serde_json::from_str::<MessageEvent>(line) else {
                    turn.unrecognised.record("user.<unparsable>");
                    continue;
                };
                if event.parent_tool_use_id.is_some() {
                    continue;
                }
                let (texts, unknown) = event.message.content.texts();
                for kind in unknown {
                    turn.unrecognised.record(format!("user.content.{kind}"));
                }
                for text in texts {
                    if text.trim().is_empty() {
                        continue;
                    }
                    if is_reopening(text, spoken) {
                        reopened = true;
                        turn.messages.push(Message::hook_injection(text));
                    } else {
                        turn.messages.push(Message::delegate(Role::User, text));
                    }
                }
            }
            "rate_limit_event" => {
                if let Ok(event) = serde_json::from_str::<RateLimitEvent>(line)
                    && event
                        .rate_limit_info
                        .status
                        .as_deref()
                        .is_some_and(|s| s != "allowed")
                {
                    let status = event.rate_limit_info.status.unwrap_or_default();
                    turn.messages.push(Message::synthetic(format!(
                        "delegate rate limit status: {status}"
                    )));
                }
            }
            "result" => {
                let Ok(event) = serde_json::from_str::<ResultEvent>(line) else {
                    turn.unrecognised.record("result.<unparsable>");
                    continue;
                };
                turn.outcome = outcome_of(&event, reopened);
                // Nothing may follow a terminal event.
                // Stopping here keeps the rendered transcript strictly append-only, which is what
                // `tail`'s byte cursor relies on.
                break;
            }
            other => turn.unrecognised.record(other),
        }
    }

    Fold { turn, session }
}

/// Whether a `user` text message is something reopening a turn the delegate had finished.
///
/// The test is **structural**, not textual.
/// Under `--print` the delegate's own tool results arrive as `tool_result` blocks, which never
/// reach this function, and subagent chatter is filtered by `parent_tool_use_id`.
/// So a `text` block attributed to the user, arriving after the delegate has already spoken, is by
/// construction something that was injected into a finished turn.
///
/// Matching the CLI's `"… hook feedback:"` wording would be simpler and is the wrong test: the day
/// that string is reworded, the injection would be recorded as an ordinary user message, nothing
/// would be counted, no warning would be rendered, and the reopened turn's output would once again
/// read as the answer.
/// That is precisely the silent loss this crate exists to prevent, so detection must not depend on
/// prose.
fn is_reopening(text: &str, delegate_has_spoken: bool) -> bool {
    delegate_has_spoken || names_a_hook(text)
}

/// Whether the text is the CLI's own hook-feedback wording, for naming rather than detection.
fn names_a_hook(text: &str) -> bool {
    text.lines()
        .next()
        .is_some_and(|first| first.to_ascii_lowercase().contains(HOOK_FEEDBACK_MARKER))
}

/// Decide a turn's outcome from the terminal `result` event.
///
/// `is_error` is authoritative; `subtype` is not, because an unknown model reports
/// `subtype: "success"` and `is_error: true` together.
fn outcome_of(event: &ResultEvent, reopened: bool) -> Outcome {
    if !event.is_error {
        return Outcome::Completed {
            usage: event.usage.as_ref().map(Usage::from).unwrap_or_default(),
            cost_usd: event.total_cost_usd,
        };
    }

    let reason = event.terminal_reason.as_deref().unwrap_or("");
    // `subtype` is deliberately left out of the detail.
    // It reads `success` on a failed turn, so quoting it produces "failed … : success", which
    // invites a reader to doubt the failure rather than act on it.
    //
    // `result` is the reopened turn's text when a hook blocked, so it is unusable as failure
    // detail in that case; the terminal reason still is.
    let text = if reopened {
        ""
    } else {
        event.result.as_deref().unwrap_or("")
    };
    let detail = [reason, text]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(": ");

    let kind = match (reason, event.api_error_status) {
        (_, Some(404)) => FailureKind::ModelUnavailable,
        (_, Some(429)) => FailureKind::RateLimited,
        (_, Some(402)) => FailureKind::BudgetExceeded,
        ("max_turns" | "turn_limit", _) => FailureKind::TurnLimit,
        ("refusal", _) => FailureKind::ContentFlagged,
        _ => classify(&detail),
    };

    Outcome::Failed {
        kind,
        detail: if detail.is_empty() {
            "delegate reported an error".to_owned()
        } else {
            detail
        },
    }
}
