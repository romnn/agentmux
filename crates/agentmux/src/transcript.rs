//! The vendor-neutral record of what a delegate said.
//!
//! Every historical loss in this problem space has the same shape: something returned the *last*
//! message where the *whole* transcript was required.
//! The types here make that unrepresentable.
//! No type has a field meaning "the answer"; [`Transcript::final_message`] derives one on demand
//! and nothing stores it.
//! [`Outcome::Failed`] sits *beside* [`Turn::messages`], so a failure cannot discard the work that
//! preceded it.
//!
//! Nothing in this module knows which vendor produced a message.
//! The `stream` module parses at the edge and everything downstream is neutral.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The delegate model.
    Assistant,
    /// A turn input: the question, or something injected into the conversation.
    User,
    /// The delegate CLI speaking for itself rather than for the model.
    System,
}

/// Where a message came from, which is not the same question as who it is attributed to.
///
/// A blocking `Stop` hook injects a synthetic `user` message and reopens a finished turn.
/// That message is attributed to the user by the vendor but was written by neither party, and a
/// reader who cannot tell the difference will mistake the reopened turn's output for the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSource {
    /// Genuinely part of the consultation: the question, or the model's reply.
    Delegate,
    /// Injected by a hook in the delegate's own environment, reopening a finished turn.
    HookInjection,
    /// Written by agentmux to record something the event stream implied but did not say.
    Synthetic,
}

/// One message in a consultation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Who the message is attributed to.
    pub role: Role,
    /// The message text, exactly as the delegate emitted it.
    pub text: String,
    /// How the message entered the conversation.
    pub source: MessageSource,
}

impl Message {
    /// A message the delegate genuinely produced.
    #[must_use]
    pub fn delegate(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            source: MessageSource::Delegate,
        }
    }

    /// A `user` message a hook injected to reopen a finished turn.
    #[must_use]
    pub fn hook_injection(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            source: MessageSource::HookInjection,
        }
    }

    /// A note agentmux wrote itself, such as a non-fatal warning the delegate reported.
    #[must_use]
    pub fn synthetic(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            text: text.into(),
            source: MessageSource::Synthetic,
        }
    }

    /// Whether this message counts as part of the deliverable.
    ///
    /// Assistant text the delegate produced is the report.
    /// Hook injections and agentmux's own notes are context around it.
    #[must_use]
    pub fn is_report_content(&self) -> bool {
        self.role == Role::Assistant && self.source == MessageSource::Delegate
    }
}

/// Token accounting for one turn, in the union of what both vendors report.
///
/// Every field is optional because neither vendor promises to report all of them, and a missing
/// count must stay distinguishable from a zero count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "the `_tokens` suffix is load-bearing: `input`/`output` alone would read as message \
              counts, which this struct sits next to"
)]
pub struct Usage {
    /// Prompt tokens billed at the full rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Prompt tokens served from the provider's cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Prompt tokens written into the provider's cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
    /// Tokens the model emitted, including reasoning where the vendor folds it in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Reasoning tokens, where the vendor reports them separately.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
}

/// Why a turn ended badly.
///
/// Exhaustive matching is the point: a new failure mode breaks the build at every site that has to
/// decide what to do about it.
/// [`FailureKind::Unclassified`] exists because the alternative is silently mapping an unfamiliar
/// vendor error onto a familiar one, which is the same class of quiet lie this crate is built to
/// prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The requested model does not exist, or this account cannot reach it.
    ModelUnavailable,
    /// A provider guardrail refused the request or blocked the closing message.
    ContentFlagged,
    /// The account is over a rate limit or usage window.
    RateLimited,
    /// The account is out of credit or over a spend cap.
    BudgetExceeded,
    /// The delegate hit its own maximum number of turns.
    TurnLimit,
    /// The child process could not be started, or died before saying anything.
    LaunchFailed,
    /// The delegate reported an error agentmux does not recognise.
    /// `detail` carries it verbatim.
    Unclassified,
}

impl FailureKind {
    /// A short human-facing label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ModelUnavailable => "model unavailable",
            Self::ContentFlagged => "content flagged",
            Self::RateLimited => "rate limited",
            Self::BudgetExceeded => "budget exceeded",
            Self::TurnLimit => "turn limit",
            Self::LaunchFailed => "launch failed",
            Self::Unclassified => "error",
        }
    }
}

/// How a turn ended.
///
/// `Failed` carries only the reason.
/// The messages collected before the failure stay in [`Turn::messages`], so a codex run whose
/// closing message was blocked after two hours of analysis recovers by reading the transcript
/// rather than by hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Outcome {
    /// The delegate has not finished, or agentmux has not yet seen it finish.
    Running,
    /// The delegate finished its turn.
    Completed {
        /// Token accounting, empty if the vendor reported none.
        usage: Usage,
        /// Dollar cost, where the vendor reports it.
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    /// The turn ended badly.
    /// The messages collected so far are still in the turn.
    Failed {
        /// What went wrong, classified.
        kind: FailureKind,
        /// The vendor's own words, verbatim.
        detail: String,
    },
    /// A caller stopped the consultation.
    Cancelled,
}

impl Outcome {
    /// Whether no further messages can arrive for this turn.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }

    /// A one-word state name, stable enough for a caller to branch on.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Event types the parser met but does not model, counted rather than skipped.
///
/// A parser that silently ignores what it does not recognise will one day silently ignore the
/// report.
/// Counting turns schema drift into something loud: the count appears in `status` and in `result`,
/// and a fixture test fails the build when a recorded stream contains a top-level event type the
/// parser does not name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnrecognisedEvents(BTreeMap<String, u32>);

impl UnrecognisedEvents {
    /// Record one sighting of an event type the parser does not model.
    pub fn record(&mut self, kind: impl Into<String>) {
        *self.0.entry(kind.into()).or_insert(0) += 1;
    }

    /// Total sightings across all unrecognised types.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.0.values().copied().sum()
    }

    /// Whether every event in the stream was understood.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The counted types, ascending by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u32)> {
        self.0.iter().map(|(kind, count)| (kind.as_str(), *count))
    }

    /// Fold another tally into this one.
    pub fn merge(&mut self, other: &Self) {
        for (kind, count) in other.iter() {
            *self.0.entry(kind.to_owned()).or_insert(0) += count;
        }
    }

    /// A compact `type xN, type xM` summary, or `None` when nothing was unrecognised.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = String::new();
        for (index, (kind, count)) in self.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{kind} x{count}");
        }
        Some(out)
    }
}

/// One question put to the delegate and everything that came back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// Position in the consultation, from zero.
    pub index: u32,
    /// The question as asked.
    pub question: String,
    /// Everything the delegate said, in order, including hook injections.
    pub messages: Vec<Message>,
    /// How the turn ended.
    pub outcome: Outcome,
    /// Event types the parser met but does not model.
    pub unrecognised: UnrecognisedEvents,
    /// Whether this turn was meant to continue an earlier session but did not.
    ///
    /// Both CLIs accept a resume against a session they no longer hold, and answer from an empty
    /// context instead of failing.
    /// The delegate then replies confidently with no memory of the brief, which reads as a
    /// continuation and is not one.
    pub broke_continuity: bool,
    /// A reply recovered from outside the event stream, once the turn was over.
    ///
    /// This is the one place a field can hold the whole answer, and it does not contradict the
    /// rule that no type stores "the answer".
    /// It is only ever set when the turn is terminal *and* the stream carried no reply at all, so
    /// it never sits beside a transcript it could be read instead of — it is what there is.
    ///
    /// Deliberately not a [`Message`] in [`Turn::messages`].
    /// Messages render above the outcome footer and the drift note, and this content becomes
    /// available strictly after the terminal event — measured at 228 ms after it, for Codex.
    /// Anything rendered before those would insert bytes above text already handed to a caller as
    /// a cursor, so this renders last, after everything whose content is final at the terminal
    /// event.
    pub recovered: Option<String>,
}

impl Turn {
    /// An empty turn that has not produced anything yet.
    #[must_use]
    pub fn new(index: u32, question: impl Into<String>) -> Self {
        Self {
            index,
            question: question.into(),
            messages: Vec::new(),
            outcome: Outcome::Running,
            unrecognised: UnrecognisedEvents::default(),
            broke_continuity: false,
            recovered: None,
        }
    }

    /// Whether a hook reopened this turn after the delegate had finished speaking.
    #[must_use]
    pub fn was_reopened_by_hook(&self) -> bool {
        self.messages
            .iter()
            .any(|message| message.source == MessageSource::HookInjection)
    }
}

/// A consultation: every question put to one delegate, and everything it said back.
///
/// This is the deliverable.
/// Everything else in the crate is metadata about how it was obtained.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// The turns, in order.
    pub turns: Vec<Turn>,
}

impl Transcript {
    /// The text of the delegate's last substantive reply.
    ///
    /// Derived on every call and stored nowhere, so it can never be recorded *instead of* the
    /// transcript.
    /// It is a convenience for a caller who already has the whole thing, never the thing itself:
    /// when a hook reopened the turn, the last message is the reopened turn's output and the
    /// report is further up.
    /// [`Transcript::was_reopened_by_hook`] says when to distrust it.
    ///
    /// Falls back to [`Turn::recovered`], so a run whose reply was recovered from outside the
    /// event stream does not report a visibly non-empty transcript and no final message.
    #[must_use]
    pub fn final_message(&self) -> Option<&str> {
        self.turns.iter().rev().find_map(|turn| {
            turn.messages
                .iter()
                .rev()
                .find(|message| message.is_report_content())
                .map(|message| message.text.as_str())
                .or(turn.recovered.as_deref())
        })
    }

    /// The first turn that failed, when it is not the turn that decides [`Self::outcome`].
    ///
    /// [`Self::outcome`] reports the newest turn, which is what a caller polling for "is it done"
    /// needs.
    /// On its own that lets a follow-up hide a failure: turn one is refused, turn two succeeds,
    /// and the consultation reports `completed` with the refusal buried in the body.
    /// Callers report this alongside the outcome so the earlier failure stays visible.
    #[must_use]
    pub fn earlier_failure(&self) -> Option<(u32, FailureKind)> {
        let last = self.turns.len().saturating_sub(1);
        self.turns
            .iter()
            .take(last)
            .find_map(|turn| match &turn.outcome {
                Outcome::Failed { kind, .. } => Some((turn.index, *kind)),
                _ => None,
            })
    }

    /// Whether the delegate ever said anything, across every turn.
    ///
    /// This is the evidence that there is a conversation worth continuing, and it is deliberately
    /// evidence rather than a rule about failure categories.
    /// A category cannot tell you *when* a failure happened: a rate limit that interrupts a
    /// follow-up says nothing about whether the first turn's session still exists, and refusing
    /// on the category alone would lock a caller out of a consultation that is merely paused.
    #[must_use]
    pub fn has_delegate_content(&self) -> bool {
        self.messages().any(Message::is_report_content)
            || self.turns.iter().any(|turn| turn.recovered.is_some())
    }

    /// Whether any turn was reopened by a hook, which makes [`Self::final_message`] untrustworthy.
    #[must_use]
    pub fn was_reopened_by_hook(&self) -> bool {
        self.turns.iter().any(Turn::was_reopened_by_hook)
    }

    /// How the consultation currently stands: the last turn's outcome.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        self.turns
            .last()
            .map_or(Outcome::Running, |turn| turn.outcome.clone())
    }

    /// Every message across every turn, in order.
    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.turns.iter().flat_map(|turn| turn.messages.iter())
    }

    /// Unrecognised event types across the whole consultation.
    #[must_use]
    pub fn unrecognised(&self) -> UnrecognisedEvents {
        let mut merged = UnrecognisedEvents::default();
        for turn in &self.turns {
            merged.merge(&turn.unrecognised);
        }
        merged
    }

    /// Total token accounting across every turn that reported any.
    #[must_use]
    pub fn usage(&self) -> Usage {
        fn add(total: &mut Option<u64>, part: Option<u64>) {
            if let Some(part) = part {
                *total = Some(total.unwrap_or(0).saturating_add(part));
            }
        }
        let mut total = Usage::default();
        for turn in &self.turns {
            if let Outcome::Completed { usage, .. } = &turn.outcome {
                add(&mut total.input_tokens, usage.input_tokens);
                add(&mut total.cached_input_tokens, usage.cached_input_tokens);
                add(
                    &mut total.cache_write_input_tokens,
                    usage.cache_write_input_tokens,
                );
                add(&mut total.output_tokens, usage.output_tokens);
                add(
                    &mut total.reasoning_output_tokens,
                    usage.reasoning_output_tokens,
                );
            }
        }
        total
    }

    /// Total reported dollar cost, or `None` when no turn reported one.
    #[must_use]
    pub fn cost_usd(&self) -> Option<f64> {
        let mut total: Option<f64> = None;
        for turn in &self.turns {
            if let Outcome::Completed {
                cost_usd: Some(cost),
                ..
            } = &turn.outcome
            {
                total = Some(total.unwrap_or(0.0) + cost);
            }
        }
        total
    }
}

/// Render one turn as Markdown.
///
/// The result is **append-only with respect to the event prefix it was folded from**: appending
/// events to the stream can only append bytes here, never rewrite earlier ones.
/// `result` pages through a rendered transcript by byte offset and relies on that; the
/// running-turn footer is therefore written only once the outcome is terminal.
#[must_use]
pub fn render_turn(turn: &Turn) -> String {
    let mut out = String::new();
    let number = turn.index.saturating_add(1);

    let _ = write!(
        out,
        "\n## Turn {number} — question\n\n{}\n",
        turn.question.trim_end()
    );

    let mut reopened = false;
    for message in &turn.messages {
        match message.source {
            MessageSource::HookInjection => {
                reopened = true;
                let _ = write!(
                    out,
                    "\n> [!WARNING]\n\
                     > **A hook in the delegate's environment reopened this finished turn.**\n\
                     > Everything below is the reopened turn, not the answer. The answer is above.\n\
                     >\n\
                     > Injected text: {}\n",
                    single_line(&message.text)
                );
            }
            MessageSource::Synthetic => {
                let _ = write!(out, "\n> [!NOTE]\n> {}\n", single_line(&message.text));
            }
            MessageSource::Delegate => {
                let label = match (message.role, reopened) {
                    (Role::Assistant, false) => "assistant",
                    (Role::Assistant, true) => "assistant (after hook reopen — not the answer)",
                    (Role::User, _) => "user",
                    (Role::System, _) => "system",
                };
                let _ = write!(out, "\n### {label}\n\n{}\n", message.text.trim_end());
            }
        }
    }

    // Everything below is written only once the outcome is terminal.
    // While a turn is running the render must be able to grow only at its end, because `tail`
    // pages it by byte offset; a footer or a drift note that appeared early and then moved would
    // corrupt every cursor.
    if !turn.outcome.is_terminal() {
        return out;
    }

    match &turn.outcome {
        Outcome::Completed { usage, cost_usd } => {
            let _ = write!(
                out,
                "\n---\n\n_Turn {number} completed{}_\n",
                accounting(usage, *cost_usd)
            );
        }
        Outcome::Failed { kind, detail } => {
            let _ = write!(
                out,
                "\n---\n\n_Turn {number} failed: {} — {}_\n\n\
                 Anything the delegate said before the failure is above and is not affected by it.\n",
                kind.label(),
                single_line(detail),
            );
        }
        Outcome::Cancelled => {
            let _ = write!(
                out,
                "\n---\n\n_Turn {number} cancelled. Everything collected before the cancellation is above._\n",
            );
        }
        // Unreachable: the guard above returns for a running turn.
        Outcome::Running => {}
    }

    if let Some(summary) = turn.unrecognised.summary() {
        let _ = write!(
            out,
            "\n> [!NOTE]\n\
             > agentmux did not recognise {} event(s) in this turn's stream: {summary}.\n\
             > The delegate CLI may have changed its output format. The raw stream is kept.\n",
            turn.unrecognised.total(),
        );
    }

    if let Some(recovered) = &turn.recovered {
        let _ = write!(
            out,
            "\n> [!NOTE]\n\
             > The delegate's event stream carried no reply, so agentmux recovered the text below\n\
             > from the file the CLI writes directly.\n\
             > Anything the delegate said earlier in the turn is not in that file.\n\
             \n### assistant (recovered)\n\n{}\n",
            recovered.trim_end(),
        );
    }

    out
}

/// Collapse newlines so a quoted blockquote stays one blockquote.
fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn accounting(usage: &Usage, cost_usd: Option<f64>) -> String {
    let mut parts = Vec::new();
    if let Some(input) = usage.input_tokens {
        parts.push(format!("{input} in"));
    }
    if let Some(output) = usage.output_tokens {
        parts.push(format!("{output} out"));
    }
    // Zero reasoning tokens is the common case and saying so every time is noise.
    if let Some(reasoning) = usage.reasoning_output_tokens.filter(|count| *count > 0) {
        parts.push(format!("{reasoning} reasoning"));
    }
    if let Some(cost) = cost_usd {
        parts.push(format!("${cost:.4}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" — {}", parts.join(", "))
    }
}
