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
//!   Whether that injection *reopened* a finished turn or merely added context mid-turn is
//!   decided by what the delegate was doing when it arrived: after a message that called a
//!   tool, the turn was still under way; after a message that did not, the turn had ended.

use std::fmt::Write as _;

use chrono::DateTime;
use serde::Deserialize;

use crate::delegate::SessionRef;
use crate::stream::{Fold, MALFORMED_LINE, classify, complete_lines, probe};
use crate::transcript::{FailureKind, Message, Outcome, RateLimit, Role, Turn, Usage};

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
    /// The model the vendor actually ran, which need not be the identifier that was asked for.
    #[serde(default)]
    model: Option<String>,
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
    /// Absent on every non-text block; absent on a *text* block only if the vendor renamed it,
    /// which is counted rather than read as an empty message.
    #[serde(default)]
    text: Option<String>,
}

/// Content block types that carry no prose and are dropped on purpose.
///
/// Named so that a block type outside this list is counted rather than filtered.
/// A renamed text block — `output_text` in place of `text`, say — would otherwise parse cleanly,
/// be dropped by the filter, and take the report with it without a single diagnostic.
const KNOWN_SILENT_BLOCKS: &[&str] = &["tool_use", "tool_result", "thinking", "image"];

/// What one message's content amounts to.
#[derive(Debug, Default)]
struct Inspected<'a> {
    /// The prose, in order.
    texts: Vec<&'a str>,
    /// Block shapes the parser does not account for, for the drift counter.
    unknown: Vec<&'a str>,
    /// Whether the message called a tool, which means the turn was not over when it was sent.
    calls_a_tool: bool,
}

impl Content {
    /// The prose of a message, plus any block shape the parser does not account for.
    fn inspect(&self) -> Inspected<'_> {
        match self {
            Self::Text(text) => Inspected {
                texts: vec![text.as_str()],
                ..Inspected::default()
            },
            Self::Blocks(blocks) => {
                let mut inspected = Inspected::default();
                for block in blocks {
                    match (block.kind.as_str(), block.text.as_deref()) {
                        ("text", Some(text)) => inspected.texts.push(text),
                        // A text block without its text is not an empty message; it is the
                        // payload field renamed, and dropping it would drop the report.
                        ("text", None) => inspected.unknown.push("text.<missing>"),
                        ("tool_use", _) => inspected.calls_a_tool = true,
                        (kind, _) if !KNOWN_SILENT_BLOCKS.contains(&kind) => {
                            inspected.unknown.push(kind);
                        }
                        _ => {}
                    }
                }
                inspected
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
    ///
    /// Required, not defaulted: a `result` without it is a `result` whose shape changed, and
    /// defaulting it to "no error" would turn that drift into a clean, empty completion.
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
    /// Unix seconds at which the window reopens.
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<i64>,
    #[serde(default, rename = "rateLimitType")]
    rate_limit_type: Option<String>,
}

/// Record what a `rate_limit_event` says about the account's usage window.
///
/// The window is kept whatever the status says, because a run that succeeded against a nearly
/// closed window still tells the caller what to expect from the next question.
/// Only a refusal earns a line in the transcript.
fn fold_rate_limit(turn: &mut Turn, line: &str) {
    let Ok(event) = serde_json::from_str::<RateLimitEvent>(line) else {
        turn.unrecognised.record("rate_limit_event.<unparsable>");
        return;
    };
    let info = event.rate_limit_info;

    if let Some(resets_at) = info.resets_at.and_then(DateTime::from_timestamp_secs) {
        turn.rate_limit = Some(RateLimit {
            resets_at,
            window: info.rate_limit_type.clone(),
        });
    }

    if let Some(status) = info.status.filter(|s| s != "allowed") {
        let mut note = format!("delegate rate limit status: {status}");
        if let Some(limit) = &turn.rate_limit {
            match &limit.window {
                Some(window) => {
                    let _ = write!(note, "; the {window} window reopens at ");
                }
                None => {
                    let _ = write!(note, "; the window reopens at ");
                }
            }
            let _ = write!(note, "{}", limit.resets_at.to_rfc3339());
        }
        turn.messages.push(Message::synthetic(note));
    }
}

/// Fold a Claude `stream-json` stream into a turn.
#[must_use]
pub fn fold(index: u32, question: &str, events: &str) -> Fold {
    let mut turn = Turn::new(index, question);
    let mut session = None;
    let mut reopened = false;
    // What the delegate was doing when a `user` text message arrives decides what that message
    // is: see [`classify_injection`].
    let mut delegate = DelegateActivity::Silent;

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
                // The first message that names one is kept: later messages in a turn repeat it,
                // and a subagent's model was skipped above with the rest of its conversation.
                if turn.resolved_model.is_none() {
                    turn.resolved_model.clone_from(&event.message.model);
                }
                let inspected = event.message.content.inspect();
                for kind in inspected.unknown {
                    turn.unrecognised
                        .record(format!("assistant.content.{kind}"));
                }
                delegate = if inspected.calls_a_tool {
                    DelegateActivity::CallingATool
                } else {
                    DelegateActivity::Finished
                };
                for text in inspected.texts {
                    if text.trim().is_empty() {
                        continue;
                    }
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
                let inspected = event.message.content.inspect();
                for kind in inspected.unknown {
                    turn.unrecognised.record(format!("user.content.{kind}"));
                }
                for text in inspected.texts {
                    if text.trim().is_empty() {
                        continue;
                    }
                    turn.messages
                        .push(match classify_injection(text, delegate) {
                            Injection::Reopening => {
                                reopened = true;
                                Message::hook_injection(text)
                            }
                            Injection::Context => Message::hook_context(text),
                            Injection::None => Message::delegate(Role::User, text),
                        });
                }
            }
            "rate_limit_event" => fold_rate_limit(&mut turn, line),
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

/// What the delegate was doing when a `user` text message arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelegateActivity {
    /// It has not said anything yet.
    Silent,
    /// Its latest message called a tool, so the turn is still under way.
    CallingATool,
    /// Its latest message called no tool, so as far as the model is concerned the turn ended.
    Finished,
}

/// What a `user` text message turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Injection {
    /// Nothing injected: an ordinary turn input.
    None,
    /// A hook added context while the delegate was still working.
    Context,
    /// A hook reopened a turn the delegate had finished, so what follows is not the answer.
    Reopening,
}

/// Decide what a `user` text message is from what the delegate was doing when it arrived.
///
/// The test is **structural**, not textual.
/// Under `--print` the delegate's own tool results arrive as `tool_result` blocks, which never
/// reach this function, and subagent chatter is filtered by `parent_tool_use_id`.
/// So a `text` block attributed to the user, arriving after the delegate has already spoken, is by
/// construction something that was injected — and whether it *reopened* the turn follows from the
/// delegate's latest message: one that called a tool was mid-turn and the injection is context
/// (a `PostToolUse` hook, say); one that did not had finished, and the injection reopened it.
///
/// Matching the CLI's `"… hook feedback:"` wording would be simpler and is the wrong test: the day
/// that string is reworded, the injection would be recorded as an ordinary user message, nothing
/// would be counted, no warning would be rendered, and the reopened turn's output would once again
/// read as the answer.
/// That is precisely the silent loss this crate exists to prevent, so detection must not depend on
/// prose.
/// The wording is consulted only before the delegate has spoken, where structure cannot tell an
/// injected prelude from the prompt.
fn classify_injection(text: &str, delegate: DelegateActivity) -> Injection {
    match delegate {
        DelegateActivity::Finished => Injection::Reopening,
        DelegateActivity::CallingATool => Injection::Context,
        DelegateActivity::Silent if names_a_hook(text) => Injection::Context,
        DelegateActivity::Silent => Injection::None,
    }
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
