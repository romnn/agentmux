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

use std::collections::BTreeMap;
use std::path::Path;

use chrono::Utc;

use crate::accounting::{Spend, Tokens, Totals};
use crate::delegate::{AccountAlias, Billing, Vendor};
use crate::quota::{AccountQuota, Observation, Origin, Refresh};
use crate::run::{
    AUTO_RESUME_QUESTION, AutoResume, HookReopening, RunStatus, TurnLeftRunning, describe_delegate,
};
use crate::transcript::{EarlierFailure, FailureKind, Outcome, UsageWindow};

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
    /// What agentmux does, or did, about the newest turn having failed because the model was
    /// overloaded: resume it automatically, or why it no longer will.
    AutoResume(&'a AutoResume),
    /// Turns before the newest failed, whatever the newest is doing.
    EarlierFailures(&'a [EarlierFailure]),
    /// A turn left processes running when it ended, which agentmux signalled to stop, so work in
    /// them may be incomplete.
    LeftRunning(&'a TurnLeftRunning),
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
        if let Some(resume) = &self.auto_resume {
            warnings.push(Warning::AutoResume(resume));
        }
        if !self.earlier_failures.is_empty() {
            warnings.push(Warning::EarlierFailures(&self.earlier_failures));
        }
        warnings.extend(self.left_running.iter().map(Warning::LeftRunning));
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

    /// The usage window that is refusing requests, which is the one a retry after a rate limit
    /// can be timed by.
    ///
    /// A window with room left resets on its own schedule, and timing a retry by it would wait
    /// for nothing.
    #[must_use]
    pub fn exhausted_window(&self) -> Option<&UsageWindow> {
        self.usage_window.as_ref().filter(|window| window.exhausted)
    }
}

/// What a turn left running, in one line, such as `turn 2 left 3 processes running outside the
/// delegate CLI's process tree after the CLI exited; agentmux signalled them to stop, and 2 are no
/// longer running and 1 still is — work in them may be incomplete`.
#[must_use]
pub fn left_running_line(left_running: &TurnLeftRunning) -> String {
    let TurnLeftRunning { turn, report } = left_running;
    let them = report.pronoun();
    format!(
        "turn {turn} left {} running outside the delegate CLI's process tree after the CLI \
         exited; agentmux signalled {them} to stop, and {} — work in {them} may be incomplete",
        report.processes_phrase(),
        report.fates(),
    )
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

/// What agentmux does, or did, about a turn that failed because the model was overloaded, in one
/// line both front ends print verbatim.
#[must_use]
pub fn auto_resume_line(resume: &AutoResume) -> String {
    match resume {
        AutoResume::Scheduled {
            turn,
            attempt,
            attempts,
            due_at,
        } => {
            let remaining = (*due_at - Utc::now()).to_std().map_or_else(
                |_| "now".to_owned(),
                |remaining| format!("in {}", duration(remaining)),
            );
            format!(
                "agentmux resumes this automatically at {} ({remaining}) as turn {turn}, sending \
                 \"{AUTO_RESUME_QUESTION}\" (attempt {attempt} of {attempts}); `cancel` stops it",
                due_at.to_rfc3339()
            )
        }
        AutoResume::Resumed {
            turn,
            attempt,
            attempts,
        } => format!(
            "agentmux resumed this automatically as turn {turn}, sending \
             \"{AUTO_RESUME_QUESTION}\" (attempt {attempt} of {attempts})"
        ),
        AutoResume::Unknown { turn, reason } => format!(
            "turn {turn} has begun since, but whether agentmux sent it as an automatic resume \
             cannot be read: {reason}"
        ),
        AutoResume::Cancelled { attempt, attempts } => format!(
            "the automatic resume (attempt {attempt} of {attempts}) was cancelled, so nothing more \
             runs unless you follow up"
        ),
        AutoResume::Abandoned {
            due_at,
            supervisor_log,
        } => format!(
            "the automatic resume due at {} did not happen: the agentmux supervisor that was to \
             send it has gone; its log is {}",
            due_at.to_rfc3339(),
            supervisor_log.display()
        ),
        AutoResume::Exhausted { attempts } => format!(
            "agentmux resumed this automatically {attempts} times in a row and the model was \
             overloaded every time, so it has stopped resuming"
        ),
    }
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

/// How many per-turn lines [`spend_lines`] prints at most, newest kept.
const MAX_SPEND_LINES: usize = 10;

/// What the consultation spent across all its turns, as one line.
///
/// Says so when nothing was reported, rather than leaving the line blank, because a vendor that
/// reports no figure and a turn that spent nothing would otherwise read the same.
#[must_use]
pub fn usage_line(status: &RunStatus) -> String {
    let spend = &status.spend;
    // Each total says for itself whether it is all that was spent: a price that could not be
    // attributed says nothing about the tokens beside it.
    let tokens = tokens_text(&status.usage)
        .map(|tokens| format!("{}{tokens}", at_least(spend.tokens_incomplete)));
    // Labelled because the figure is token counts priced at list: on a subscription account
    // nothing of the sort is charged, and a bare dollar amount reads as money that was billed.
    let price = price_text(status.cost_usd, spend.price_incomplete, |cost| {
        format!("${cost:.4} list price")
    });
    let totals = match (tokens, price) {
        (None, None) if status.is_terminal() => return "nothing reported".to_owned(),
        (None, None) => return "nothing reported yet".to_owned(),
        (Some(tokens), Some(price)) => format!("run total, all turns: {tokens} · {price}"),
        (Some(tokens), None) => format!("run total, all turns: {tokens} · no price reported"),
        (None, Some(price)) => format!("run total, all turns: no tokens reported · {price}"),
    };
    let unreported = u32::try_from(spend.unreported_turns.len()).unwrap_or(u32::MAX);
    match unreported_text(unreported) {
        Some(unreported) => format!("{totals} · {unreported}"),
        None => totals,
    }
}

/// How many turns the totals leave out because they reported nothing, or `None` for none.
fn unreported_text(count: u32) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 turn unreported".to_owned()),
        count => Some(format!("{count} turns unreported")),
    }
}

/// A price total as `priced` writes it, qualified when it is not all that was spent, or
/// `price unknown` when a price was reported and none could be counted.
fn price_text(
    cost_usd: Option<f64>,
    incomplete: bool,
    priced: impl Fn(f64) -> String,
) -> Option<String> {
    match cost_usd {
        Some(cost) => Some(format!("{}{}", at_least(incomplete), priced(cost))),
        None if incomplete => Some("price unknown".to_owned()),
        None => None,
    }
}

/// The turns a line of [`spend_lines`] covers, ordered newest last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Span {
    last_turn: u32,
    first_turn: u32,
}

/// What one line of [`spend_lines`] says about its turns.
#[derive(Debug, Default)]
struct SpanFacts<'a> {
    tokens: Option<&'a Spend<Tokens>>,
    price: Option<&'a Spend<f64>>,
    price_unknown: bool,
    unreported: bool,
}

/// What each turn, or each run of turns a running total covered, spent, one line each, newest
/// last, when there is more than one line to show.
///
/// A token and a price figure covering the same turns share a line.
/// At most [`MAX_SPEND_LINES`] are shown, after a line saying how many earlier turns are not.
#[must_use]
pub fn spend_lines(status: &RunStatus) -> Vec<String> {
    let spend = &status.spend;
    let mut spans: BTreeMap<Span, SpanFacts<'_>> = BTreeMap::new();
    let single = |turn: u32| Span {
        last_turn: turn,
        first_turn: turn,
    };
    for tokens in &spend.tokens {
        spans
            .entry(Span {
                last_turn: tokens.last_turn,
                first_turn: tokens.first_turn,
            })
            .or_default()
            .tokens = Some(tokens);
    }
    for price in &spend.prices {
        spans
            .entry(Span {
                last_turn: price.last_turn,
                first_turn: price.first_turn,
            })
            .or_default()
            .price = Some(price);
    }
    for turn in &spend.unpriced_turns {
        spans.entry(single(*turn)).or_default().price_unknown = true;
    }
    for turn in &spend.unreported_turns {
        spans.entry(single(*turn)).or_default().unreported = true;
    }
    if spans.len() < 2 {
        return Vec::new();
    }

    let shown = spans
        .iter()
        .skip(spans.len().saturating_sub(MAX_SPEND_LINES));
    let mut lines = Vec::new();
    let earliest_shown = shown.clone().map(|(span, _)| span.first_turn).min();
    if let Some(hidden) = earliest_shown
        .map(|turn| turn.saturating_sub(1))
        .filter(|hidden| *hidden > 0)
    {
        lines.push(format!("… {hidden} earlier turn(s)"));
    }
    for (span, facts) in shown {
        let turns = if span.first_turn == span.last_turn {
            format!("turn {}", span.last_turn)
        } else {
            format!("turns {}–{}", span.first_turn, span.last_turn)
        };
        let mut parts = Vec::new();
        if let Some(tokens) = facts.tokens
            && let Some(text) = tokens_text(&tokens.amount)
        {
            parts.push(format!("{}{text}", at_least(tokens.incomplete)));
        }
        if let Some(price) = facts.price {
            parts.push(format!(
                "{}${:.4}",
                at_least(price.incomplete),
                price.amount
            ));
        }
        if facts.price_unknown {
            parts.push("price unknown".to_owned());
        }
        if facts.unreported {
            parts.push("nothing reported".to_owned());
        }
        lines.push(format!("{turns}: {}", parts.join(" · ")));
    }
    lines
}

fn at_least(incomplete: bool) -> &'static str {
    if incomplete { "at least " } else { "" }
}

/// A token figure in full, or `None` when it knows neither the input nor the output.
///
/// The parts of a count, such as cached input or reasoning, are left out when zero or unknown.
fn tokens_text(tokens: &Tokens) -> Option<String> {
    let input = tokens.input_tokens.map(|input| {
        let cached = parenthesised([
            counted(tokens.cached_input_tokens, "cached"),
            counted(tokens.cache_write_input_tokens, "cache write"),
        ]);
        format!("{input} in{cached}")
    });
    let output = tokens.output_tokens.map(|output| {
        let reasoning = parenthesised([counted(tokens.reasoning_output_tokens, "reasoning")]);
        format!("{output} out{reasoning}")
    });
    match (input, output) {
        (None, None) => None,
        (Some(input), Some(output)) => Some(format!("{input} / {output}")),
        (Some(only), None) | (None, Some(only)) => Some(only),
    }
}

/// A part of a token count, such as `300 cached`, or `None` when it is zero or unknown.
fn counted(count: Option<u64>, name: &str) -> Option<String> {
    count
        .filter(|count| *count > 0)
        .map(|count| format!("{count} {name}"))
}

/// The parts that are there, as ` (a, b)`, or nothing when none is.
fn parenthesised(parts: impl IntoIterator<Item = Option<String>>) -> String {
    let parts: Vec<String> = parts.into_iter().flatten().collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

/// A consultation's run totals in the few characters a `list` line has room for, such as
/// `12.3M in / 45k out · $0.12`, or `-` when nothing was reported.
///
/// Qualified as [`usage_line`] qualifies them: `at least` before a total that is not all that was
/// spent, `price unknown` for a price that could not be counted, and how many turns reported
/// nothing.
#[must_use]
pub fn usage_column(totals: &Totals) -> String {
    let Totals {
        usage,
        cost_usd,
        tokens_incomplete,
        price_incomplete,
        unreported_turn_count,
    } = totals;
    let mut parts = Vec::new();
    let tokens: Vec<String> = [
        usage
            .input_tokens
            .map(|input| format!("{} in", compact_count(input))),
        usage
            .output_tokens
            .map(|output| format!("{} out", compact_count(output))),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !tokens.is_empty() {
        parts.push(format!(
            "{}{}",
            at_least(*tokens_incomplete),
            tokens.join(" / ")
        ));
    }
    parts.extend(price_text(*cost_usd, *price_incomplete, |cost| {
        format!("${cost:.2}")
    }));
    // A consultation that reported nothing at all is a dash, unreported turns or not.
    if parts.is_empty() {
        return "-".to_owned();
    }
    parts.extend(unreported_text(*unreported_turn_count));
    parts.join(" · ")
}

/// A count to one decimal of its thousands, millions or billions, rounded down: `45k`, `12.3M`.
fn compact_count(count: u64) -> String {
    let (unit, suffix) = match count {
        0..1_000 => return count.to_string(),
        1_000..1_000_000 => (1_000, "k"),
        1_000_000..1_000_000_000 => (1_000_000, "M"),
        _ => (1_000_000_000, "G"),
    };
    let tenths = count / (unit / 10);
    match tenths % 10 {
        0 => format!("{}{suffix}", tenths / 10),
        fraction => format!("{}.{fraction}{suffix}", tenths / 10),
    }
}

/// Where an account's usage window stands.
#[must_use]
pub fn usage_window_line(limit: &UsageWindow) -> String {
    let window = limit.window.as_deref().unwrap_or("usage");
    let remaining = limit.resets_in(Utc::now());
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
