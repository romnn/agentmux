//! What a caller is told about a consultation.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::delegate::{Billing, Delegate, Isolation, ModelId};
use crate::transcript::{FailureKind, Outcome, RateLimit, UnrecognisedEvents, Usage};

use super::id::RunId;

/// A consultation as a caller sees it.
///
/// Serialises as the machine-readable form both front ends print, so a field added here reaches
/// `--json` without anyone remembering to copy it.
#[derive(Debug, Clone, Serialize)]
pub struct RunStatus {
    /// The consultation's id.
    pub run_id: RunId,
    /// Who is being consulted, as its newest turn ran: the isolation and permissions a follow-up
    /// keeps unless it changes them.
    pub delegate: Delegate,
    /// The directory the delegate is reading the project from.
    ///
    /// Reported because getting it wrong is silent: a delegate pointed at the wrong worktree
    /// returns a confident review of code that was never under review.
    pub cwd: PathBuf,
    /// `running`, `completed`, `failed` or `cancelled`, with the detail of a failure.
    pub outcome: Outcome,
    /// When `start` was called.
    pub created_at: DateTime<Utc>,
    /// How long the consultation has been going, or how long it took.
    #[serde(rename = "elapsed_seconds", serialize_with = "whole_seconds")]
    pub elapsed: Duration,
    /// How many turns the consultation has.
    pub turns: u32,
    /// How many messages the transcript holds across every turn.
    ///
    /// Counts hook injections and agentmux's own notes alongside the delegate's replies, because
    /// a caller polling this wants to know that *something* arrived.
    pub message_count: usize,
    /// Token accounting so far.
    pub usage: Usage,
    /// Reported dollar cost so far, where the vendor reports one.
    pub cost_usd: Option<f64>,
    /// The model identifier the vendor reported running, where it reported one.
    ///
    /// Kept beside the requested identifier rather than replacing it: a vendor may expand an alias
    /// into a full identifier, and a caller comparing two consultations needs to see which of the
    /// two it is looking at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    /// The model the caller named, when this machine's configuration rewrote it to another.
    ///
    /// `delegate` names the model that ran.
    /// Reported so that a caller which pinned a model and reads another back can see that a rule
    /// of its own machine moved it, rather than reading it as a pin agentmux ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewritten_from: Option<ModelId>,
    /// The directory the delegate CLI saves its sessions in, when agentmux could not write to it
    /// as the newest turn was launched, which means the delegate could not save its session either.
    ///
    /// Carried so every reader says it: the answer is unaffected, but no follow-up can resume the
    /// consultation, which is also why `resumable` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsaved_session_store: Option<PathBuf>,
    /// What the delegate CLI wrote to stderr, when the newest turn failed and its failure detail
    /// does not already quote it.
    ///
    /// A CLI often explains a refusal only there: a resume that finds no saved conversation says so
    /// on stderr and hands the stream nothing but a bare error.
    /// A failure its exit record settled already quotes the stderr tail that record froze, so the
    /// stderr is quoted here only when no exit record settled the failure, and never twice.
    /// This is a snapshot kept outside the append-only transcript, so reading a file that may
    /// still grow moves nothing already published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegate_stderr: Option<String>,
    /// Event types the parser met but does not model.
    pub unrecognised: UnrecognisedEvents,
    /// What it means that a hook reopened a finished turn, when one did.
    ///
    /// Decided here rather than by each front end, because the same event means opposite things
    /// depending on what the run loaded and every reader has to say the same thing about it.
    pub hook_reopening: Option<HookReopening>,
    /// An earlier turn that failed, when the newest turn is not the one that failed.
    ///
    /// Without this a follow-up onto a failed turn would report the consultation as `completed`
    /// and leave the failure visible only deep inside the transcript body.
    pub earlier_failure: Option<(u32, FailureKind)>,
    /// What each account of this vendor had left, when the consultation was rate limited.
    ///
    /// Empty for every other outcome: probing costs a process, and the answer is only actionable
    /// when the caller has just been refused.
    pub quota: Vec<crate::quota::AccountQuota>,
    /// The most recent usage window the delegate reported, when it reported one.
    ///
    /// Carried out of the transcript because the right response to a rate limit depends on how
    /// long the window has left, and a caller should not have to read the transcript body to find
    /// a timestamp the stream already gave up.
    pub rate_limit: Option<RateLimit>,
    /// Whether any turn asked to continue a session and silently opened a new one instead.
    pub broke_continuity: bool,
    /// Whether codex sent app-server messages the generated protocol could not read, which
    /// running the consultation over `codex exec` gets round.
    #[serde(rename = "undecoded_messages")]
    pub has_undecoded_app_server_messages: bool,
    /// Where the rendered transcript is written.
    pub transcript_path: PathBuf,
    /// Where the delegate's raw event stream for the newest turn is written.
    pub events_path: PathBuf,
    /// Where the delegate's stderr for the newest turn is written.
    pub stderr_path: PathBuf,
    /// The size of the rendered transcript, which is also the cursor a `tail` would end at.
    pub transcript_bytes: u64,
    /// Whether a follow-up is possible: the delegate announced a resumable session.
    pub resumable: bool,
    /// Whether `steer` can reach the newest turn: it is running, and it was launched over
    /// `codex app-server`.
    pub steerable: bool,
    /// The pricing the newest turn was launched on, or `None` for a turn recorded before agentmux
    /// kept it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
    /// Whether the newest turn's settlement is committed: its transcript is published and final,
    /// and `wait` returns it.
    ///
    /// Apart from [`RunStatus::outcome`], which says what the stream reported: a turn whose
    /// stream has ended settles once its delegate has gone, a moment later.
    pub settled: bool,
}

pub(super) fn whole_seconds<S: serde::Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(duration.as_secs())
}

impl RunStatus {
    /// Whether nothing more can arrive.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.outcome.is_terminal()
    }
}

/// What it means that a hook reopened a finished turn.
///
/// Ordered by how alarming it is, so the most alarming of several turns' readings is their
/// maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookReopening {
    /// The run inherited the account's settings, so a hook is a documented consequence of what
    /// the caller asked for.
    Expected,
    /// The run loaded no settings, so no hook should have been able to reach it, and the whole
    /// transcript is suspect.
    Unexpected,
}

impl HookReopening {
    /// What the reopenings in `turns` mean, each judged by its own turn's isolation, or by
    /// `recorded`, the consultation's, for a turn that predates agentmux keeping it per turn.
    ///
    /// One reopening of a turn that loaded no settings is alarming whatever the others were, so
    /// it decides.
    pub(super) fn of_turns(
        turns: &[crate::transcript::Turn],
        recorded: Option<Isolation>,
    ) -> Option<Self> {
        turns
            .iter()
            .filter(|turn| turn.was_reopened_by_hook())
            .filter_map(|turn| Self::of(true, turn.isolation.or(recorded)))
            .max()
    }

    /// What a reopening means for a run with this isolation, when one happened.
    fn of(reopened: bool, isolation: Option<Isolation>) -> Option<Self> {
        if !reopened {
            return None;
        }
        Some(match isolation {
            Some(Isolation::Inherit) => Self::Expected,
            // `None` means the run predates isolation being recorded, so the safe reading is the
            // alarming one: it is written out rather than wildcarded so a third mode cannot land
            // here by default.
            None | Some(Isolation::Isolated) => Self::Unexpected,
        })
    }
}

/// One line about a consultation, for `list`.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    /// The consultation's id.
    pub run_id: RunId,
    /// A one-line description of the delegate.
    pub delegate: String,
    /// How the consultation stands.
    pub outcome: Outcome,
    /// When `start` was called.
    pub created_at: DateTime<Utc>,
    /// The first line of the question, truncated.
    pub question: String,
    /// The pricing the newest turn was launched on, or `None` for one recorded before agentmux
    /// kept it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
}

/// A slice of the rendered transcript.
#[derive(Debug, Clone)]
pub struct TranscriptPage {
    /// The text, from `offset`.
    pub text: String,
    /// Where this slice started.
    pub offset: u64,
    /// Where the next slice starts.
    /// Pass it back as the next `offset` or `cursor`.
    pub next_offset: u64,
    /// Total size of the rendered transcript right now.
    pub total_bytes: u64,
    /// Whether `next_offset` has reached the end of what exists so far.
    pub at_end: bool,
}

/// One line naming the delegate, and the model that was asked for when another one ran.
///
/// The one phrasing of a rewrite, so the transcript header, `list` and every front end say the
/// same thing about it.
/// A consultation whose model was rewritten is otherwise indistinguishable from one that asked
/// for the rewritten model outright, and the two are worth different amounts to a reader working
/// out why an answer looks the way it does.
#[must_use]
pub fn describe_delegate(delegate: &Delegate, rewritten_from: Option<&ModelId>) -> String {
    let summary = delegate.summary();
    match rewritten_from {
        Some(asked) => format!("{summary} (asked for {asked})"),
        None => summary,
    }
}

pub(super) fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let cut: String = line.chars().take(max).collect();
    format!("{cut}…")
}

/// How much of a failed turn's stderr a status carries.
///
/// Half of what an exit record keeps: a status is read on every poll and shown inline, the few
/// lines a CLI explains a refusal in fit, and the whole file is named beside it.
pub(super) const STATUS_STDERR_TAIL: usize = 1000;

/// Slice a render by byte offset without ever splitting a character.
pub(super) fn page_of(full: &str, offset: u64, max_bytes: usize) -> TranscriptPage {
    let total = u64::try_from(full.len()).unwrap_or(u64::MAX);
    let start = usize::try_from(offset.min(total)).unwrap_or(usize::MAX);
    // Never split a UTF-8 character: walk back to a boundary, then forward for the end.
    let start = full.floor_char_boundary(start);
    let mut end = full.floor_char_boundary(start.saturating_add(max_bytes).min(full.len()));
    if end <= start && start < full.len() {
        // `max_bytes` was smaller than the next character — every turn heading contains an em
        // dash, so this is reachable with an accepted page size.
        // Returning an empty page would leave the cursor where it was and a polling caller
        // would spin forever, so emit one whole character instead.
        end = full.ceil_char_boundary(start.saturating_add(1));
    }
    let text = full.get(start..end).unwrap_or_default().to_owned();
    let next = u64::try_from(end).unwrap_or(total);
    TranscriptPage {
        text,
        offset: u64::try_from(start).unwrap_or(offset),
        next_offset: next,
        total_bytes: total,
        at_end: next >= total,
    }
}

/// How long a consultation has been going, or how long it took.
///
/// A finished consultation's clock stops at the last write to a capture file rather than at now,
/// so a run collected an hour after it finished does not report a one-hour duration.
pub(super) fn elapsed_since(
    created_at: DateTime<Utc>,
    outcome: &Outcome,
    last_activity: Option<SystemTime>,
) -> Duration {
    let end = if outcome.is_terminal() {
        last_activity.map_or_else(Utc::now, DateTime::<Utc>::from)
    } else {
        Utc::now()
    };
    end.signed_duration_since(created_at)
        .to_std()
        .unwrap_or_default()
}
