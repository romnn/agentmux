//! How a tool's answer reaches the calling agent.
//!
//! Changes when the tool output text changes.
//!
//! The reader is another language model, mid-task, that will act on this text without re-reading
//! any documentation.
//! Three rules follow from that:
//!
//! - **Header first.**
//!   A host that truncates a long result truncates the end, so the run id, the state and the
//!   paging cursor go at the top where truncation cannot reach them.
//! - **Warnings above the content.**
//!   Anything that changes how the transcript must be read — a reopened turn, an unrecognised
//!   event — is useless underneath the thing it is about.
//! - **Say what to call next, with the arguments filled in.**
//!   A caller that has to derive the next call is a caller that will guess.
//!
//! There is deliberately no `answer`, `final_message` or `summary` anywhere in this module.
//! That absence is the product, and [`crate`]'s tests guard it.

use std::borrow::Cow;
use std::fmt::Write as _;

use agentmux::run::{RunStatus, RunSummary, TranscriptPage};
use agentmux::transcript::{FailureKind, Outcome, RateLimit};
use chrono::Utc;

use crate::tools::MAX_WAIT_SECONDS;

/// A one-line state, with the failure reason inline when there is one.
#[must_use]
pub fn outcome_line(outcome: &Outcome) -> String {
    match outcome {
        // The detail is the vendor's verbatim text: a JSON blob from Claude, sometimes several
        // lines from Codex.
        // Left whole it breaks the header's alignment and pushes `transcript:` and `bytes:` down,
        // undoing the truncation protection the header exists to provide.
        // The full text is in the failure warning below, where length costs nothing.
        Outcome::Failed { kind, detail } => {
            format!("failed ({}): {}", kind.label(), summarise(detail, 160))
        }
        other => other.state().to_owned(),
    }
}

/// Collapse whitespace and clip, so a value stays on one header line.
fn summarise(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let clipped: String = flat.chars().take(max_chars).collect();
    format!("{clipped}…")
}

/// Which tool is speaking, so the paging advice in the header names that tool rather than another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// A result that carries no transcript text.
    State,
    /// `result`, which pages the whole transcript from an offset.
    Result,
    /// `tail`, which follows the end of a growing transcript from a cursor.
    Tail,
}

/// The header every tool result opens with.
///
/// `page` is present when the result carries transcript bytes, so the caller can see what it has
/// and what remains.
#[must_use]
pub fn header(status: &RunStatus, caller: Caller, page: Option<&TranscriptPage>) -> String {
    let mut out = String::new();
    let mut state = outcome_line(&status.outcome);
    if let Some((turn, kind)) = status.earlier_failure {
        // The state line reports the newest turn, so without this a follow-up onto a failed turn
        // would present the consultation as completed and leave the failure buried in the body.
        let _ = write!(
            state,
            " (turn {} failed: {} — see the transcript)",
            turn.saturating_add(1),
            kind.label(),
        );
    }
    let _ = write!(
        out,
        "run_id:     {}\n\
         state:      {}\n\
         delegate:   {}\n\
         reading:    {}\n\
         progress:   {} elapsed · {} turn(s) · {} message(s)",
        status.run_id,
        state,
        status.delegate.summary(),
        status.cwd.display(),
        duration(status.elapsed),
        status.turns,
        status.message_count,
    );
    if let Some(input) = status.usage.input_tokens {
        let _ = write!(
            out,
            " · {input} in / {} out",
            status.usage.output_tokens.unwrap_or(0)
        );
    }
    if let Some(cost) = status.cost_usd {
        let _ = write!(out, " · ${cost:.4}");
    }
    let _ = writeln!(out, "\ntranscript: {}", status.transcript_path.display());

    match page {
        Some(page) if page.at_end => {
            let _ = writeln!(
                out,
                "bytes:      {}-{} of {}",
                page.offset, page.next_offset, page.total_bytes
            );
        }
        Some(page) => {
            let follow_on = match caller {
                Caller::Tail => "call tail again with cursor",
                Caller::State | Caller::Result => "call result with offset",
            };
            let _ = writeln!(
                out,
                "bytes:      {}-{} of {} — more follows; {follow_on} {}",
                page.offset, page.next_offset, page.total_bytes, page.next_offset,
            );
        }
        None => {
            let _ = writeln!(out, "bytes:      {} so far", status.transcript_bytes);
        }
    }
    out.push_str(&warnings(status));
    out
}

/// Warnings and disclosures that change how the transcript must be read.
#[must_use]
pub fn warnings(status: &RunStatus) -> String {
    let mut out = String::new();

    if let Outcome::Failed { kind, detail } = &status.outcome {
        let _ = write!(
            out,
            "warning:    this consultation failed ({}), and the failure did not discard the work.\n\
             \x20           {} message(s) arrived before it and are all in the transcript below.\n\
             \x20           {}\n\
             \x20           The delegate said: {}\n",
            kind.label(),
            status.message_count,
            recovery_advice(*kind, status.rate_limit.as_ref()),
            summarise(detail, 600),
        );
    }
    if status.broke_continuity {
        out.push_str(
            "warning:    a follow-up did not resume the earlier session.\n\
             \x20           The delegate CLI accepted the resume and silently started a new\n\
             \x20           conversation, so that turn answered with no memory of the earlier\n\
             \x20           ones. Read it as a fresh consultation, and restate the brief if the\n\
             \x20           answer looks context-free.\n",
        );
    }
    if status.reopened_by_hook {
        out.push_str(
            "warning:    a hook in the delegate's environment reopened a finished turn. The last\n\
             \x20           thing the delegate said is NOT the answer — the answer is above the\n\
             \x20           injection, which is marked in the transcript.\n",
        );
    }
    if let Some(drift) = status.unrecognised.summary() {
        let _ = write!(
            out,
            "warning:    agentmux did not recognise {} event(s) from the delegate CLI ({drift}).\n\
             \x20           The CLI may have changed its output format and this transcript may be\n\
             \x20           missing content. The raw stream is at {}.\n",
            status.unrecognised.total(),
            status.events_path.display(),
        );
    }
    // Measured: `--setting-sources ""` stops hooks and settings but does not stop the Claude CLI
    // reading a project's instruction file, and it searches up the directory tree rather than
    // only in `cwd`.
    // agentmux cannot tell which files that found, so it states the rule rather than guessing at
    // the answer — a note that fired only when the file happened to sit in `cwd` would read as
    // "there is none" everywhere else.
    if status.delegate.vendor() == agentmux::delegate::Vendor::Claude {
        let _ = write!(
            out,
            "note:       a Claude delegate also reads any CLAUDE.md or AGENTS.md it finds at or \n\
             \x20           above {}. Those become part of its prompt and you did not write them.\n",
            status.cwd.display(),
        );
    }

    out
}

/// What a caller should do about a particular failure.
///
/// A failure is not automatically a reason to retry.
/// The most valuable case is the one where retrying is exactly wrong: a blocked closing message
/// means the analysis is already in the transcript and paying for it again would produce the same
/// block.
fn recovery_advice(kind: FailureKind, rate_limit: Option<&RateLimit>) -> Cow<'static, str> {
    // A rate limit is the only failure whose advice inverts on a number, so it is decided before
    // the table of fixed answers below.
    if kind == FailureKind::RateLimited
        && let Some(limit) = rate_limit
    {
        return Cow::Owned(rate_limit_advice(limit));
    }

    Cow::Borrowed(fixed_advice(kind))
}

/// One line describing where an account's usage window stands.
///
/// Shared with the CLI so both surfaces name the window the same way; a caller comparing the two
/// should never have to work out whether they mean the same thing.
#[must_use]
pub fn rate_limit_line(limit: &RateLimit) -> String {
    let window = limit.window.as_deref().unwrap_or("usage");
    match limit.reopens_in(Utc::now()) {
        Some(remaining) => format!(
            "{window} window reopens in {} ({})",
            duration(remaining),
            limit.resets_at.to_rfc3339()
        ),
        None => format!("{window} window has reopened"),
    }
}

/// Advice for a rate limit whose window reopening time the vendor disclosed.
///
/// The choice between waiting and switching is not a matter of taste: below the cap a single
/// `start` call can block for, waiting costs one call, and above it waiting means a caller polling
/// a run that cannot progress.
fn rate_limit_advice(limit: &RateLimit) -> String {
    let window = limit
        .window
        .as_deref()
        .map_or_else(String::new, |name| format!(" ({name})"));

    let Some(remaining) = limit.reopens_in(Utc::now()) else {
        return format!(
            "The usage window{window} has already reopened, so this failure is stale — `start` \
             the same question again."
        );
    };

    if remaining <= std::time::Duration::from_secs(MAX_WAIT_SECONDS) {
        format!(
            "Nothing to fix in your call. The window{window} reopens in {}, which is inside what \
             one `start` can wait for — retry with `wait_seconds` set to cover it rather than \
             switching vendor.",
            duration(remaining)
        )
    } else {
        format!(
            "Nothing to fix in your call, and waiting is not worth it: the window{window} does \
             not reopen for {}. `start` the same question against the other vendor, or against a \
             model this account has not exhausted.",
            duration(remaining)
        )
    }
}

/// Advice that depends only on the kind of failure.
fn fixed_advice(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::ContentFlagged => {
            "A guardrail refused the write-up, not the work — the findings above are usually \
             complete, so read them before retrying. If you do retry, rephrase rather than \
             escalate: the vocabulary of adversarial review trips this on ordinary code review."
        }
        FailureKind::ModelUnavailable => {
            "The delegate CLI named what it does accept in the detail below. Correct `model` and \
             `start` again."
        }
        FailureKind::RateLimited => {
            "Nothing to fix in your call. Wait, or `start` the same question against the other \
             vendor."
        }
        FailureKind::BudgetExceeded => {
            "The account is out of credit or over a cap. Nothing to retry until that changes."
        }
        FailureKind::TurnLimit => {
            "The delegate ran out of turns with work still to do. Its session is still open, so a \
             `follow_up` asking it to continue is cheaper than starting over."
        }
        FailureKind::LaunchFailed => {
            "The delegate CLI did not start. Check that it is installed, on PATH, and logged in."
        }
        FailureKind::Unclassified => {
            "agentmux does not recognise this failure. The delegate's own words are below, and \
             the raw stream is at the events path."
        }
    }
}

/// What to call next, with the arguments filled in.
#[must_use]
pub fn next_steps(status: &RunStatus) -> String {
    let id = &status.run_id;
    if status.is_terminal() {
        let mut out = format!("\nnext:       result {{\"run_id\": \"{id}\"}}\n");
        if status.resumable {
            let _ = write!(
                out,
                "            follow_up {{\"run_id\": \"{id}\", \"question\": \"…\"}} — continues the\n\
                 \x20           delegate's own session, so do not restate the brief\n",
            );
        }
        out
    } else {
        format!(
            "\nStill working. The delegate is detached and survives an agentmux restart.\n\
             next:       tail {{\"run_id\": \"{id}\", \"cursor\": {}}} — new output only\n\
             \x20           result {{\"run_id\": \"{id}\", \"wait_seconds\": 45}} — block for the answer\n\
             \x20           cancel {{\"run_id\": \"{id}\"}} — stop it, keeping what was collected\n",
            status.transcript_bytes,
        )
    }
}

/// A consultation's state, with no transcript text.
///
/// What both `start` and `status` return: the same facts and the same next call, because a
/// consultation that has just begun and one being polled need exactly the same thing said about
/// them.
#[must_use]
pub fn status(status: &RunStatus) -> String {
    format!(
        "{}{}",
        header(status, Caller::State, None),
        next_steps(status)
    )
}

/// A transcript, with the header a reader needs in order to trust it.
#[must_use]
pub fn transcript(status: &RunStatus, page: &TranscriptPage) -> String {
    let mut out = header(status, Caller::Result, Some(page));
    if !status.is_terminal() {
        out.push_str("note:       still running; what follows is only what has arrived so far.\n");
    }
    out.push_str("\n---\n\n");
    out.push_str(&page.text);
    if !page.at_end {
        let _ = write!(
            out,
            "\n\n---\n[truncated. Continue with result {{\"run_id\": \"{}\", \"offset\": {}}}, or \
             read the whole transcript from the file named above with your own file tools.]\n",
            status.run_id, page.next_offset,
        );
    }
    out
}

/// New transcript bytes since a cursor.
#[must_use]
pub fn tail(status: &RunStatus, page: &TranscriptPage) -> String {
    let mut out = header(status, Caller::Tail, Some(page));
    let _ = writeln!(out, "next_cursor: {}", page.next_offset);
    if page.text.is_empty() {
        let _ = write!(
            out,
            "\nNo new output since byte {}. Call tail again with the cursor above.\n",
            page.offset,
        );
        return out;
    }
    out.push_str("\n---\n\n");
    out.push_str(&page.text);
    if !page.at_end {
        out.push_str("\n\n---\n[more is already waiting; call tail again with next_cursor.]\n");
    } else if !status.is_terminal() {
        let _ = write!(
            out,
            "\n\n---\n[caught up. Call tail again with next_cursor {}, or result with \
             wait_seconds to block for the answer.]\n",
            page.next_offset,
        );
    }
    out
}

/// Recent consultations.
#[must_use]
pub fn list(runs: &[RunSummary]) -> String {
    if runs.is_empty() {
        return "no consultations yet. Start one with `ask`, or with `start` if it will take \
                minutes."
            .to_owned();
    }
    let mut out = String::new();
    for run in runs {
        let _ = write!(
            out,
            "{}  {:<9}  {}\n    {}\n    {}\n",
            run.run_id,
            run.state,
            run.created_at.to_rfc3339(),
            run.delegate,
            run.question,
        );
    }
    out
}

/// Render an elapsed time the way both front ends do.
///
/// Public so the CLI can print the same string; a second copy across the crate boundary would
/// drift where nobody would notice.
#[must_use]
pub fn duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{}s", seconds / 60, seconds % 60),
        _ => format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60),
    }
}
