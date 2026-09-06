//! Folds a delegate's raw event stream into a [`Turn`].
//!
//! Parsing happens here and only here.
//! No `serde_json::Value` leaves this module, so everything downstream is vendor-neutral and
//! cannot be called with the wrong vendor's shape.
//!
//! # Why every fold counts what it does not recognise
//!
//! agentmux wraps two CLIs owned by other people, whose JSON event shapes are versioned by
//! nobody's promise.
//! A parser that silently skips what it does not recognise will one day silently skip the report —
//! the process still exits zero and still prints something plausible.
//! So unrecognised event types are **counted** into [`Turn::unrecognised`] and surfaced in `status`
//! and `result`, and a fixture test fails the build when a recorded stream contains a type the
//! parser does not name.
//! Drift becomes loud instead of lossy.

pub mod claude;
pub mod codex;

use serde::Deserialize;

use crate::delegate::{SessionRef, Vendor};
use crate::transcript::Turn;

/// The result of folding one child process's event stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Fold {
    /// The turn as the stream describes it.
    pub turn: Turn,
    /// The delegate's own handle on the conversation, when the stream announced one.
    ///
    /// Without this a follow-up is impossible, so it is recovered on every fold rather than
    /// remembered from the launch.
    pub session: Option<SessionRef>,
}

/// Counted under [`Turn::unrecognised`] when a line is not JSON at all.
///
/// A CLI that starts printing a human-readable warning onto its JSON channel would otherwise be
/// invisible.
pub const MALFORMED_LINE: &str = "<malformed-json-line>";

/// Fold a delegate's event stream into a turn.
///
/// `events` is the raw JSONL exactly as the child wrote it.
/// A trailing partial line — normal while the child is still running — is ignored rather than
/// counted as malformed.
#[must_use]
pub fn fold(vendor: Vendor, index: u32, question: &str, events: &str) -> Fold {
    match vendor {
        Vendor::Claude => claude::fold(index, question, events),
        Vendor::Codex => codex::fold(index, question, events),
    }
}

/// Iterate the complete lines of a JSONL buffer, skipping blanks and any trailing partial line.
///
/// While a child is running the last line is usually half-written.
/// Treating it as data would produce a spurious malformed-line count on every poll.
pub(crate) fn complete_lines(events: &str) -> impl Iterator<Item = &str> {
    let complete = match events.rfind('\n') {
        Some(last) => events.get(..=last).unwrap_or(events),
        None => "",
    };
    complete
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
}

/// Just enough of a line to know which typed shape to parse it as.
#[derive(Debug, Deserialize)]
pub(crate) struct TypeProbe {
    #[serde(rename = "type")]
    pub(crate) kind: String,
}

/// Read the top-level `type` of a JSONL line.
pub(crate) fn probe(line: &str) -> Option<String> {
    serde_json::from_str::<TypeProbe>(line)
        .ok()
        .map(|probe| probe.kind)
}

/// Classify a vendor's error text into a [`crate::transcript::FailureKind`].
///
/// Both CLIs report failures as prose.
/// The mapping is a best effort over phrases both vendors have been observed to emit; anything
/// unfamiliar becomes `Unclassified` with the text kept verbatim, because guessing wrong is worse
/// than admitting ignorance.
pub(crate) fn classify(detail: &str) -> crate::transcript::FailureKind {
    use crate::transcript::FailureKind;

    let text = detail.to_ascii_lowercase();
    let has = |needle: &str| text.contains(needle);

    if has("flagged") || has("cybersecurity risk") || has("refus") || has("safety") {
        FailureKind::ContentFlagged
    } else if has("rate limit") || has("rate_limit") || has("429") || has("too many requests") {
        FailureKind::RateLimited
    } else if has("quota") || has("insufficient_quota") || has("credit") || has("billing") {
        FailureKind::BudgetExceeded
    } else if has("model") && (has("not found") || has("not supported") || has("does not exist")) {
        FailureKind::ModelUnavailable
    } else if has("max turns") || has("max_turns") || has("turn limit") {
        FailureKind::TurnLimit
    } else {
        FailureKind::Unclassified
    }
}
