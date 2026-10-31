//! What both front ends say about a consultation, phrased once in `agentmux::report`.

use agentmux::accounting::{Tokens, Totals};
use agentmux::report::{usage_column, usage_window_line};
use agentmux::transcript::UsageWindow;
use chrono::Utc;
use googletest::prelude::*;

/// A Codex window resetting `hours_left` from now.
fn codex_window(hours_left: i64) -> UsageWindow {
    UsageWindow {
        resets_at: Utc::now() + chrono::Duration::hours(hours_left),
        window: Some("codex".to_owned()),
        exhausted: false,
        used_percent: None,
    }
}

/// A window with room left says how much is used and when it resets, never that it reopens.
///
/// "Reopens" reads as "closed until then", and a caller told that of a week-long window that is
/// a quarter used stops sending work to an account that can still answer.
#[gtest]
fn a_window_with_room_left_does_not_read_as_closed() {
    let line = usage_window_line(&UsageWindow {
        used_percent: Some(28.0),
        ..codex_window(157)
    });
    assert_that!(line, starts_with("codex window 28% used, resets in 15"));
    assert_that!(line, not(contains_substring("reopen")));
    assert_that!(line, not(contains_substring("exhausted")));

    // A vendor that reports no figure is still not called closed.
    let line = usage_window_line(&codex_window(157));
    assert_that!(line, starts_with("codex window resets in 15"));
}

/// Only an exhausted window is said to reopen, and once it has, the line says so.
#[gtest]
fn an_exhausted_window_says_when_it_reopens() {
    let line = usage_window_line(&UsageWindow {
        exhausted: true,
        used_percent: Some(100.0),
        ..codex_window(3)
    });
    assert_that!(line, starts_with("codex window exhausted, reopens in 2h"));

    let line = usage_window_line(&UsageWindow {
        exhausted: true,
        ..codex_window(-1)
    });
    assert_that!(line, eq("codex window has reopened"));
}

/// A listing's total fits a column: counts to one decimal of their unit, rounded down, the
/// price to the cent, and a dash for a consultation that reported nothing.
#[gtest]
fn a_listed_total_is_compact() {
    let usage = Tokens {
        input_tokens: Some(12_345_678),
        output_tokens: Some(45_000),
        ..Tokens::default()
    };
    assert_that!(
        usage_column(&Totals {
            usage,
            ..Totals::default()
        }),
        eq("12.3M in / 45k out")
    );
    assert_that!(
        usage_column(&Totals {
            usage: Tokens {
                input_tokens: Some(999),
                output_tokens: Some(1_999),
                ..Tokens::default()
            },
            cost_usd: Some(0.1234),
            ..Totals::default()
        }),
        eq("999 in / 1.9k out · $0.12")
    );
    assert_that!(usage_column(&Totals::default()), eq("-"));
}

/// A listed total that is not all that was spent says so as `status` does, each total for
/// itself: a counter restart qualifies the tokens alone, and a price that could not be counted
/// reads as unknown rather than as none.
#[gtest]
fn a_listed_total_says_what_it_leaves_out() {
    let usage = Tokens {
        input_tokens: Some(130),
        output_tokens: Some(13),
        ..Tokens::default()
    };
    assert_that!(
        usage_column(&Totals {
            usage,
            cost_usd: Some(0.25),
            tokens_incomplete: true,
            price_incomplete: false,
        }),
        eq("at least 130 in / 13 out · $0.25")
    );
    assert_that!(
        usage_column(&Totals {
            usage,
            cost_usd: Some(0.25),
            tokens_incomplete: false,
            price_incomplete: true,
        }),
        eq("130 in / 13 out · at least $0.25")
    );
    assert_that!(
        usage_column(&Totals {
            usage,
            cost_usd: None,
            tokens_incomplete: false,
            price_incomplete: true,
        }),
        eq("130 in / 13 out · price unknown")
    );
}
