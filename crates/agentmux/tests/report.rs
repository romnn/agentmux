//! What both front ends say about a consultation, phrased once in `agentmux::report`.

use agentmux::report::rate_limit_line;
use agentmux::transcript::RateLimit;
use chrono::Utc;
use googletest::prelude::*;

fn window(exhausted: bool, used_percent: Option<f64>, hours_left: i64) -> RateLimit {
    RateLimit {
        resets_at: Utc::now() + chrono::Duration::hours(hours_left),
        window: Some("codex".to_owned()),
        exhausted,
        used_percent,
    }
}

/// A window with room left says how much is used and when it resets, never that it reopens.
///
/// "Reopens" reads as "closed until then", and a caller told that of a week-long window that is
/// a quarter used stops sending work to an account that can still answer.
#[gtest]
fn a_window_with_room_left_does_not_read_as_closed() {
    let line = rate_limit_line(&window(false, Some(28.0), 157));
    assert_that!(line, starts_with("codex window 28% used, resets in 15"));
    assert_that!(line, not(contains_substring("reopen")));
    assert_that!(line, not(contains_substring("exhausted")));

    // A vendor that reports no figure is still not called closed.
    let line = rate_limit_line(&window(false, None, 157));
    assert_that!(line, starts_with("codex window resets in 15"));
}

/// Only an exhausted window is said to reopen, and once it has, the line says so.
#[gtest]
fn an_exhausted_window_says_when_it_reopens() {
    let line = rate_limit_line(&window(true, Some(100.0), 3));
    assert_that!(line, starts_with("codex window exhausted, reopens in 2h"));

    let line = rate_limit_line(&window(true, Some(100.0), -1));
    assert_that!(line, eq("codex window has reopened"));
}
