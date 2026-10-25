//! What every front end tells a reader about a consultation: which warnings apply, and the facts
//! both the CLI and the MCP server print the same way.
//!
//! Changes when what a reader must be told changes.
//!
//! The front ends differ in audience — a person at a terminal, or a language model that will act
//! on the text — and so in layout and wording, but never in which facts they report.
//! [`RunStatus::warnings`] is the one place that decides which warnings apply, and each front end
//! matches [`Warning`] without a wildcard, so a warning added here does not compile until both say
//! it.
//! The lines below are facts with one phrasing, which both front ends print verbatim.

use std::path::Path;

use chrono::Utc;

use crate::delegate::{AccountAlias, Billing, Vendor};
use crate::quota::{AccountQuota, Observation, Origin, Refresh};
use crate::run::{HookReopening, RunStatus, describe_delegate};
use crate::transcript::{EarlierFailure, FailureKind, Outcome, RateLimit};

/// Something a reader must be told about a consultation, because it changes how the transcript
/// reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning<'a> {
    /// The newest turn failed; whatever arrived before the failure is in the transcript.
    Failed {
        /// What kind of failure.
        kind: FailureKind,
        /// The delegate's own words.
        detail: &'a str,
        /// What its CLI wrote to stderr, when the failure detail does not already quote it.
        stderr: Option<&'a str>,
    },
    /// Turns before the newest failed, whatever the newest is doing.
    EarlierFailures(&'a [EarlierFailure]),
    /// The delegate could not save its session, so no follow-up can resume it.
    SessionNotSaved {
        /// Where it would have been saved.
        store: &'a Path,
    },
    /// A follow-up asked to continue a session and opened a new one, so it answered with no
    /// memory of the earlier turns.
    BrokeContinuity,
    /// A hook reopened a finished turn.
    HookReopened(HookReopening),
    /// The delegate CLI wrote events agentmux does not model.
    Unrecognised {
        /// How many.
        total: u32,
        /// Which, with counts.
        summary: String,
        /// Where the raw stream is.
        events_path: &'a Path,
        /// Whether codex sent app-server messages the generated protocol could not read, which
        /// running the consultation over `codex exec` gets round.
        has_undecoded_app_server_messages: bool,
    },
    /// A Claude delegate also reads the instruction files at or above its working directory,
    /// which isolation does not stop.
    InstructionFiles {
        /// Its working directory.
        cwd: &'a Path,
    },
}

impl RunStatus {
    /// Every warning that applies to this consultation, in the order a reader should meet them.
    #[must_use]
    pub fn warnings(&self) -> Vec<Warning<'_>> {
        let mut warnings = Vec::new();
        if let Outcome::Failed { kind, detail } = &self.outcome {
            warnings.push(Warning::Failed {
                kind: *kind,
                detail,
                stderr: self.delegate_stderr.as_deref(),
            });
        }
        if !self.earlier_failures.is_empty() {
            warnings.push(Warning::EarlierFailures(&self.earlier_failures));
        }
        if let Some(store) = &self.unsaved_session_store {
            warnings.push(Warning::SessionNotSaved { store });
        }
        if self.broke_continuity {
            warnings.push(Warning::BrokeContinuity);
        }
        if let Some(reopening) = self.hook_reopening {
            warnings.push(Warning::HookReopened(reopening));
        }
        if let Some(summary) = self.unrecognised.summary() {
            warnings.push(Warning::Unrecognised {
                total: self.unrecognised.total(),
                summary,
                events_path: &self.events_path,
                has_undecoded_app_server_messages: self.has_undecoded_app_server_messages,
            });
        }
        // Measured: `--setting-sources ""` stops hooks and settings but does not stop the Claude
        // CLI reading a project's instruction file, and it searches up the directory tree rather
        // than only in `cwd`.
        // agentmux cannot tell which files that found, so the rule is stated rather than guessed
        // at: a note that fired only when the file sat in `cwd` would read as "there is none"
        // everywhere else.
        if self.delegate.vendor() == Vendor::Claude {
            warnings.push(Warning::InstructionFiles { cwd: &self.cwd });
        }
        warnings
    }
}

/// A one-line state, with the failure reason inline when there is one.
#[must_use]
pub fn outcome_line(outcome: &Outcome) -> String {
    match outcome {
        // The detail is the vendor's verbatim text: a JSON blob from Claude, sometimes several
        // lines from Codex.
        // Left whole it breaks a header's alignment; the full text is in the failure warning.
        Outcome::Failed { kind, detail } => {
            format!("failed ({}): {}", kind.label(), summarise(detail, 160))
        }
        other => other.state().to_string(),
    }
}

/// The turns before the newest that failed, in one line, such as
/// `turn 2 failed (content flagged), turn 3 failed (rate limited)`.
///
/// Phrased to stand beside [`outcome_line`] rather than inside it: the state reports the newest
/// turn, and an earlier failure written into it reads as the state of the turn running now.
#[must_use]
pub fn earlier_failures_line(failures: &[EarlierFailure]) -> String {
    failures
        .iter()
        .map(|failure| {
            format!(
                "turn {} failed ({})",
                failure.turn_number(),
                failure.kind().label()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Collapse whitespace and clip, so a value stays on one line.
#[must_use]
pub fn summarise(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let clipped: String = flat.chars().take(max_chars).collect();
    format!("{clipped}…")
}

/// Who ran, and what actually answered when that is not what was asked for.
///
/// Two things can move a model out from under a request, and a caller comparing consultations has
/// to be able to see both.
/// The machine's configuration may rewrite the identifier before launch, which
/// [`describe_delegate`] names; the vendor may then expand it into a fuller one — `sonnet`
/// answering as `claude-sonnet-5` — which is invisible in a result that echoes only the request.
///
/// A turn on API pricing says so here, on the one line every result opens with, because which
/// pricing a consultation runs on is decided by the account and invisible from the request.
#[must_use]
pub fn delegate_line(status: &RunStatus) -> String {
    let summary = describe_delegate(&status.delegate, status.rewritten_from.as_ref());
    let line = match &status.resolved_model {
        Some(resolved) if resolved != status.delegate.model().as_str() => {
            format!("{summary} (answered by {resolved})")
        }
        _ => summary,
    };
    match status.billing {
        Some(Billing::Api) => format!("{line} billing=api"),
        Some(Billing::Subscription) | None => line,
    }
}

/// An elapsed time, as both front ends print it.
#[must_use]
pub fn duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{}s", seconds / 60, seconds % 60),
        _ => format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// Where an account's usage window stands.
#[must_use]
pub fn rate_limit_line(limit: &RateLimit) -> String {
    let window = limit.window.as_deref().unwrap_or("usage");
    let remaining = limit.reopens_in(Utc::now());
    // Only an exhausted window is closed until it resets; saying "reopens" of any other reads as
    // a block on an account that can still answer.
    match (limit.exhausted, remaining) {
        (true, Some(remaining)) => format!(
            "{window} window exhausted, reopens in {} ({})",
            duration(remaining),
            limit.resets_at.to_rfc3339()
        ),
        (true, None) => format!("{window} window has reopened"),
        (false, Some(remaining)) => {
            let used = limit
                .used_percent
                .map_or_else(String::new, |percent| format!(" {percent:.0}% used,"));
            format!(
                "{window} window{used} resets in {} ({})",
                duration(remaining),
                limit.resets_at.to_rfc3339()
            )
        }
        (false, None) => format!("{window} window has reset"),
    }
}

/// The line that introduces the other accounts, for a consultation that ran as
/// `consultation_account`.
///
/// A consultation on the CLI's own login is not told the listed aliases are *other* accounts:
/// one of them may name that very login, and agentmux cannot tell.
/// With no other account at all there is nothing to switch to, and the lead-in says what the
/// figures are instead of inviting a switch to the account that just failed.
fn quota_lead_in(consultation_account: Option<&AccountAlias>, others_exist: bool) -> &'static str {
    match (others_exist, consultation_account) {
        (false, _) => "this account's usage:",
        (true, Some(_)) => "other accounts on this machine (pass one as `account`):",
        (true, None) => {
            "accounts this machine configures (pass one as `account`; this consultation ran on \
             the CLI's own login, which one of them may also be):"
        }
    }
}

/// The lines that report every account's figures after a rate-limited failure, lead-in first.
///
/// Empty for every other outcome, because only a rate limit fetches them.
/// Both front ends print exactly these lines under their own indentation, so they cannot order
/// or introduce the list differently.
///
/// The account that just failed goes last, flagged: it eats several lines restating what the
/// failure already said, and a reader skimming the top of the list would otherwise meet the
/// exhausted one first and conclude nothing is available.
#[must_use]
pub fn quota_block(status: &RunStatus) -> Vec<String> {
    let consultation_account = status.delegate.account().alias();
    // Only this vendor's accounts: the lead-in offers each as an `account` value, and an alias
    // from the other vendor's table resolves to nothing but a paid round trip.
    let (consultation_accounts, others): (Vec<_>, Vec<_>) = status
        .quota
        .iter()
        .filter(|entry| entry.vendor == status.delegate.vendor())
        .partition(|entry| entry.account.as_ref() == consultation_account);
    if consultation_accounts.is_empty() && others.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![quota_lead_in(consultation_account, !others.is_empty()).to_owned()];
    lines.extend(others.into_iter().flat_map(quota_report));
    for entry in consultation_accounts {
        let mut report = quota_report(entry);
        if let Some(first) = report.first_mut() {
            first.push_str(" — this consultation");
        }
        lines.extend(report);
    }
    lines
}

/// One account's quota, as lines a reader scans.
///
/// The vendor payloads have nothing in common — Claude reports a `limits` array of percentages,
/// Codex a map of named buckets — so this reads whichever shape is present and says nothing when
/// it recognises neither.
/// It is presentation only: the payload itself is passed through untouched, so a field this
/// function has never heard of still reaches a caller reading the JSON.
#[must_use]
pub fn quota_report(entry: &AccountQuota) -> Vec<String> {
    let mut lines = Vec::new();
    let account = entry
        .account
        .as_ref()
        .map_or_else(|| "(default)".to_owned(), ToString::to_string);
    let described = entry
        .description
        .as_ref()
        .map_or_else(String::new, |d| format!(" — {d}"));
    lines.push(format!("{} {account}{described}", entry.vendor));

    match &entry.observation {
        Observation::Unavailable { reason } => {
            // Said plainly, because the one reading this may be choosing where to send work and
            // must not read silence as spare capacity.
            lines.push(format!("  unavailable, not idle — {reason}"));
        }
        Observation::Reported { origin, payload } => {
            lines.push(format!("  {}", describe_origin(origin)));
            let windows = claude_windows(payload)
                .or_else(|| codex_windows(payload))
                .unwrap_or_default();
            if windows.is_empty() {
                lines.push("  no window this build recognises; read the payload".to_owned());
            }
            lines.extend(windows.into_iter().map(|line| format!("  {line}")));
        }
    }
    lines
}

/// How current a set of figures is.
fn describe_origin(origin: &Origin) -> String {
    match origin {
        Origin::Live => "live".to_owned(),
        Origin::Cache {
            fetched_at_ms,
            refresh,
            ..
        } => {
            let age = fetched_at_ms
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map_or_else(
                    || "from the CLI's cache, age unknown".to_owned(),
                    |at| {
                        let age = Utc::now().signed_duration_since(at).to_std().ok();
                        age.map_or_else(
                            || "from the CLI's cache".to_owned(),
                            |age| format!("from the CLI's cache, measured {} ago", duration(age)),
                        )
                    },
                );
            format!("{age}{}", describe_refresh(refresh))
        }
    }
}

/// What the attempt to refresh the cache adds to a cached reading.
///
/// Always said, including when nothing was spawned.
/// The age alone cannot distinguish figures that are current from figures that merely survived a
/// refresh which timed out, and that is the difference a reader is trying to make.
fn describe_refresh(refresh: &Refresh) -> String {
    match refresh {
        Refresh::Skipped => ", still inside the refresh window".to_owned(),
        Refresh::Succeeded => ", just refreshed".to_owned(),
        Refresh::TimedOut => ", and a refresh timed out — these figures predate it".to_owned(),
        Refresh::Failed { reason } => {
            format!(", and the refresh failed ({reason}) — these figures predate it")
        }
    }
}

/// Claude reports a `limits` array, each entry a percentage with its own severity.
fn claude_windows(payload: &serde_json::Value) -> Option<Vec<String>> {
    let limits = payload.get("utilization")?.get("limits")?.as_array()?;
    Some(
        limits
            .iter()
            .map(|limit| {
                let kind = limit
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("window");
                // The scope names a model, and only by display name: the vendor leaves `id` null,
                // which is why nothing here tries to match it to the model a caller asked for.
                let scope = limit
                    .get("scope")
                    .and_then(|s| s.get("model"))
                    .and_then(|m| m.get("display_name"))
                    .and_then(|n| n.as_str())
                    .map_or_else(String::new, |name| format!(" ({name})"));
                let percent = limit
                    .get("percent")
                    .and_then(serde_json::Value::as_i64)
                    .map_or_else(|| "?".to_owned(), |p| format!("{p}%"));
                let severity = limit
                    .get("severity")
                    .and_then(|s| s.as_str())
                    .filter(|s| *s != "normal")
                    .map_or_else(String::new, |s| format!(" [{s}]"));
                let resets = limit
                    .get("resets_at")
                    .and_then(|r| r.as_str())
                    .map_or_else(String::new, |r| format!(", resets {r}"));
                format!("{kind}{scope}: {percent} used{severity}{resets}")
            })
            .collect(),
    )
}

/// Codex reports a map of named buckets, each with a primary and optional secondary window.
fn codex_windows(payload: &serde_json::Value) -> Option<Vec<String>> {
    let buckets = payload.get("rateLimitsByLimitId")?.as_object()?;
    Some(
        buckets
            .iter()
            .map(|(id, bucket)| {
                let name = bucket
                    .get("limitName")
                    .and_then(|n| n.as_str())
                    .map_or_else(String::new, |n| format!(" ({n})"));
                let windows = ["primary", "secondary"]
                    .into_iter()
                    .filter_map(|which| {
                        let window = bucket.get(which)?;
                        let percent = window.get("usedPercent")?.as_i64()?;
                        let minutes = window
                            .get("windowDurationMins")
                            .and_then(serde_json::Value::as_i64)
                            .map_or_else(String::new, |m| format!(" over {m}m"));
                        Some(format!("{percent}% used{minutes}"))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{id}{name}: {windows}")
            })
            .collect(),
    )
}
