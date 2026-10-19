//! The usage windows codex records in a session rollout, the one place it reports them for a turn
//! run with `codex exec`.
//!
//! A rollout is `$CODEX_HOME/sessions/**/rollout-*<thread-id>.jsonl`, one [`RolloutItem`] per line.
//! codex publishes no schema for it, so this mirrors the part agentmux reads by hand: a token count
//! event's rate-limit snapshot, from `codex-rs/history` (`RolloutLine`, `RolloutItem`) and
//! `codex-rs/protocol` (`EventMsg`, `TokenCountEvent`, `RateLimitSnapshot`).
//! The oracle in `codex-app-server-codegen/oracle` converts codex's snapshot into this one, naming
//! every field codex has, and reads codex's own rollout lines back as these types.
//!
//! It only reads, and reads leniently: every other kind of line is [`RolloutItem::Other`], every
//! other event is [`EventMsg::Other`], and every field codex has that agentmux does not read is
//! left out.

use serde::Deserialize;

/// One line of a rollout.
///
/// codex writes the kind beside a `payload`, which is the kind's own value.
/// It is read as an internally tagged enum, not an adjacently tagged one, because serde reads an
/// adjacently tagged fallback only from a line whose payload is empty.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RolloutItem {
    /// An event codex emitted during the session.
    EventMsg {
        /// The event.
        payload: EventMsg,
    },
    /// Anything else a rollout records.
    #[serde(other)]
    Other,
}

/// An event codex emitted during the session.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    /// The token count after a model response, with the account's usage windows as the response
    /// reported them.
    TokenCount(TokenCountEvent),
    /// Any other event.
    #[serde(other)]
    Other,
}

/// A token count event.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TokenCountEvent {
    /// The account's usage windows, when the response reported them.
    pub rate_limits: Option<RateLimitSnapshot>,
}

/// One bucket of the account's usage windows.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RateLimitSnapshot {
    /// The bucket's identifier, such as `codex`.
    pub limit_id: Option<String>,
    /// The shorter window, such as five hours, or the only one.
    pub primary: Option<RateLimitWindow>,
    /// The longer window, such as a week, when the plan has two.
    pub secondary: Option<RateLimitWindow>,
}

/// One usage window.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RateLimitWindow {
    /// How much of the window is used, from 0 to 100.
    pub used_percent: f64,
    /// How long the window is, in minutes.
    pub window_minutes: Option<i64>,
    /// When the window reopens, in seconds since the Unix epoch.
    pub resets_at: Option<i64>,
}

impl RateLimitWindow {
    /// Whether the window is used up, so a request against it is refused until it reopens.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.used_percent >= 100.0
    }
}
