//! Folds the agy CLI's `stream-json` event stream into a turn.
//!
//! Changes when the agy CLI changes its JSON.
//!
//! # What this parser knows that the CLI does not document
//!
//! - **A step's text arrives in chunks.**
//!   An `agent_response` step streams `text_delta` chunks while it is `ACTIVE`, and the `DONE`
//!   update carries the last one, so a step's text is the chunks joined, kept once the step is
//!   done.
//!   `result.response` repeats the same text and is never read as the answer, so nothing appears
//!   twice.
//! - **A denial does not fail the run.**
//!   A call that needed approval nobody could give is listed in `result.denied_actions` of a
//!   `SUCCESS` result, and a call a deny rule refused is a `tool` step ending `ERROR`.
//!   Either way the run exits 0, so a turn that then gave no answer is failed here.
//! - **An `error_message` step with no payload is routine.**
//!   agy writes them in healthy runs, so they mean nothing on their own.
//! - **Only an unknown model is classified.**
//!   agy reports every failure as `result.error` prose, and only its model rejection has been
//!   observed; anything else stays unclassified rather than guessed at, an overload included,
//!   which agentmux would act on by itself.

use serde::Deserialize;
use serde_json::Value;

use crate::delegate::SessionRef;
use crate::stream::{Fold, MALFORMED_LINE, TurnInput, complete_lines};
use crate::transcript::{FailureKind, Message, Outcome, Role, SpendReport, Turn, Usage};

/// One line of the stream, by its `event`.
///
/// Every variant is one that has been observed with the exact flags [`crate::delegate`] builds;
/// anything else reads as `Unknown` and is counted.
#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Init {
        #[serde(default)]
        conversation_id: Option<String>,
        init: Init,
    },
    StepUpdate {
        step_update: StepUpdate,
    },
    Result {
        result: ResultEvent,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct Init {
    /// The model agy runs, which need not be the identifier that was asked for.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StepUpdate {
    step_index: u64,
    state: StepState,
    step_type: StepType,
    #[serde(default)]
    text_delta: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_info: Option<ToolInfo>,
}

/// Where a step stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum StepState {
    Active,
    Done,
    /// The step failed: a tool call a deny rule refused, or one that broke.
    Error,
}

/// What a step is, by its `step_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StepType {
    UserInput,
    AgentResponse,
    Tool,
    SystemMessage,
    ErrorMessage,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ToolInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    error: Option<ToolError>,
}

#[derive(Debug, Deserialize)]
struct ToolError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResultEvent {
    /// The session this turn belonged to, empty when agy refused the run before opening one.
    #[serde(default)]
    conversation_id: Option<String>,
    /// `SUCCESS` for a run that finished; required, because a `result` without it is one whose
    /// shape changed, and reading that as a success would turn drift into a clean completion.
    status: String,
    #[serde(default)]
    response: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    usage: Option<AgyUsage>,
    #[serde(default)]
    denied_actions: Vec<DeniedAction>,
}

/// One call agy denied because it needed an approval nobody could give.
#[derive(Debug, Deserialize)]
struct DeniedAction {
    action: String,
    #[serde(default)]
    display_name: Option<String>,
}

/// agy's token figures, which count cache reads beside `input_tokens`, not inside it.
#[derive(Debug, Default, Deserialize)]
struct AgyUsage {
    #[serde(rename = "input_tokens")]
    input: Option<u64>,
    #[serde(rename = "output_tokens")]
    output: Option<u64>,
    #[serde(rename = "thinking_tokens")]
    thinking: Option<u64>,
    #[serde(rename = "cache_read_tokens")]
    cache_read: Option<u64>,
}

impl From<&AgyUsage> for Usage {
    fn from(usage: &AgyUsage) -> Self {
        Self {
            input_tokens: usage.input,
            cached_input_tokens: usage.cache_read,
            cache_write_input_tokens: None,
            output_tokens: usage.output,
            reasoning_output_tokens: usage.thinking,
        }
    }
}

/// Enough of a line to name it in the drift count.
#[derive(Debug, Deserialize)]
struct EventProbe {
    event: String,
    #[serde(default)]
    step_update: Option<StepTypeProbe>,
}

#[derive(Debug, Deserialize)]
struct StepTypeProbe {
    step_type: String,
}

/// What the drift count calls a line: its `event`, and a step's `step_type`.
fn drift_label(value: &Value) -> String {
    match EventProbe::deserialize(value) {
        Ok(EventProbe {
            event,
            step_update: Some(StepTypeProbe { step_type }),
        }) => format!("{event}.{step_type}"),
        Ok(EventProbe { event, .. }) => event,
        Err(_) => MALFORMED_LINE.to_owned(),
    }
}

/// What the drift count calls a line it cannot read: its `event`.
fn unparsable_drift_label(value: &Value) -> String {
    match EventProbe::deserialize(value) {
        Ok(EventProbe { event, .. }) => format!("{event}.<unparsable>"),
        Err(_) => MALFORMED_LINE.to_owned(),
    }
}

/// Classify the error agy reported for a run that did not finish.
///
/// Only its rejection of an unknown model has been observed, and that alone is classified.
pub(crate) fn classify(error: &str) -> FailureKind {
    if error.contains("invalid model selection") {
        FailureKind::ModelUnavailable
    } else {
        FailureKind::Unclassified
    }
}

/// The text of the `agent_response` step under way, collected until the step is done.
struct PendingText {
    step_index: u64,
    text: String,
}

/// Fold an agy `stream-json` stream into a turn.
#[must_use]
pub fn fold(index: u32, question: &str, events: &str) -> Fold {
    let mut turn = Turn::new(index, question);
    let mut session = None;
    let mut pending: Option<PendingText> = None;
    // What each failed tool call said, for a turn that then gave no answer.
    let mut failed_calls = Vec::new();

    for line in complete_lines(events) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            turn.unrecognised.record(MALFORMED_LINE);
            continue;
        };
        let Ok(event) = Event::deserialize(&value) else {
            turn.unrecognised.record(unparsable_drift_label(&value));
            continue;
        };
        match event {
            Event::Init {
                conversation_id,
                init,
            } => {
                if let Some(parsed) = conversation_id
                    .as_deref()
                    .and_then(|id| SessionRef::parse(id).ok())
                {
                    session = Some(parsed);
                }
                if turn.resolved_model.is_none() {
                    turn.resolved_model = init.model;
                }
            }
            Event::StepUpdate { step_update } => match step_update.step_type {
                StepType::AgentResponse => {
                    collect_text(&mut pending, &mut turn, step_update);
                }
                StepType::Tool if step_update.state == StepState::Error => {
                    let failure = describe_failed_call(&step_update);
                    turn.messages.push(Message::synthetic(failure.clone()));
                    failed_calls.push(failure);
                }
                // Tool activity is the delegate narrating its work: kept in the capture and, as
                // for the other vendors, not rendered.
                StepType::Tool
                | StepType::UserInput
                | StepType::SystemMessage
                | StepType::ErrorMessage => {}
                StepType::Unknown => turn.unrecognised.record(drift_label(&value)),
            },
            Event::Result { result } => {
                flush(&mut pending, &mut turn);
                if session.is_none()
                    && let Some(parsed) = result
                        .conversation_id
                        .as_deref()
                        .and_then(|id| SessionRef::parse(id).ok())
                {
                    session = Some(parsed);
                }
                turn.outcome = outcome_of(&result, &mut turn, &failed_calls);
                // Nothing may follow a terminal event, so the rendered transcript only grows at
                // its end.
                break;
            }
            Event::Unknown => turn.unrecognised.record(drift_label(&value)),
        }
    }

    Fold {
        turn,
        session,
        input: TurnInput::Pending,
        last_result: None,
        steers_taken: Vec::new(),
    }
}

/// Add one `agent_response` update's text to the step under way, and keep the step's text as a
/// message once it is done.
fn collect_text(pending: &mut Option<PendingText>, turn: &mut Turn, step: StepUpdate) {
    if pending
        .as_ref()
        .is_some_and(|under_way| under_way.step_index != step.step_index)
    {
        // A step that never said it was done still said what it said.
        flush(pending, turn);
    }
    let under_way = pending.get_or_insert_with(|| PendingText {
        step_index: step.step_index,
        text: String::new(),
    });
    if let Some(delta) = step.text_delta {
        under_way.text.push_str(&delta);
    }
    if step.state != StepState::Active {
        flush(pending, turn);
    }
}

/// Keep the text of the step under way, if it said anything.
fn flush(pending: &mut Option<PendingText>, turn: &mut Turn) {
    if let Some(PendingText { text, .. }) = pending.take()
        && !text.trim().is_empty()
    {
        turn.messages.push(Message::delegate(Role::Assistant, text));
    }
}

/// How a failed tool call reads in the transcript: the tool, and what agy said about it.
fn describe_failed_call(step: &StepUpdate) -> String {
    let info = step.tool_info.as_ref();
    let tool = info
        .and_then(|info| info.name.as_deref())
        .or(step.tool_name.as_deref())
        .unwrap_or("tool");
    let message = info
        .and_then(|info| info.error.as_ref())
        .and_then(|error| error.message.as_deref())
        .unwrap_or("no reason given");
    format!("the delegate's `{tool}` call failed: {message}")
}

/// Decide a turn's outcome from its `result`, noting the calls agy denied and keeping what the
/// turn spent.
///
/// A run that finished with no answer after a denial or a failed tool call is failed: it exits 0
/// and reports success, and a caller told so would take the silence for an answer.
fn outcome_of(result: &ResultEvent, turn: &mut Turn, failed_calls: &[String]) -> Outcome {
    let usage = result.usage.as_ref().map(Usage::from).unwrap_or_default();
    turn.spend_report = Some(SpendReport {
        usage,
        cost_usd: None,
        cli_version: None,
    });

    let denied: Vec<String> = result
        .denied_actions
        .iter()
        .map(
            |DeniedAction {
                 action,
                 display_name,
             }| match display_name {
                Some(name) => format!("{name} (`{action}`)"),
                None => format!("`{action}`"),
            },
        )
        .collect();
    if !denied.is_empty() {
        turn.messages.push(Message::synthetic(format!(
            "agy denied calls that needed an approval nobody can give a consultation: {}",
            denied.join(", ")
        )));
    }

    if result.status != "SUCCESS" {
        let detail = result
            .error
            .as_deref()
            .filter(|error| !error.trim().is_empty())
            .map_or_else(|| format!("agy reported {}", result.status), str::to_owned);
        return Outcome::Failed {
            kind: classify(&detail),
            detail,
        };
    }
    let answered = result
        .response
        .as_deref()
        .is_some_and(|response| !response.trim().is_empty());
    if !answered && !denied.is_empty() {
        return Outcome::Failed {
            kind: FailureKind::Unclassified,
            detail: format!(
                "the delegate gave no answer after agy denied {}",
                denied.join(", ")
            ),
        };
    }
    if !answered && !failed_calls.is_empty() {
        return Outcome::Failed {
            kind: FailureKind::Unclassified,
            detail: format!(
                "the delegate gave no answer after {}",
                failed_calls.join("; ")
            ),
        };
    }
    Outcome::Completed {
        usage,
        cost_usd: None,
    }
}
