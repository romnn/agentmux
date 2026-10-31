//! What a consultation spent, derived from what each vendor reported.
//!
//! Changes when how a vendor reports spend changes.
//!
//! A turn keeps the vendor's figures exactly as they were reported, in a [`SpendReport`], and its
//! footer quotes them so; nothing here changes that evidence.
//! What the figures mean depends on who reported them, and this module is the one place that
//! knows:
//!
//! - Codex's tokens are its thread's running total, which a resumed thread carries on from, and
//!   it reports no price.
//! - Claude's tokens cover the one call.
//!   Its `total_cost_usd` covered the one call until Claude Code 2.1.277, and has been the
//!   session's running total across `--resume` since.
//!   A price whose release cannot be read has no known scope, so it is not counted rather than
//!   guessed at.
//!
//! Codex counts cache reads inside its prompt tokens, and Claude counts cache reads and writes
//! beside them, so a [`Tokens`] figure counts every prompt token, with the cached ones as a part of
//! it, and counts reasoning as a part of the output, as both vendors do.
//!
//! # Deriving spend
//!
//! Tokens and price are independent measures, each with its own running baseline, because one
//! vendor can report them in different scopes.
//! The turns are read in order, and for each measure:
//!
//! - A report of nothing at all is no report: Codex writes zeros for a turn it saw no usage for,
//!   and Claude for a run that crashed, and neither says what was spent.
//!   It neither counts nor moves the baseline.
//! - A turn that asked to resume a session and opened a new one starts a new session, so every
//!   running baseline is cleared before the turn is read, whether or not it reports anything.
//! - A figure for the one turn is that turn's spend.
//! - A running total with no baseline is the session's whole spend so far, and one at or above
//!   the baseline spent the difference.
//!   One below the baseline means the vendor restarted its counter inside the session: the new
//!   figure is counted, and marked incomplete, because what came before the restart is missing.
//!   Either way the total becomes the baseline, and what it counted covers every turn since the
//!   measure's previous report, so a turn that reported nothing is covered by the next one that
//!   does.
//!   A count a report leaves out keeps its last known baseline until a restart or a new session,
//!   so the report that brings it back is taken apart against that.
//! - A running total that follows figures for single turns in the same session, as a Claude
//!   upgraded across 2.1.277 mid-consultation reports, would count those turns again, so it is
//!   not counted; it only becomes the baseline.
//!   The same holds after a figure of unknown scope, which may be part of it.
//!
//! A turn no report covers, such as a running turn or a failed Codex turn nothing followed, is
//! listed as unreported.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::delegate::{Vendor, release_of};
use crate::transcript::{SpendReport, Turn, Usage};

/// The first Claude Code release whose `total_cost_usd` is the session's running total across
/// `--resume` rather than the one call's, as Anthropic's cost-tracking documentation says.
const CLAUDE_SESSION_COST_SINCE: (u64, u64, u64) = (2, 1, 277);

/// Token counts in one set of categories whatever the vendor.
///
/// Every field is optional, because a count no report carried is unknown, which is not zero.
/// Serialised under the names [`Usage`] uses, so a reader of either finds the same keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[expect(
    clippy::struct_field_names,
    reason = "the `_tokens` suffix is load-bearing and matches `Usage`'s serialised names, which \
              `status --json` readers already address"
)]
pub struct Tokens {
    /// Every prompt token, those read from or written to the cache included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Prompt tokens read from the cache, a part of `input_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Prompt tokens written to the cache, a part of `input_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
    /// Every token the model emitted, reasoning included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Reasoning tokens, a part of `output_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
}

impl Tokens {
    /// What a vendor's figures count, in these categories.
    fn reported(vendor: Vendor, usage: &Usage) -> Self {
        let Usage {
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            reasoning_output_tokens,
        } = *usage;
        let input_tokens = match vendor {
            Vendor::Codex => input_tokens,
            // Claude counts the cached prompt tokens beside its `input_tokens`, not inside it.
            Vendor::Claude => input_tokens.map(|uncached| {
                uncached
                    .saturating_add(cached_input_tokens.unwrap_or(0))
                    .saturating_add(cache_write_input_tokens.unwrap_or(0))
            }),
        };
        Self {
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            reasoning_output_tokens,
        }
    }

    /// Whether no count is known.
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        self.counts().iter().all(Option::is_none)
    }

    fn counts(self) -> [Option<u64>; 5] {
        [
            self.input_tokens,
            self.cached_input_tokens,
            self.cache_write_input_tokens,
            self.output_tokens,
            self.reasoning_output_tokens,
        ]
    }

    /// Each count of `self` and `other` combined by `combine`.
    fn zip_with(
        self,
        other: Self,
        combine: impl Fn(Option<u64>, Option<u64>) -> Option<u64>,
    ) -> Self {
        Self {
            input_tokens: combine(self.input_tokens, other.input_tokens),
            cached_input_tokens: combine(self.cached_input_tokens, other.cached_input_tokens),
            cache_write_input_tokens: combine(
                self.cache_write_input_tokens,
                other.cache_write_input_tokens,
            ),
            output_tokens: combine(self.output_tokens, other.output_tokens),
            reasoning_output_tokens: combine(
                self.reasoning_output_tokens,
                other.reasoning_output_tokens,
            ),
        }
    }

    /// Both added up, where a count either knows is known.
    fn plus(self, other: Self) -> Self {
        self.zip_with(other, |left, right| match (left, right) {
            (None, None) => None,
            (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
        })
    }
}

/// An amount a vendor reports, which a running total can be taken apart into.
trait Measure: Copy {
    /// Whether a report of this amount says nothing was measured.
    fn is_nothing(&self) -> bool;
    /// Whether this running total is lower than `baseline`, which only a restarted counter is.
    fn fell_below(&self, baseline: &Self) -> bool;
    /// What this running total added to `baseline`.
    fn since(&self, baseline: &Self) -> Self;
    /// This running total as the next baseline, keeping from `baseline` what it does not report.
    ///
    /// A count one report leaves out has not stopped running, so the next report of it is taken
    /// apart against the last value known, and covers the reports that left it out.
    fn kept_over(&self, baseline: &Self) -> Self;
}

impl Measure for Tokens {
    fn is_nothing(&self) -> bool {
        self.counts().iter().all(|count| count.unwrap_or(0) == 0)
    }

    fn fell_below(&self, baseline: &Self) -> bool {
        self.counts().iter().zip(baseline.counts()).any(
            |(count, base)| matches!((count, base), (Some(count), Some(base)) if *count < base),
        )
    }

    fn since(&self, baseline: &Self) -> Self {
        self.zip_with(*baseline, |count, base| {
            count.map(|count| count.saturating_sub(base.unwrap_or(0)))
        })
    }

    fn kept_over(&self, baseline: &Self) -> Self {
        self.zip_with(*baseline, Option::or)
    }
}

/// A price in US dollars.
impl Measure for f64 {
    fn is_nothing(&self) -> bool {
        !self.is_finite() || *self <= 0.0
    }

    fn fell_below(&self, baseline: &Self) -> bool {
        self < baseline
    }

    fn since(&self, baseline: &Self) -> Self {
        self - baseline
    }

    fn kept_over(&self, _baseline: &Self) -> Self {
        *self
    }
}

/// What a vendor's figure covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// What the one turn spent.
    Turn,
    /// The session's running total.
    Session,
}

/// What a vendor's token figures cover.
fn token_scope(vendor: Vendor) -> Scope {
    match vendor {
        Vendor::Codex => Scope::Session,
        Vendor::Claude => Scope::Turn,
    }
}

/// What a vendor's price covers, or `None` when that cannot be known.
fn price_scope(vendor: Vendor, cli_version: Option<&str>) -> Option<Scope> {
    match vendor {
        // Codex reports no price, so a price it did report would be in a scope nothing documents.
        Vendor::Codex => None,
        Vendor::Claude => {
            let release = cli_version.and_then(release_of)?;
            Some(if release < CLAUDE_SESSION_COST_SINCE {
                Scope::Turn
            } else {
                Scope::Session
            })
        }
    }
}

/// What a run of turns spent of one measure, by the turns it covers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Spend<T> {
    /// The first turn the amount covers, counted from 1 as the transcript's headings count.
    pub first_turn: u32,
    /// The last turn the amount covers.
    ///
    /// Later than [`Spend::first_turn`] when the turns before it reported nothing of the measure
    /// and a running total covered them.
    pub last_turn: u32,
    /// What those turns spent.
    pub amount: T,
    /// Whether those turns spent more than the amount: the vendor restarted its counter inside
    /// the session, and what was spent before the restart is missing.
    pub incomplete: bool,
}

/// One measure's spends, and what its next running total is to be taken apart against.
struct Ledger<T> {
    spends: Vec<Spend<T>>,
    /// The session's running total at the latest report, when that report was one.
    baseline: Option<T>,
    /// Whether the session spent something a running total would include and `baseline` does not.
    unaccounted: bool,
    /// The first turn the next running total covers.
    next_turn: u32,
}

impl<T: Measure> Ledger<T> {
    fn new() -> Self {
        Self {
            spends: Vec::new(),
            baseline: None,
            unaccounted: false,
            next_turn: 1,
        }
    }

    /// Count from `turn` on as a new session, which owes nothing to the one before.
    fn new_session(&mut self, turn: u32) {
        self.baseline = None;
        self.unaccounted = false;
        self.next_turn = turn;
    }

    /// Note a report at `turn` whose scope is unknown, which a later running total may include.
    fn lose_track(&mut self, turn: u32) {
        self.baseline = None;
        self.unaccounted = true;
        self.next_turn = turn.saturating_add(1);
    }

    /// Count what the report at `turn` says, or return `false` when it cannot be attributed.
    fn record(&mut self, turn: u32, amount: T, scope: Scope) -> bool {
        let first_turn = std::mem::replace(&mut self.next_turn, turn.saturating_add(1));
        let spend = match scope {
            Scope::Turn => {
                // A running total reported later in this session would count this turn again.
                self.baseline = None;
                self.unaccounted = true;
                Some(Spend {
                    first_turn: turn,
                    last_turn: turn,
                    amount,
                    incomplete: false,
                })
            }
            Scope::Session => {
                let unaccounted = std::mem::replace(&mut self.unaccounted, false);
                match self.baseline {
                    // Restarted inside the session, so no earlier value is a baseline any more,
                    // not even of a count this report leaves out.
                    Some(baseline) if amount.fell_below(&baseline) => {
                        self.baseline = Some(amount);
                        Some(Spend {
                            first_turn,
                            last_turn: turn,
                            amount,
                            incomplete: true,
                        })
                    }
                    Some(baseline) => {
                        self.baseline = Some(amount.kept_over(&baseline));
                        Some(Spend {
                            first_turn,
                            last_turn: turn,
                            amount: amount.since(&baseline),
                            incomplete: false,
                        })
                    }
                    None => {
                        self.baseline = Some(amount);
                        (!unaccounted).then_some(Spend {
                            first_turn,
                            last_turn: turn,
                            amount,
                            incomplete: false,
                        })
                    }
                }
            }
        };
        let attributed = spend.is_some();
        self.spends.extend(spend);
        attributed
    }
}

/// What a consultation spent, turn by turn, derived from its vendor's reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Accounting {
    /// What each turn, or each run of turns a running total covered, spent in tokens, oldest
    /// first.
    pub tokens: Vec<Spend<Tokens>>,
    /// What each turn, or each run of turns a running total covered, spent in US dollars at list
    /// price, oldest first.
    pub prices: Vec<Spend<f64>>,
    /// The turns whose reported price could not be attributed to them, and so is not counted.
    pub unpriced_turns: Vec<u32>,
    /// The turns no report covers, such as one still running.
    pub unreported_turns: Vec<u32>,
    /// Whether the token total is less than was spent, because a report could not be counted
    /// whole.
    pub tokens_incomplete: bool,
    /// Whether the price total is less than was spent, because a report could not be counted
    /// whole or a turn's price could not be attributed.
    pub price_incomplete: bool,
}

/// What a consultation spent across all its turns, and whether that is all it spent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Totals {
    /// The run total across all turns, in tokens.
    pub usage: Tokens,
    /// The run total across all turns, in US dollars at list price, or `None` when no price was
    /// counted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// As [`Accounting::tokens_incomplete`] says.
    pub tokens_incomplete: bool,
    /// As [`Accounting::price_incomplete`] says.
    pub price_incomplete: bool,
}

impl Accounting {
    /// Derive what `turns`, all of one consultation with `vendor`, spent.
    #[must_use]
    pub fn of(vendor: Vendor, turns: &[Turn]) -> Self {
        let mut tokens = Ledger::<Tokens>::new();
        let mut prices = Ledger::<f64>::new();
        let mut unpriced_turns = Vec::new();
        let mut tokens_incomplete = false;
        // Turns with a report that could not be attributed, which are reported all the same.
        let mut unattributed = BTreeSet::new();

        for turn in turns {
            let number = turn.index.saturating_add(1);
            if turn.broke_continuity {
                tokens.new_session(number);
                prices.new_session(number);
            }
            let Some(SpendReport {
                usage,
                cost_usd,
                cli_version,
            }) = &turn.spend_report
            else {
                continue;
            };

            let reported_tokens = Tokens::reported(vendor, usage);
            // Token scopes are fixed per vendor, so no token figure is refused today; one that
            // were refused would still be a figure the totals leave out.
            if !reported_tokens.is_nothing()
                && !tokens.record(number, reported_tokens, token_scope(vendor))
            {
                unattributed.insert(number);
                tokens_incomplete = true;
            }

            if let Some(price) = cost_usd.filter(|price| !price.is_nothing()) {
                let attributed = if let Some(scope) = price_scope(vendor, cli_version.as_deref()) {
                    prices.record(number, price, scope)
                } else {
                    prices.lose_track(number);
                    false
                };
                if !attributed {
                    unpriced_turns.push(number);
                }
            }
        }

        let covered: BTreeSet<u32> = spans(&tokens.spends)
            .chain(spans(&prices.spends))
            .chain(unpriced_turns.iter().copied())
            .chain(unattributed)
            .collect();
        let unreported_turns = turns
            .iter()
            .map(|turn| turn.index.saturating_add(1))
            .filter(|number| !covered.contains(number))
            .collect();
        tokens_incomplete |= tokens.spends.iter().any(|spend| spend.incomplete);
        let price_incomplete =
            !unpriced_turns.is_empty() || prices.spends.iter().any(|spend| spend.incomplete);

        Self {
            tokens: tokens.spends,
            prices: prices.spends,
            unpriced_turns,
            unreported_turns,
            tokens_incomplete,
            price_incomplete,
        }
    }

    /// The run totals across all turns, and whether each is all that was spent.
    #[must_use]
    pub fn totals(&self) -> Totals {
        Totals {
            usage: self.total(),
            cost_usd: self.total_cost_usd(),
            tokens_incomplete: self.tokens_incomplete,
            price_incomplete: self.price_incomplete,
        }
    }

    /// The run total across all turns, in tokens.
    #[must_use]
    pub fn total(&self) -> Tokens {
        self.tokens
            .iter()
            .fold(Tokens::default(), |total, spend| total.plus(spend.amount))
    }

    /// The run total across all turns, in US dollars at list price, or `None` when no price was
    /// counted.
    #[must_use]
    pub fn total_cost_usd(&self) -> Option<f64> {
        (!self.prices.is_empty()).then(|| self.prices.iter().map(|spend| spend.amount).sum())
    }
}

/// Every turn the spends cover.
fn spans<T>(spends: &[Spend<T>]) -> impl Iterator<Item = u32> + '_ {
    spends
        .iter()
        .flat_map(|spend| spend.first_turn..=spend.last_turn)
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::{Accounting, Spend, Tokens};
    use crate::delegate::Vendor;
    use crate::transcript::{SpendReport, Turn, Usage};

    /// A Claude release whose price covers the one call.
    const PER_TURN_PRICE: &str = "2.1.261";
    /// A Claude release whose price is the session's running total.
    const SESSION_PRICE: &str = "2.1.277 (Claude Code)";

    /// Turns numbered from 1 in order, each reporting what `reports` holds for it.
    fn turns(reports: impl IntoIterator<Item = Option<SpendReport>>) -> Vec<Turn> {
        reports
            .into_iter()
            .zip(0..)
            .map(|(report, index)| {
                let mut turn = Turn::new(index, "q");
                turn.spend_report = report;
                turn
            })
            .collect()
    }

    /// A Codex report of its thread's running total.
    fn codex(input: u64, output: u64) -> SpendReport {
        SpendReport {
            usage: Usage {
                input_tokens: Some(input),
                cached_input_tokens: Some(0),
                cache_write_input_tokens: Some(0),
                output_tokens: Some(output),
                reasoning_output_tokens: Some(0),
            },
            cost_usd: None,
            cli_version: Some("codex-cli 0.163.0".to_owned()),
        }
    }

    /// A Claude report of one call's tokens and a price, from `release`.
    fn claude(input: u64, price: f64, release: Option<&str>) -> SpendReport {
        SpendReport {
            usage: Usage {
                input_tokens: Some(input),
                cached_input_tokens: Some(0),
                cache_write_input_tokens: Some(0),
                output_tokens: Some(1),
                reasoning_output_tokens: None,
            },
            cost_usd: Some(price),
            cli_version: release.map(ToOwned::to_owned),
        }
    }

    fn input(spend: &Spend<Tokens>) -> Option<u64> {
        spend.amount.input_tokens
    }

    /// Codex's running totals are taken apart into what each turn added.
    #[gtest]
    fn codex_running_totals_become_per_turn_spend() {
        let accounting = Accounting::of(
            Vendor::Codex,
            &turns([Some(codex(100, 10)), Some(codex(160, 15))]),
        );

        assert_that!(
            accounting.tokens,
            elements_are![
                matches_pattern!(Spend {
                    first_turn: eq(&1),
                    last_turn: eq(&1),
                    amount: matches_pattern!(Tokens {
                        input_tokens: some(eq(&100)),
                        output_tokens: some(eq(&10)),
                        ..
                    }),
                    incomplete: eq(&false),
                }),
                matches_pattern!(Spend {
                    first_turn: eq(&2),
                    last_turn: eq(&2),
                    amount: matches_pattern!(Tokens {
                        input_tokens: some(eq(&60)),
                        output_tokens: some(eq(&5)),
                        ..
                    }),
                    incomplete: eq(&false),
                }),
            ]
        );
        // The total is the session's last running total, not the sum of every report.
        assert_that!(accounting.total().input_tokens, some(eq(160)));
        assert_that!(accounting.total_cost_usd(), none());
        assert_that!(accounting.unreported_turns, is_empty());
        assert_that!(accounting.tokens_incomplete, eq(false));
    }

    /// A failed turn that reported nothing is covered by the next running total, as one span.
    #[gtest]
    fn a_running_total_covers_the_turns_that_reported_nothing() {
        let accounting = Accounting::of(
            Vendor::Codex,
            &turns([Some(codex(100, 10)), None, Some(codex(250, 30))]),
        );

        assert_that!(
            accounting
                .tokens
                .iter()
                .map(|spend| (spend.first_turn, spend.last_turn, input(spend)))
                .collect::<Vec<_>>(),
            elements_are![eq(&(1, 1, Some(100))), eq(&(2, 3, Some(150)))]
        );
        assert_that!(accounting.unreported_turns, is_empty());
    }

    /// Codex's synthetic all-zero usage is no report, so the next total is not counted whole.
    #[gtest]
    fn an_all_zero_report_is_no_report() {
        let accounting = Accounting::of(
            Vendor::Codex,
            &turns([
                Some(codex(100, 10)),
                Some(codex(0, 0)),
                Some(codex(200, 20)),
            ]),
        );

        assert_that!(
            accounting
                .tokens
                .iter()
                .map(|spend| (spend.first_turn, spend.last_turn, input(spend)))
                .collect::<Vec<_>>(),
            elements_are![eq(&(1, 1, Some(100))), eq(&(2, 3, Some(100)))]
        );
        assert_that!(accounting.total().input_tokens, some(eq(200)));
        assert_that!(accounting.tokens_incomplete, eq(false));
    }

    /// A turn that opened a new session counts from zero again, even when it reports nothing
    /// itself.
    #[gtest]
    fn a_broken_continuity_starts_a_new_session() {
        let mut broken = turns([Some(codex(100, 10)), Some(codex(150, 12))]);
        if let Some(turn) = broken.get_mut(1) {
            turn.broke_continuity = true;
        }
        let accounting = Accounting::of(Vendor::Codex, &broken);
        assert_that!(
            accounting.tokens.iter().map(input).collect::<Vec<_>>(),
            elements_are![some(eq(&100)), some(eq(&150))]
        );

        // The break clears the baseline although the breaking turn itself reported nothing, so
        // the turn after it counts its total whole and covers only the new session's turns.
        let mut silent_break = turns([Some(codex(100, 10)), None, Some(codex(40, 4))]);
        if let Some(turn) = silent_break.get_mut(1) {
            turn.broke_continuity = true;
        }
        let accounting = Accounting::of(Vendor::Codex, &silent_break);
        assert_that!(
            accounting
                .tokens
                .iter()
                .map(|spend| (spend.first_turn, spend.last_turn, input(spend)))
                .collect::<Vec<_>>(),
            elements_are![eq(&(1, 1, Some(100))), eq(&(2, 3, Some(40)))]
        );
        assert_that!(accounting.tokens_incomplete, eq(false));
    }

    /// A count one running total leaves out keeps its baseline, so its next report is taken
    /// apart against the last value known rather than counted whole.
    #[gtest]
    fn a_count_left_out_of_one_report_keeps_its_baseline() {
        let report = |input: u64, cached: Option<u64>, output: u64| {
            Some(SpendReport {
                usage: Usage {
                    input_tokens: Some(input),
                    cached_input_tokens: cached,
                    cache_write_input_tokens: None,
                    output_tokens: Some(output),
                    reasoning_output_tokens: None,
                },
                cost_usd: None,
                cli_version: None,
            })
        };
        let accounting = Accounting::of(
            Vendor::Codex,
            &turns([
                report(100, Some(90), 10),
                report(150, None, 15),
                report(200, Some(180), 20),
            ]),
        );

        // The cached count's 180 covers turns 2 and 3 against turn 1's 90, so it stays a part of
        // the input.
        assert_that!(
            accounting.total(),
            eq(Tokens {
                input_tokens: Some(200),
                cached_input_tokens: Some(180),
                cache_write_input_tokens: None,
                output_tokens: Some(20),
                reasoning_output_tokens: None,
            })
        );
        assert_that!(accounting.tokens_incomplete, eq(false));
    }

    /// A running total that falls inside one session means the counter restarted, and what came
    /// before the restart is missing.
    #[gtest]
    fn a_counter_restarted_inside_the_session_is_incomplete() {
        let accounting = Accounting::of(
            Vendor::Codex,
            &turns([Some(codex(100, 10)), Some(codex(30, 3))]),
        );

        assert_that!(
            accounting.tokens.get(1),
            some(matches_pattern!(Spend {
                amount: matches_pattern!(Tokens {
                    input_tokens: some(eq(&30)),
                    ..
                }),
                incomplete: eq(&true),
                ..
            }))
        );
        assert_that!(accounting.total().input_tokens, some(eq(130)));
        assert_that!(accounting.tokens_incomplete, eq(true));
        assert_that!(accounting.price_incomplete, eq(false));
    }

    /// Claude's tokens are per call while a current release's price is the session's running
    /// total, so the two measures are taken apart independently.
    #[gtest]
    fn claude_tokens_per_turn_and_price_per_session() {
        let accounting = Accounting::of(
            Vendor::Claude,
            &turns([
                Some(claude(100, 0.10, Some(SESSION_PRICE))),
                Some(claude(50, 0.25, Some(SESSION_PRICE))),
            ]),
        );

        assert_that!(
            accounting.tokens.iter().map(input).collect::<Vec<_>>(),
            elements_are![some(eq(&100)), some(eq(&50))]
        );
        assert_that!(accounting.total().input_tokens, some(eq(150)));
        assert_that!(
            accounting
                .prices
                .iter()
                .map(|spend| spend.amount)
                .collect::<Vec<_>>(),
            elements_are![approx_eq(0.10), approx_eq(0.15)]
        );
        assert_that!(accounting.total_cost_usd(), some(approx_eq(0.25)));
        assert_that!(accounting.tokens_incomplete, eq(false));
        assert_that!(accounting.price_incomplete, eq(false));
    }

    /// An older release's price covers the one call, so it adds up turn by turn.
    #[gtest]
    fn an_older_claude_prices_each_turn() {
        let accounting = Accounting::of(
            Vendor::Claude,
            &turns([
                Some(claude(100, 0.10, Some(PER_TURN_PRICE))),
                Some(claude(50, 0.25, Some(PER_TURN_PRICE))),
            ]),
        );

        assert_that!(accounting.total_cost_usd(), some(approx_eq(0.35)));
        assert_that!(accounting.unpriced_turns, is_empty());
    }

    /// A price whose release cannot be read is not counted, and the turn says its price is
    /// unknown, while its tokens still count.
    #[gtest]
    fn a_price_of_unknown_scope_is_not_guessed() {
        let accounting = Accounting::of(
            Vendor::Claude,
            &turns([
                Some(claude(100, 0.10, None)),
                Some(claude(50, 0.25, Some("an unparsable version"))),
            ]),
        );

        assert_that!(accounting.prices, is_empty());
        assert_that!(accounting.total_cost_usd(), none());
        assert_that!(accounting.unpriced_turns, elements_are![eq(&1), eq(&2)]);
        assert_that!(accounting.total().input_tokens, some(eq(150)));
        assert_that!(accounting.unreported_turns, is_empty());
        assert_that!(accounting.price_incomplete, eq(true));
        assert_that!(accounting.tokens_incomplete, eq(false));
    }

    /// A Claude upgraded across 2.1.277 mid-consultation reports a running total that includes
    /// the turns already priced one by one, so that one total is not counted, and the next is
    /// taken apart against it.
    #[gtest]
    fn a_price_crossing_into_session_scope_is_not_counted_twice() {
        let accounting = Accounting::of(
            Vendor::Claude,
            &turns([
                Some(claude(100, 0.10, Some(PER_TURN_PRICE))),
                Some(claude(100, 0.30, Some(SESSION_PRICE))),
                Some(claude(100, 0.45, Some(SESSION_PRICE))),
            ]),
        );

        assert_that!(
            accounting
                .prices
                .iter()
                .map(|spend| (spend.first_turn, spend.last_turn))
                .collect::<Vec<_>>(),
            elements_are![eq(&(1, 1)), eq(&(3, 3))]
        );
        assert_that!(accounting.total_cost_usd(), some(approx_eq(0.25)));
        assert_that!(accounting.unpriced_turns, elements_are![eq(&2)]);
        assert_that!(accounting.price_incomplete, eq(true));
        assert_that!(accounting.tokens_incomplete, eq(false));
    }

    /// Tokens and price keep their own spans: a turn that reported tokens but no price leaves
    /// the next running price covering both turns, while the tokens stay per turn.
    #[gtest]
    fn tokens_and_price_keep_their_own_spans() {
        let mut unpriced = claude(50, 0.0, Some(SESSION_PRICE));
        unpriced.cost_usd = None;
        let accounting = Accounting::of(
            Vendor::Claude,
            &turns([
                Some(claude(100, 0.10, Some(SESSION_PRICE))),
                Some(unpriced),
                Some(claude(70, 0.40, Some(SESSION_PRICE))),
            ]),
        );

        assert_that!(
            accounting
                .tokens
                .iter()
                .map(|spend| (spend.first_turn, spend.last_turn))
                .collect::<Vec<_>>(),
            elements_are![eq(&(1, 1)), eq(&(2, 2)), eq(&(3, 3))]
        );
        assert_that!(
            accounting
                .prices
                .iter()
                .map(|spend| (spend.first_turn, spend.last_turn))
                .collect::<Vec<_>>(),
            elements_are![eq(&(1, 1)), eq(&(2, 3))]
        );
        assert_that!(accounting.total_cost_usd(), some(approx_eq(0.40)));
    }

    /// Claude counts cached prompt tokens beside its input, so the input is made inclusive and
    /// the cached counts become parts of it.
    #[gtest]
    fn claude_input_is_made_inclusive_of_the_cache() {
        let report = SpendReport {
            usage: Usage {
                input_tokens: Some(2),
                cached_input_tokens: Some(18_574),
                cache_write_input_tokens: Some(10_866),
                output_tokens: Some(128),
                reasoning_output_tokens: Some(116),
            },
            cost_usd: Some(0.01),
            cli_version: Some(PER_TURN_PRICE.to_owned()),
        };
        let accounting = Accounting::of(Vendor::Claude, &turns([Some(report)]));

        assert_that!(
            accounting.total(),
            eq(Tokens {
                input_tokens: Some(29_442),
                cached_input_tokens: Some(18_574),
                cache_write_input_tokens: Some(10_866),
                output_tokens: Some(128),
                reasoning_output_tokens: Some(116),
            })
        );
    }

    /// A turn nothing reported for and no later total covers, such as a trailing failed or a
    /// running turn, is listed as unreported, and a Claude crash's zeros are no report either.
    #[gtest]
    fn turns_no_report_covers_are_unreported() {
        let codex_run = Accounting::of(Vendor::Codex, &turns([Some(codex(100, 10)), None]));
        assert_that!(codex_run.unreported_turns, elements_are![eq(&2)]);

        let crashed = SpendReport {
            usage: Usage {
                reasoning_output_tokens: Some(0),
                ..Usage::default()
            },
            cost_usd: Some(0.0),
            cli_version: Some(PER_TURN_PRICE.to_owned()),
        };
        let claude_run = Accounting::of(
            Vendor::Claude,
            &turns([Some(crashed), Some(claude(10, 0.01, Some(PER_TURN_PRICE)))]),
        );
        assert_that!(claude_run.unreported_turns, elements_are![eq(&1)]);
        assert_that!(claude_run.total().input_tokens, some(eq(10)));

        let empty = Accounting::of(Vendor::Codex, &[]);
        assert_that!(empty.total().is_unknown(), eq(true));
        assert_that!(empty.unreported_turns, is_empty());
    }
}
