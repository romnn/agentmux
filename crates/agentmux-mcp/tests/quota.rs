//! How an account's remaining quota reaches a calling model.
//!
//! The dangerous failure is not a wrong number but a missing one read as a good one: an account
//! that could not be asked must never look like an account with capacity to spare.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentmux::delegate::{CodexSandbox, Delegate, Effort, ModelId, Vendor};
use agentmux::quota::{AccountQuota, Observation, Origin, Refresh};
use agentmux::run::{HookReopening, Retention, RunStatus, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher};
use agentmux::transcript::{FailureKind, Outcome, RateLimit};
use chrono::Utc;
use googletest::prelude::*;

/// Drive one scripted consultation and return the status a tool would render.
fn consult(events: &str) -> Result<(RunStore, RunStatus, tempfile::TempDir)> {
    let dir = tempfile::tempdir().or_fail()?;
    let env: BTreeMap<String, String> = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([Script::completed(events)])),
        env,
    )
    .or_fail()?;
    let status = store
        .start(&StartRequest {
            delegate: Delegate::Codex {
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                effort: Some(Effort::parse("xhigh").or_fail()?),
                sandbox: CodexSandbox::ReadOnly,
                account: None,
                isolation: None,
            },
            question: "Review the diff.".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
        })
        .or_fail()?;
    Ok((store, status, dir))
}

/// An account that could not be asked must never render as an idle one.
///
/// This is the failure that turns a typo into "always route to the account that cannot answer": an
/// unauthenticated Claude directory reports zero usage cheerfully, so anything that reads silence
/// as capacity picks the broken account every time.
#[gtest]
fn an_unavailable_account_never_reads_as_spare_capacity() {
    let entry = AccountQuota {
        vendor: Vendor::Claude,
        account: None,
        description: None,
        observation: Observation::Unavailable {
            reason: "not logged in".to_owned(),
        },
    };

    let rendered = agentmux_mcp::render::quota_report(&entry).join("\n");

    assert_that!(rendered, contains_substring("unavailable"));
    assert_that!(rendered, contains_substring("not idle"));
    // No percentage anywhere, because there is no measurement to report.
    assert_that!(rendered, not(contains_substring("%")));
}

/// agentmux states no opinion about which account is better placed to answer.
///
/// Ranking would need a table of what each plan tier is worth, which is the roster this crate
/// refuses to keep.
/// The words below are the ones that would appear if that rule were broken.
#[gtest]
fn nothing_rendered_recommends_an_account() {
    let entry = AccountQuota {
        vendor: Vendor::Claude,
        account: None,
        description: None,
        observation: Observation::Reported {
            origin: Origin::Live,
            payload: serde_json::json!({
                "utilization": {"limits": [
                    {"kind": "weekly_all", "percent": 3, "severity": "normal"}
                ]}
            }),
        },
    };

    let rendered = agentmux_mcp::render::quota_report(&entry)
        .join("\n")
        .to_lowercase();

    for verdict in ["best", "recommend", "use this", "most capacity", "prefer"] {
        assert_that!(
            rendered.contains(verdict),
            eq(false),
            "the rendering recommends an account with {verdict:?}"
        );
    }
}

/// A rate limit with a working account elsewhere must not be told to switch vendor.
///
/// The advice and the list beneath it are read together, so advising a vendor switch while showing
/// a same-vendor account with capacity leaves the caller to resolve a contradiction, or to act on
/// the more expensive of two remedies.
#[gtest]
fn a_rate_limit_with_another_account_available_names_that_account() -> Result<()> {
    let (_store, mut status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"rate limit reached"}}
    "#})?;
    status.outcome = Outcome::Failed {
        kind: FailureKind::RateLimited,
        detail: "rate limit reached".to_owned(),
    };
    status.rate_limit = Some(RateLimit {
        resets_at: Utc::now() + chrono::Duration::hours(9),
        window: Some("seven_day".to_owned()),
    });
    status.quota = vec![
        reported(Vendor::Codex, "personal", 11)?,
        reported(Vendor::Codex, "work", 99)?,
    ];

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, contains_substring("personal"));
    assert_that!(warnings, contains_substring("pass one as `account`"));
    // The expensive remedy is no longer the headline when a cheap one exists.
    assert_that!(warnings, not(contains_substring("waiting is not worth it")));
    Ok(())
}

/// With no other account, the advice falls back to the window's own reopening time.
#[gtest]
fn a_rate_limit_with_no_alternative_still_advises_from_the_window() -> Result<()> {
    let (_store, mut status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"rate limit reached"}}
    "#})?;
    status.outcome = Outcome::Failed {
        kind: FailureKind::RateLimited,
        detail: "rate limit reached".to_owned(),
    };
    status.rate_limit = Some(RateLimit {
        resets_at: Utc::now() + chrono::Duration::hours(9),
        window: Some("seven_day".to_owned()),
    });

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, contains_substring("other vendor"));
    Ok(())
}

/// One account reporting a figure, for building a status by hand.
///
/// The vendor is a parameter because the advice must only ever name an account of the vendor that
/// failed: an alias from the other vendor's table resolves to nothing but a paid round trip.
fn reported(vendor: Vendor, alias: &str, percent: i64) -> Result<AccountQuota> {
    Ok(AccountQuota {
        vendor,
        account: Some(agentmux::delegate::AccountAlias::parse(alias).or_fail()?),
        description: None,
        observation: Observation::Reported {
            origin: Origin::Live,
            payload: serde_json::json!({
                "utilization": {"limits": [
                    {"kind": "weekly_all", "percent": percent, "severity": "normal"}
                ]}
            }),
        },
    })
}

/// A window that has already reopened must not send the caller to another subscription.
///
/// This is the case the alternatives branch got wrong when it was added: it preempted the window
/// advice entirely, so a failure that had already expired moved work onto a different identity to
/// solve a problem that no longer existed.
#[gtest]
fn a_reopened_window_is_retried_rather_than_routed_elsewhere() -> Result<()> {
    let (_store, mut status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"rate limit reached"}}
    "#})?;
    status.outcome = Outcome::Failed {
        kind: FailureKind::RateLimited,
        detail: "rate limit reached".to_owned(),
    };
    status.rate_limit = Some(RateLimit {
        resets_at: Utc::now() - chrono::Duration::minutes(5),
        window: Some("five_hour".to_owned()),
    });
    status.quota = vec![reported(Vendor::Codex, "personal", 11)?];

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, contains_substring("already reopened"));
    assert_that!(warnings, not(contains_substring("Retry with a different")));
    Ok(())
}

/// A window reopening within one `start`'s wait is waited for, not routed around.
#[gtest]
fn a_window_reopening_soon_outranks_switching_account() -> Result<()> {
    let (_store, mut status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"rate limit reached"}}
    "#})?;
    status.outcome = Outcome::Failed {
        kind: FailureKind::RateLimited,
        detail: "rate limit reached".to_owned(),
    };
    status.rate_limit = Some(RateLimit {
        resets_at: Utc::now() + chrono::Duration::minutes(3),
        window: Some("five_hour".to_owned()),
    });
    status.quota = vec![reported(Vendor::Codex, "personal", 11)?];

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, contains_substring("wait_seconds"));
    assert_that!(warnings, not(contains_substring("Retry with a different")));
    Ok(())
}

/// A reopened turn reads as expected when the run inherited its account's hooks.
///
/// The same event means opposite things in the two worlds.
/// Under isolation a hook should have been impossible, so the transcript is suspect; under
/// inheritance it is exactly what the caller asked for, and phrasing it as an anomaly invites the
/// caller to escalate a working run.
#[gtest]
fn a_reopened_turn_reads_as_expected_when_settings_were_inherited() -> Result<()> {
    let (_store, mut status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"the report"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#})?;
    status.hook_reopening = Some(HookReopening::Expected);
    let inherited = agentmux_mcp::render::warnings(&status);
    assert_that!(
        inherited,
        contains_substring("expected because this run inherited")
    );
    assert_that!(inherited, not(contains_substring("should not be possible")));

    status.hook_reopening = Some(HookReopening::Unexpected);
    let isolated = agentmux_mcp::render::warnings(&status);
    assert_that!(isolated, contains_substring("should not be possible"));
    // The load-bearing sentence survives in the world where it is an alarm.
    assert_that!(isolated, contains_substring("NOT the answer"));
    Ok(())
}

/// The advice must never name an account belonging to the other vendor.
///
/// `quota_for_failure` probes only the failing run's vendor today, but that invariant lives in
/// another crate and nothing at the point of use depended on it.
/// An alias from the wrong table resolves to `UnknownAccount` after a paid round trip.
#[gtest]
fn the_advice_never_names_an_account_of_the_other_vendor() -> Result<()> {
    let (_store, mut status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"rate limit reached"}}
    "#})?;
    status.outcome = Outcome::Failed {
        kind: FailureKind::RateLimited,
        detail: "rate limit reached".to_owned(),
    };
    status.rate_limit = Some(RateLimit {
        resets_at: Utc::now() + chrono::Duration::hours(9),
        window: Some("seven_day".to_owned()),
    });
    // The consultation ran on Codex; this account belongs to Claude.
    status.quota = vec![reported(Vendor::Claude, "other-vendor-account", 3)?];

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, not(contains_substring("other-vendor-account")));
    // With no usable alternative it falls back to the window, as it would with none at all.
    assert_that!(warnings, contains_substring("other vendor"));
    Ok(())
}

/// Cached figures that survived a failed refresh do not read as current ones.
///
/// The age alone cannot tell them apart: a refresh that timed out leaves the previous numbers in
/// place, and `fetchedAtMs` then describes a reading nobody managed to replace.
/// A caller choosing an account on those numbers is choosing on figures agentmux could not
/// confirm.
#[gtest]
fn a_refresh_that_did_not_happen_is_named_rather_than_implied() {
    let timed_out = cached_with(Refresh::TimedOut);
    let failed = cached_with(Refresh::Failed {
        reason: "`claude --print /usage` exited with exit status: 1".to_owned(),
    });

    let timed_out = agentmux_mcp::render::quota_report(&timed_out).join("\n");
    let failed = agentmux_mcp::render::quota_report(&failed).join("\n");

    // Both say the figures predate the attempt, so neither can be mistaken for a fresh reading.
    assert_that!(timed_out, contains_substring("timed out"));
    assert_that!(timed_out, contains_substring("predate it"));
    assert_that!(failed, contains_substring("refresh failed"));
    assert_that!(failed, contains_substring("exit status: 1"));
}

/// The ordinary outcomes are named too, so silence never has to be interpreted.
#[gtest]
fn a_refresh_that_happened_says_so() {
    let refreshed = agentmux_mcp::render::quota_report(&cached_with(Refresh::Succeeded)).join("\n");
    let skipped = agentmux_mcp::render::quota_report(&cached_with(Refresh::Skipped)).join("\n");

    assert_that!(refreshed, contains_substring("just refreshed"));
    assert_that!(
        skipped,
        contains_substring("still inside the refresh window")
    );
}

/// A cached Claude reading whose refresh ended the given way.
fn cached_with(refresh: Refresh) -> AccountQuota {
    AccountQuota {
        vendor: Vendor::Claude,
        account: None,
        description: None,
        observation: Observation::Reported {
            origin: Origin::Cache {
                path: PathBuf::from("/home/dev/.claude.json"),
                fetched_at_ms: None,
                refresh,
            },
            payload: serde_json::json!({"utilization": {"limits": []}}),
        },
    }
}
