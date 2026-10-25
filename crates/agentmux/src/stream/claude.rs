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
use serde_json::Value;

use crate::delegate::SessionRef;
use crate::stream::{Fold, MALFORMED_LINE, classify, complete_lines};
use crate::transcript::{FailureKind, Message, Outcome, Role, Turn, Usage, UsageWindow};

/// The prefix the CLI puts on a `user` message a blocking hook injected.
///
/// Used only to *name* the injection in the transcript.
/// Detection does not depend on it: see [`classify_injection`].
/// If the CLI rewords this, the injection is still caught and still marked, and only the wording
/// of the note changes.
const HOOK_FEEDBACK_MARKER: &str = "hook feedback:";

/// One line of the stream, by its `type`.
///
/// Every variant is one that has been observed with the exact flags [`crate::delegate`] builds,
/// and deliberately no more: each is an event the drift counter will never report, so a name
/// added on a guess is a hole in the guarantee.
/// A new one reads as `Unknown` and is counted, which is loud and harmless; the alternative is
/// silence.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    System(SystemEvent),
    Assistant(MessageEvent),
    User(MessageEvent),
    /// A heartbeat while a tool runs, carrying only how long it has been going.
    /// It says nothing the transcript keeps, but it has to be named: an event type nobody names
    /// is reported as drift, and a turn that merely ran a slow command would report some.
    ToolProgress {},
    RateLimitEvent(RateLimitEvent),
    Result(ResultEvent),
    #[serde(other)]
    Unknown,
}

/// A `system` event, by its `subtype`.
#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
enum SystemEvent {
    Init(Init),
    ThinkingTokens {},
    Notification {},
    HookStarted {},
    HookResponse {},
    /// A backgrounded tool starting and finishing.
    /// Both carry a description and a status and no part of the answer: what the task actually
    /// produced arrives as an ordinary `tool_result`.
    TaskStarted {},
    TaskNotification {},
    /// The same task's status changing, and the set of running background tasks changing.
    /// A status patch and a task list; still nothing a reader of the answer needs.
    TaskUpdated {},
    BackgroundTasksChanged {},
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct Init {
    session_id: Option<String>,
    /// Where the CLI found an API key.
    ///
    /// The environment agentmux hands a subscription delegate carries no key, but the CLI has
    /// other sources an inheriting delegate may load — a settings file's `env` table or its
    /// `apiKeyHelper`, or a login made with a Console key — and this is the CLI's own report of
    /// which one it used.
    #[serde(default, rename = "apiKeySource")]
    api_key_source: Option<ApiKeySource>,
}

/// Where the CLI found an API key, which it says is `none`, in any case, when it runs on a
/// logged-in subscription.
#[derive(Debug, Deserialize)]
#[serde(from = "String")]
enum ApiKeySource {
    None,
    Named(String),
}

impl From<String> for ApiKeySource {
    fn from(source: String) -> Self {
        if source.eq_ignore_ascii_case("none") {
            Self::None
        } else {
            Self::Named(source)
        }
    }
}

/// Enough of a line to name it in the drift count.
#[derive(Debug, Deserialize)]
struct EventTypeProbe {
    #[serde(rename = "type")]
    kind: String,
}

/// The one event whose shape its `subtype` decides, which its drift name carries too.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SystemSubtypeProbe {
    System {
        subtype: String,
    },
    #[serde(other)]
    Other,
}

/// What the drift count calls a line it cannot read: its `type`.
fn unparsable_drift_label(value: &Value) -> String {
    match EventTypeProbe::deserialize(value) {
        Ok(EventTypeProbe { kind }) => format!("{kind}.<unparsable>"),
        Err(_) => MALFORMED_LINE.to_owned(),
    }
}

/// What the drift count calls a line, or a content block: its `type`, and a `system` event's
/// `subtype`.
fn drift_label(value: &Value) -> String {
    match (
        EventTypeProbe::deserialize(value),
        SystemSubtypeProbe::deserialize(value),
    ) {
        (Ok(EventTypeProbe { kind }), Ok(SystemSubtypeProbe::System { subtype })) => {
            format!("{kind}.{subtype}")
        }
        (Ok(EventTypeProbe { kind }), _) => kind,
        (Err(_), _) => MALFORMED_LINE.to_owned(),
    }
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
/// The blocks are kept as JSON values and read one by one, so a block in a shape the parser does
/// not know is counted without taking the message's prose with it.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Value>),
}

impl Default for Content {
    fn default() -> Self {
        Self::Blocks(Vec::new())
    }
}

/// One content block, by its `type`.
///
/// The blocks besides `text` carry no prose and are dropped on purpose, but named, so that a
/// block type outside this list is counted rather than filtered.
/// A renamed text block — `output_text` in place of `text`, say — would otherwise parse cleanly,
/// be dropped by the filter, and take the report with it without a single diagnostic.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    /// Prose; a text block without its text is not an empty message, and is counted.
    Text {
        #[serde(default)]
        text: Option<String>,
    },
    /// A tool call, which means the turn was not over when the message was sent.
    ToolUse {},
    ToolResult {},
    Thinking {},
    Image {},
    #[serde(other)]
    Unknown,
}

/// What one message's content amounts to.
#[derive(Debug, Default)]
struct ContentSummary {
    /// The prose, in order.
    texts: Vec<String>,
    /// Block shapes the parser does not account for, for the drift counter.
    unrecognised_block_labels: Vec<String>,
    /// Whether the message called a tool, which means the turn was not over when it was sent.
    calls_a_tool: bool,
}

impl Content {
    /// The prose of a message, plus any block shape the parser does not account for.
    fn inspect(self) -> ContentSummary {
        match self {
            Self::Text(text) => ContentSummary {
                texts: vec![text],
                ..ContentSummary::default()
            },
            Self::Blocks(blocks) => {
                let mut content_summary = ContentSummary::default();
                for block in blocks {
                    match ContentBlock::deserialize(&block) {
                        Ok(ContentBlock::Text { text: Some(text) }) => {
                            content_summary.texts.push(text);
                        }
                        // The payload field renamed, and dropping it would drop the report.
                        Ok(ContentBlock::Text { text: None }) => {
                            content_summary
                                .unrecognised_block_labels
                                .push("text.<missing>".to_owned());
                        }
                        Ok(ContentBlock::ToolUse {}) => content_summary.calls_a_tool = true,
                        Ok(
                            ContentBlock::ToolResult {}
                            | ContentBlock::Thinking {}
                            | ContentBlock::Image {},
                        ) => {}
                        Ok(ContentBlock::Unknown) => content_summary
                            .unrecognised_block_labels
                            .push(drift_label(&block)),
                        Err(_) => content_summary
                            .unrecognised_block_labels
                            .push(unparsable_drift_label(&block)),
                    }
                }
                content_summary
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
    /// The session this turn belonged to.
    ///
    /// Read only when no `system`/`init` announced one, which is what a resume against a session
    /// the CLI no longer holds looks like: it fails before the init event and this is the sole
    /// remaining evidence of which conversation was meant.
    #[serde(default)]
    session_id: Option<String>,
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
    status: Option<RateLimitStatus>,
    /// Unix seconds at which the window reopens.
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<i64>,
    #[serde(default, rename = "rateLimitType")]
    rate_limit_type: Option<String>,
}

/// Whether the account may still send, as the CLI says it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RateLimitStatus {
    Allowed,
    /// The account is refused until the window reopens.
    Rejected,
    /// A status this parser does not name, such as a warning that the window is nearly used.
    #[serde(untagged)]
    Other(String),
}

/// Record what a `rate_limit_event` says about the account's usage window.
///
/// The window is kept whatever the status says, because a run that succeeded against a nearly
/// closed window still tells the caller what to expect from the next question.
/// Only a status other than `allowed` earns a line in the transcript.
fn fold_rate_limit(turn: &mut Turn, event: RateLimitEvent) {
    let info = event.rate_limit_info;

    if let Some(resets_at) = info.resets_at.and_then(DateTime::from_timestamp_secs) {
        turn.usage_window = Some(UsageWindow {
            resets_at,
            window: info.rate_limit_type.clone(),
            exhausted: matches!(info.status, Some(RateLimitStatus::Rejected)),
            used_percent: None,
        });
    }

    let status = match info.status {
        None | Some(RateLimitStatus::Allowed) => None,
        Some(RateLimitStatus::Rejected) => Some("rejected".to_owned()),
        Some(RateLimitStatus::Other(status)) => Some(status),
    };
    if let Some(status) = status {
        let mut note = format!("delegate rate limit status: {status}");
        if let Some(limit) = &turn.usage_window {
            // Only a refused window is closed until then; a warning's window merely resets.
            let when = if limit.exhausted { "reopens" } else { "resets" };
            match &limit.window {
                Some(window) => {
                    let _ = write!(note, "; the {window} window {when} at ");
                }
                None => {
                    let _ = write!(note, "; the window {when} at ");
                }
            }
            let _ = write!(note, "{}", limit.resets_at.to_rfc3339());
        }
        turn.messages.push(Message::synthetic(note));
    }
}

/// Fold one `system` event: the session it announces on `init`, the key it authenticated with,
/// and a subtype the parser does not name.
fn fold_system(
    event: SystemEvent,
    line: &Value,
    turn: &mut Turn,
    session: &mut Option<SessionRef>,
) {
    let init = match event {
        SystemEvent::Init(init) => init,
        SystemEvent::Unknown => {
            turn.unrecognised.record(drift_label(line));
            return;
        }
        SystemEvent::ThinkingTokens {}
        | SystemEvent::Notification {}
        | SystemEvent::HookStarted {}
        | SystemEvent::HookResponse {}
        | SystemEvent::TaskStarted {}
        | SystemEvent::TaskNotification {}
        | SystemEvent::TaskUpdated {}
        | SystemEvent::BackgroundTasksChanged {} => return,
    };
    if let Some(parsed) = init
        .session_id
        .as_deref()
        .and_then(|id| SessionRef::parse(id).ok())
    {
        *session = Some(parsed);
    }
    // Said in the transcript rather than refused, because by the time the stream says it the CLI
    // has already chosen; a reader comparing it with the turn's billing note sees whether that was
    // the account's own opt-in.
    if let Some(ApiKeySource::Named(source)) = init.api_key_source {
        turn.messages.push(Message::synthetic(format!(
            "the delegate authenticated with an API key (apiKeySource: {source}), so this turn is \
             charged at API prices rather than to a subscription"
        )));
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
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            turn.unrecognised.record(MALFORMED_LINE);
            continue;
        };
        // An event this parser names, in a shape it does not.
        let Ok(event) = Event::deserialize(&value) else {
            turn.unrecognised.record(unparsable_drift_label(&value));
            continue;
        };

        match event {
            Event::System(event) => fold_system(event, &value, &mut turn, &mut session),
            Event::Assistant(event) => {
                if event.parent_tool_use_id.is_some() {
                    continue;
                }
                // The first message that names one is kept: later messages in a turn repeat it,
                // and a subagent's model was skipped above with the rest of its conversation.
                if turn.resolved_model.is_none() {
                    turn.resolved_model.clone_from(&event.message.model);
                }
                let content_summary = event.message.content.inspect();
                for kind in content_summary.unrecognised_block_labels {
                    turn.unrecognised
                        .record(format!("assistant.content.{kind}"));
                }
                delegate = if content_summary.calls_a_tool {
                    DelegateActivity::CallingATool
                } else {
                    DelegateActivity::Finished
                };
                for text in content_summary.texts {
                    if text.trim().is_empty() {
                        continue;
                    }
                    turn.messages.push(Message::delegate(Role::Assistant, text));
                }
            }
            Event::User(event) => {
                if event.parent_tool_use_id.is_some() {
                    continue;
                }
                let content_summary = event.message.content.inspect();
                for kind in content_summary.unrecognised_block_labels {
                    turn.unrecognised.record(format!("user.content.{kind}"));
                }
                for text in content_summary.texts {
                    if text.trim().is_empty() {
                        continue;
                    }
                    turn.messages
                        .push(match classify_injection(&text, delegate) {
                            Injection::Reopening => {
                                reopened = true;
                                Message::hook_injection(text)
                            }
                            Injection::Context => Message::hook_context(text),
                            Injection::None => Message::delegate(Role::User, text),
                        });
                }
            }
            Event::ToolProgress {} => {}
            Event::RateLimitEvent(event) => fold_rate_limit(&mut turn, event),
            Event::Result(event) => {
                // A turn that failed before announcing a session still names one here, and
                // without it the fold reports a turn whose continuity cannot be checked — drift,
                // for a stream that in fact said which conversation it meant.
                // `init` stays authoritative: this only fills a gap it left.
                if session.is_none()
                    && let Some(id) = event.session_id.as_deref()
                    && let Ok(parsed) = SessionRef::parse(id)
                {
                    session = Some(parsed);
                }
                turn.outcome = outcome_of(&event, reopened);
                // Nothing may follow a terminal event.
                // Stopping here keeps the rendered transcript strictly append-only, which is what
                // `tail`'s byte cursor relies on.
                break;
            }
            Event::Unknown => turn.unrecognised.record(drift_label(&value)),
        }
    }

    Fold {
        turn,
        session,
        turn_started: false,
    }
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

/// The terminal reasons that say what kind of failure a turn was, as the CLI spells them.
///
/// Read from the text rather than in place of it, because the text is also the failure's detail
/// and a reason this does not name is still worth quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
enum TerminalReason {
    MaxTurns,
    TurnLimit,
    Refusal,
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
    let parsed_terminal_reason = reason.parse::<TerminalReason>().ok();
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

    let kind = match (parsed_terminal_reason, event.api_error_status) {
        (_, Some(404)) => FailureKind::ModelUnavailable,
        (_, Some(429)) => FailureKind::RateLimited,
        (_, Some(402)) => FailureKind::BudgetExceeded,
        (_, Some(529)) => FailureKind::Overloaded,
        (Some(TerminalReason::MaxTurns | TerminalReason::TurnLimit), _) => FailureKind::TurnLimit,
        (Some(TerminalReason::Refusal), _) => FailureKind::ContentFlagged,
        (None, _) => classify(&detail),
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
