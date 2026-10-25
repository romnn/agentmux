//! Folds a delegate's raw event stream into a [`Turn`].
//!
//! Every capture is parsed here, and only here.
//! What a fold returns is vendor-neutral, so nothing downstream can be called with the wrong
//! vendor's shape.
//! The Codex capture model, [`codex::CaptureEvent`], is also what the app-server driver writes,
//! and it keeps whatever the server said that could not be decoded verbatim, as evidence.
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
    /// Whether the stream said the turn itself began, which only Codex's `turn.started` says.
    ///
    /// Over the app-server it is written by the driver once it follows the turn, which is when
    /// it delivers a steer.
    pub turn_started: bool,
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

/// Classify a vendor's error text into a [`crate::transcript::FailureKind`].
///
/// Both CLIs report failures as prose.
/// The mapping is a best effort over phrases both vendors have been observed to emit; anything
/// unfamiliar becomes `Unclassified` with the text kept verbatim, because guessing wrong is worse
/// than admitting ignorance.
/// Each phrase is a whole word or a specific pairing, because the text this sees is also the tail
/// of a CLI's stderr: `ECONNREFUSED` is not a refusal and a request id containing `429` is not a
/// rate limit, and a caller told either would go and fix the wrong thing.
/// It never reads an overload, which only the vendor's own stream may report: see
/// [`classify_reported`].
pub(crate) fn classify(detail: &str) -> crate::transcript::FailureKind {
    use crate::transcript::FailureKind;

    let text = detail.to_ascii_lowercase();
    let has = |needle: &str| text.contains(needle);
    let word = |needle: &str| has_word(&text, needle);

    if has("flagged") || has("cybersecurity risk") || word("refusal") || has("content policy") {
        FailureKind::ContentFlagged
    } else if has("rate limit") || has("rate_limit") || has("too many requests") || word("429") {
        FailureKind::RateLimited
    } else if has("insufficient_quota")
        || has("quota exceeded")
        || has("credit balance")
        || has("billing")
    {
        FailureKind::BudgetExceeded
    } else if has("model")
        && (has("not found") || has("not supported") || has("does not exist") || has("unsupported"))
    {
        FailureKind::ModelUnavailable
    } else if has("max turns") || has("max_turns") || has("turn limit") {
        FailureKind::TurnLimit
    } else {
        FailureKind::Unclassified
    }
}

/// Classify the error a delegate reported in its own event stream, which alone may say that the
/// vendor's model is overloaded.
///
/// An overload is the one failure agentmux acts on by itself, resuming the consultation at a
/// cost, so it is read only from the vendor's own report, and only in the vendor's own phrasing:
/// Codex's "Selected model is at capacity", Claude's `overloaded_error`, an OpenAI
/// `server_is_overloaded`, a model said to be overloaded.
/// Stderr, which [`classify`] also reads, is full of other things that are overloaded.
/// Every other failure is classified as [`classify`] does, and wins over an overload it mentions.
pub(crate) fn classify_reported(detail: &str) -> crate::transcript::FailureKind {
    use crate::transcript::FailureKind;

    const OVERLOAD: &[&str] = &[
        "model is at capacity",
        "overloaded_error",
        "server_is_overloaded",
        "model is overloaded",
        "model is currently overloaded",
    ];
    match classify(detail) {
        FailureKind::Unclassified => {
            let text = detail.to_ascii_lowercase();
            if OVERLOAD.iter().any(|phrase| text.contains(phrase)) {
                FailureKind::Overloaded
            } else {
                FailureKind::Unclassified
            }
        }
        kind => kind,
    }
}

/// Whether `needle` occurs in `text` bounded by non-alphanumerics on both sides.
fn has_word(text: &str, needle: &str) -> bool {
    text.match_indices(needle).any(|(start, _)| {
        let before = text.get(..start).and_then(|s| s.chars().next_back());
        let after = text
            .get(start + needle.len()..)
            .and_then(|s| s.chars().next());
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::{classify, classify_reported};
    use crate::transcript::FailureKind;

    /// A vendor that is out of capacity for the moment is told apart from every other failure,
    /// in each vendor's own words, and text that mentions capacity or an overload otherwise is
    /// not.
    #[gtest]
    fn an_overloaded_model_is_classified_narrowly() {
        let cases = [
            // Codex, verbatim, after a long turn
            (
                "Selected model is at capacity. Please try a different model.",
                FailureKind::Overloaded,
            ),
            // Claude's API error, as its CLI quotes it
            (
                r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                FailureKind::Overloaded,
            ),
            // OpenAI's error code
            (
                "stream error: server_is_overloaded",
                FailureKind::Overloaded,
            ),
            // A full disk is at capacity too, but no model is
            (
                "the codex CLI exited with status 1: write failed, volume at capacity",
                FailureKind::Unclassified,
            ),
            // An overload inside a longer identifier is not one
            (
                "the codex CLI exited with status 1: OverloadedMethodError in plugin",
                FailureKind::Unclassified,
            ),
            // Something else that is overloaded is not the vendor's model
            (
                "MCP server overloaded; fatal: permission denied",
                FailureKind::Unclassified,
            ),
            // A rate limit that mentions load is still a rate limit
            (
                "rate limit reached; the model is overloaded",
                FailureKind::RateLimited,
            ),
        ];
        for (detail, expected) in cases {
            expect_that!(classify_reported(detail), eq(expected), "{detail}");
        }
    }

    /// Text that did not come from the vendor's own stream, such as a CLI's stderr explaining a
    /// missing terminal event, is never read as an overload, whatever it says.
    #[gtest]
    fn stderr_never_reads_as_an_overload() {
        for detail in [
            "the codex CLI exited with status 1: Selected model is at capacity.",
            r#"the claude CLI exited with status 1: {"type":"overloaded_error"}"#,
            "the codex CLI exited with status 1: MCP server overloaded; fatal: permission denied",
        ] {
            expect_that!(classify(detail), eq(FailureKind::Unclassified), "{detail}");
        }
    }
}
