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
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What the vendor reported about the account's usage window as a turn ran.
///
/// A rate limit is the one failure whose right response depends on a number the failure itself does
/// not contain: a window reopening in ten minutes is worth waiting for, and one reopening in nine
/// hours is not.
/// Claude reports the window in its stream and Codex in the session rollout it writes, and neither
/// puts it in the error message, so it is captured here rather than left for a caller to parse back
/// out of prose.
///
/// It is carried for a turn that was never refused too, because a window nearly used up tells the
/// caller what to expect from the next question; [`UsageWindow::exhausted`] says which it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// When the window resets, which for an exhausted window is when it reopens.
    pub resets_at: DateTime<Utc>,
    /// The vendor's own name for the window, such as `seven_day_overage_included`.
    ///
    /// Quoted rather than parsed into a closed set: these names are account-plan specific and new
    /// ones appear without notice, and the name is only ever shown to a reader.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Whether the window was refusing requests when the vendor reported it.
    ///
    /// Only then is it closed until [`UsageWindow::resets_at`]; otherwise it merely resets then.
    pub exhausted: bool,
    /// How much of the window had been used, from 0 to 100, where the vendor reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
}

impl UsageWindow {
    /// How long until the window resets, or `None` once it already has.
    #[must_use]
    pub fn resets_in(&self, now: DateTime<Utc>) -> Option<std::time::Duration> {
        (self.resets_at - now).to_std().ok()
    }
}

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
    /// Injected by a hook in the delegate's own environment while the turn was still under way.
    ///
    /// A `PostToolUse` hook adding context, or a `PreToolUse` hook refusing a call with a reason,
    /// arrives with the same shape as a reopening: a `user` text message the user did not write.
    /// It differs in what it means for the reader — the delegate's later messages are still the
    /// answer — so it is kept apart rather than collapsed into [`Self::HookInjection`].
    HookContext,
    /// Sent by the caller into a turn that was still running, through `steer`.
    ///
    /// A genuine user instruction, unlike a hook's, but not the question the turn opened with:
    /// it arrived part-way through, so what the delegate said above it was said without it.
    Steer,
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

    /// A `user` message a hook injected while the delegate was still working.
    #[must_use]
    pub fn hook_context(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            source: MessageSource::HookContext,
        }
    }

    /// A `user` instruction the caller steered into the running turn.
    #[must_use]
    pub fn steer(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            source: MessageSource::Steer,
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

/// Token figures for one turn, exactly as the vendor reported them: in the vendor's own
/// categories and over the vendor's own scope.
///
/// They are evidence, not what the turn spent.
/// Codex reports its thread's running total, so a follow-up's figures include every earlier turn
/// of the session, while Claude reports the one call.
/// Codex counts cache reads inside `input_tokens`, and Claude counts them beside it.
/// [`crate::accounting`] derives what each turn spent from them; the turn's footer quotes them as
/// they are, because a published footer never changes.
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
    /// Prompt tokens.
    ///
    /// Codex: every prompt token, those read from the cache included.
    /// Claude: only the prompt tokens the cache played no part in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Prompt tokens read from the provider's cache.
    ///
    /// Codex: a part of `input_tokens`.
    /// Claude: its `cache_read_input_tokens`, counted beside `input_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Prompt tokens written into the provider's cache.
    ///
    /// Codex: read as a part of `input_tokens`, as its cache reads are.
    /// Claude: its `cache_creation_input_tokens`, counted beside `input_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
    /// Tokens the model emitted, reasoning included for both vendors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Reasoning tokens, a part of `output_tokens`.
    ///
    /// Claude: its `output_tokens_details.thinking_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
}

/// What a vendor reported spending in a turn's terminal result, whether the turn succeeded or
/// failed.
///
/// Kept beside the outcome rather than in it: a failed turn spent tokens too, and a Claude error
/// result reports them, but [`Outcome::Failed`] is published in `settled.json` and has no room for
/// them.
/// Neither serialised nor rendered; [`crate::accounting`] is its only reader.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpendReport {
    /// The token figures, in the vendor's own categories and scope.
    pub usage: Usage,
    /// The dollar figure, in the vendor's own scope, where it reported one.
    pub cost_usd: Option<f64>,
    /// The delegate CLI release that reported them, as it names itself, where that is known.
    ///
    /// Which release it was decides what scope Claude's dollar figure covers.
    pub cli_version: Option<String>,
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
    /// The vendor's model is at capacity or overloaded for everyone, for the moment.
    ///
    /// Transient and nothing to do with the account: the session is intact, and the same model
    /// usually answers again within seconds.
    Overloaded,
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
            Self::Overloaded => "model overloaded",
            Self::TurnLimit => "turn limit",
            Self::LaunchFailed => "launch failed",
            Self::Unclassified => "error",
        }
    }
}

/// A turn before the newest that failed.
///
/// The turn is read only as the one-based number its heading shows, so no reader can print the
/// zero-based index by mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EarlierFailure {
    #[serde(rename = "turn", serialize_with = "turn_number")]
    turn_index: u32,
    kind: FailureKind,
}

impl EarlierFailure {
    /// The turn, counted from 1 as the transcript's turn headings count.
    #[must_use]
    pub fn turn_number(self) -> u32 {
        self.turn_index.saturating_add(1)
    }

    /// What kind of failure.
    #[must_use]
    pub fn kind(self) -> FailureKind {
        self.kind
    }
}

/// A zero-based turn index, written as the one-based number the transcript's headings show.
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's `serialize_with` hands the field over by reference"
)]
pub(crate) fn turn_number<S: serde::Serializer>(
    index: &u32,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u32(index.saturating_add(1))
}

/// A one-based turn number as it is written, read back as the zero-based turn index.
pub(crate) fn turn_index_of_number<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u32, D::Error> {
    let number = u32::deserialize(deserializer)?;
    number
        .checked_sub(1)
        .ok_or_else(|| serde::de::Error::custom("turns are numbered from 1"))
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
        /// The vendor's token figures, as [`Usage`] describes them, empty if it reported none.
        usage: Usage,
        /// The vendor's dollar figure, in its own scope, where it reports one.
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
        match self {
            Self::Running => false,
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled => true,
        }
    }

    /// Where the turn stands, without what the outcome carries.
    #[must_use]
    pub fn state(&self) -> TurnState {
        match self {
            Self::Running => TurnState::Running,
            Self::Completed { .. } => TurnState::Completed,
            Self::Failed { .. } => TurnState::Failed,
            Self::Cancelled => TurnState::Cancelled,
        }
    }
}

/// Where a turn stands: an [`Outcome`] without what it carries.
///
/// Displayed as the one word the serialised outcome's `state` tag holds, stable enough for a
/// caller to branch on.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display, strum::VariantArray,
)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TurnState {
    /// See [`Outcome::Running`].
    Running,
    /// See [`Outcome::Completed`].
    Completed,
    /// See [`Outcome::Failed`].
    Failed,
    /// See [`Outcome::Cancelled`].
    Cancelled,
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
        let count = self.0.entry(kind.into()).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// Total sightings across all unrecognised types.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.0
            .values()
            .fold(0, |sum, count| sum.saturating_add(*count))
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
            let total = self.0.entry(kind.to_owned()).or_insert(0);
            *total = total.saturating_add(count);
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
    /// Whether codex sent app-server messages the generated protocol could not read, which
    /// running the consultation over `codex exec` gets round.
    #[serde(
        rename = "undecoded_messages",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub has_undecoded_app_server_messages: bool,
    /// What the vendor reported about the account's usage window, when it reported anything.
    ///
    /// Set whenever the stream carries the information, not only on a rate-limited failure: a run
    /// that succeeded against a nearly exhausted window is worth knowing about before the next
    /// question is asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_window: Option<UsageWindow>,
    /// The model identifier the vendor itself reported using, when it reported one.
    ///
    /// A requested identifier can be an alias the vendor expands — `sonnet` arrives as
    /// `claude-sonnet-5` — so the two are kept apart rather than one overwriting the other.
    /// agentmux still keeps no roster: this is only what the stream said, and a vendor that says
    /// nothing leaves it empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
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
    /// Where the delegate CLI saves its sessions, when it could not write there as this turn was
    /// launched, so the session this turn ran in was never kept.
    ///
    /// Known before the child is spawned — it is the launcher's own confinement, inherited — so
    /// it renders directly under the question, ahead of anything the stream adds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsaved_session_store: Option<PathBuf>,
    /// The pricing this turn's delegate was launched on, or `None` for a turn recorded before
    /// agentmux kept it.
    ///
    /// Decided before the child is spawned, so like [`Self::unsaved_session_store`] it renders
    /// under the question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<crate::delegate::Billing>,
    /// Whether this turn's delegate loaded the user's configuration, or `None` for a turn
    /// recorded before agentmux kept it per turn, which ran as the consultation's record says.
    ///
    /// A follow-up may change it, so what a hook reopening a turn means is judged by the turn's
    /// own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<crate::delegate::Isolation>,
    /// The delegate this turn ran as, when a follow-up changed its isolation or permissions from
    /// what the transcript header names.
    ///
    /// Recorded before the child is spawned, so like [`Self::billing`] it renders under the
    /// question, and the header above every turn never changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran_as: Option<String>,
    /// Which automatic resume this turn is, when agentmux rather than the caller asked its
    /// question, after the model was overloaded.
    ///
    /// Recorded before the child is spawned, so like [`Self::billing`] it renders under the
    /// question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_resume: Option<ResumeAttempt>,
    /// The label the caller gave this turn, when it gave one.
    ///
    /// Recorded before the child is spawned, so like [`Self::billing`] it renders under the
    /// question, which makes the transcript say where a consultation's purpose changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<crate::run::RunLabel>,
    /// What the vendor reported spending in this turn's terminal result, or `None` before the
    /// turn ended and for a turn whose stream carried no terminal result.
    #[serde(skip)]
    pub spend_report: Option<SpendReport>,
}

/// Which automatic resume a turn is, in an unbroken run of turns that failed because the model
/// was overloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeAttempt {
    /// Which automatic resume in a row it is, counted from 1.
    pub attempt: u32,
    /// How many agentmux makes in a row before it stops.
    pub attempts: u32,
}

impl Turn {
    /// Whether the delegate said anything in this turn: a reply in the stream, or one recovered
    /// from the CLI's own output file.
    #[must_use]
    pub fn has_delegate_content(&self) -> bool {
        self.messages.iter().any(Message::is_report_content) || self.recovered.is_some()
    }

    /// An empty turn that has not produced anything yet.
    #[must_use]
    pub fn new(index: u32, question: impl Into<String>) -> Self {
        Self {
            index,
            question: question.into(),
            messages: Vec::new(),
            outcome: Outcome::Running,
            unrecognised: UnrecognisedEvents::default(),
            has_undecoded_app_server_messages: false,
            usage_window: None,
            resolved_model: None,
            broke_continuity: false,
            recovered: None,
            unsaved_session_store: None,
            billing: None,
            isolation: None,
            ran_as: None,
            auto_resume: None,
            label: None,
            spend_report: None,
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

    /// Every turn before the newest that failed, oldest first.
    ///
    /// [`Self::outcome`] reports the newest turn, which is what a caller polling for "is it done"
    /// needs.
    /// On its own that lets a follow-up hide a failure: turn one is refused, turn two succeeds,
    /// and the consultation reports `completed` with the refusal buried in the body.
    /// Callers report these beside the outcome, never inside it, so the earlier failures stay
    /// visible without reading as the state of the turn that is running now.
    #[must_use]
    pub fn earlier_failures(&self) -> Vec<EarlierFailure> {
        let last = self.turns.len().saturating_sub(1);
        self.turns
            .iter()
            .take(last)
            .filter_map(|turn| match &turn.outcome {
                Outcome::Failed { kind, .. } => Some(EarlierFailure {
                    turn_index: turn.index,
                    kind: *kind,
                }),
                _ => None,
            })
            .collect()
    }

    /// How many of the newest turns in a row a content filter refused.
    #[must_use]
    pub fn content_flagged_in_a_row(&self) -> u32 {
        let refused = self
            .turns
            .iter()
            .rev()
            .take_while(|turn| {
                matches!(
                    turn.outcome,
                    Outcome::Failed {
                        kind: FailureKind::ContentFlagged,
                        ..
                    }
                )
            })
            .count();
        u32::try_from(refused).unwrap_or(u32::MAX)
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
        self.turns.iter().any(Turn::has_delegate_content)
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

    /// The model the vendor last reported running, or `None` when no turn reported one.
    ///
    /// The newest turn wins, because a follow-up may be answered by a different model than the
    /// turn before it and the caller is asking about the answer just received.
    #[must_use]
    pub fn resolved_model(&self) -> Option<&str> {
        self.turns
            .iter()
            .rev()
            .find_map(|turn| turn.resolved_model.as_deref())
    }
}

/// One sentence saying that a session could not be saved, and where.
///
/// The one phrasing of it, so the transcript, every status and the follow-up refusal say the same
/// thing. Only the fact: a front end that has room adds why it happens and what to do instead.
#[must_use]
pub fn describe_unsaved_session_store(store: &Path) -> String {
    format!(
        "agentmux could not write to `{}`, where the delegate CLI saves its sessions, so the \
         delegate could not save this session and no follow-up can resume it",
        store.display()
    )
}

/// The heading a turn opens with, and everything known about the turn before its child ran.
///
/// Only what was fixed at the launch may go here: this is rendered under the first fold of the
/// turn, and anything added later would move every byte below it.
fn render_question(turn: &Turn) -> String {
    let mut out = indoc::formatdoc!(
        "

            ## Turn {} — question

            {}
        ",
        turn.index.saturating_add(1),
        turn.question.trim_end()
    );
    if let Some(label) = &turn.label {
        let _ = indoc::writedoc!(
            out,
            "

            > **Label:** {label}
        "
        );
    }
    if let Some(ResumeAttempt { attempt, attempts }) = turn.auto_resume {
        let _ = indoc::writedoc!(
            out,
            "

            > [!NOTE]
            > **agentmux sent this question itself** after the model was overloaded, not the \
            caller: automatic resume {attempt} of {attempts}.
        "
        );
    }
    if let Some(store) = &turn.unsaved_session_store {
        let _ = indoc::writedoc!(
            out,
            "

            > [!WARNING]
            > **This turn's session was not saved.** {}.
        ",
            describe_unsaved_session_store(store)
        );
    }
    // Only the exception is said: a note on every subscription turn would teach a reader to skip
    // the one that is not.
    if turn.billing == Some(crate::delegate::Billing::Api) {
        out.push_str(indoc::indoc! {"

                > [!NOTE]
                > **This turn may be billed at API prices.** Its account opts into \
                `billing = \"api\"`, so the key, token or endpoint it names reached the delegate.
            "});
    }
    if let Some(ran_as) = &turn.ran_as {
        let _ = indoc::writedoc!(
            out,
            "

            > [!NOTE]
            > **This turn ran with other settings than the header names:** `{ran_as}`.
        "
        );
    }
    out
}

/// Render one turn as Markdown.
///
/// The result is **append-only with respect to the event prefix it was folded from**: appending
/// events to the stream can only append bytes here, never rewrite earlier ones.
/// `result` pages through a rendered transcript by byte offset and relies on that; the
/// running-turn footer is therefore written only once the outcome is terminal.
#[must_use]
pub fn render_turn(turn: &Turn) -> String {
    let mut out = render_question(turn);
    let number = turn.index.saturating_add(1);

    render_messages(&mut out, &turn.messages);

    // Everything below is written only once the outcome is terminal.
    // While a turn is running the render must be able to grow only at its end, because `tail`
    // pages it by byte offset; a footer or a drift note that appeared early and then moved would
    // corrupt every cursor.
    if !turn.outcome.is_terminal() {
        return out;
    }

    match &turn.outcome {
        Outcome::Completed { usage, cost_usd } => {
            let _ = indoc::writedoc!(
                out,
                "

                ---

                _Turn {number} completed{}_
            ",
                accounting(usage, *cost_usd),
            );
        }
        Outcome::Failed { kind, detail } => {
            let _ = indoc::writedoc!(
                out,
                "

                ---

                _Turn {number} failed: {} — {}_

                Anything the delegate said before the failure is above and is not affected by it.
            ",
                kind.label(),
                single_line(detail),
            );
        }
        Outcome::Cancelled => {
            let _ = indoc::writedoc!(
                out,
                "

                ---

                _Turn {number} cancelled. Everything collected before the cancellation is above._
            ",
            );
        }
        // Unreachable: the guard above returns for a running turn.
        Outcome::Running => {}
    }

    if let Some(summary) = turn.unrecognised.summary() {
        let _ = indoc::writedoc!(
            out,
            "

            > [!NOTE]
            > agentmux did not recognise {} event(s) in this turn's stream: {summary}.
            > The delegate CLI may have changed its output format. The raw stream is kept.
        ",
            turn.unrecognised.total(),
        );
    }

    if let Some(recovered) = &turn.recovered {
        let _ = indoc::writedoc!(
            out,
            "

            > [!NOTE]
            > The delegate's event stream carried no reply, so agentmux recovered the text below
            > from the file the CLI writes directly.
            > Anything the delegate said earlier in the turn is not in that file.

            ### assistant (recovered)

            {}
        ",
            recovered.trim_end(),
        );
    }

    out
}

fn render_messages(rendered_turn: &mut String, messages: &[Message]) {
    let mut reopened = false;
    for message in messages {
        match message.source {
            MessageSource::HookInjection => {
                reopened = true;
                let _ = indoc::writedoc!(
                    rendered_turn,
                    "

                    > [!WARNING]
                    > **A hook in the delegate's environment reopened this finished turn.**
                    > Everything below is the reopened turn, not the answer. The answer is above.
                    >
                    > Injected text: {}
                ",
                    single_line(&message.text)
                );
            }
            MessageSource::HookContext => {
                let _ = indoc::writedoc!(
                    rendered_turn,
                    "

                    > [!NOTE]
                    > A hook in the delegate's environment added context here, mid-turn. The
                    > delegate's later messages are still its own.
                    >
                    > Injected text: {}
                ",
                    single_line(&message.text)
                );
            }
            MessageSource::Steer => {
                let _ = indoc::writedoc!(
                    rendered_turn,
                    "

                    ### user (steer)

                    {}
                ",
                    message.text.trim_end()
                );
            }
            MessageSource::Synthetic => {
                let _ = indoc::writedoc!(
                    rendered_turn,
                    "

                    > [!NOTE]
                    > {}
                ",
                    single_line(&message.text)
                );
            }
            MessageSource::Delegate => {
                let label = match (message.role, reopened) {
                    (Role::Assistant, false) => "assistant",
                    (Role::Assistant, true) => "assistant (after hook reopen — not the answer)",
                    (Role::User, _) => "user",
                    (Role::System, _) => "system",
                };
                let _ = indoc::writedoc!(
                    rendered_turn,
                    "

                    ### {label}

                    {}
                ",
                    message.text.trim_end(),
                );
            }
        }
    }
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
    // Priced at list from the token counts, which is not what a subscription account is charged.
    if let Some(cost) = cost_usd {
        parts.push(format!("${cost:.4} list price"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" — {}", parts.join(", "))
    }
}
