//! What a calling model actually reads.
//!
//! These are the product surface: the header a host may truncate, the warnings that change how a
//! transcript must be read, and the next call a caller will make because the tool named it.
//! They run against a real [`RunStore`] driven by the scripted launcher, so what is asserted here
//! is what a client receives.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentmux::delegate::{CodexSandbox, Delegate, Effort, ModelId};
use agentmux::run::{Retention, RunStatus, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher};
use agentmux::transcript::{FailureKind, Outcome, RateLimit};
use chrono::Utc;
use googletest::prelude::*;

/// Drive one scripted consultation and return the status a tool would render.
fn consult(events: &str) -> Result<(RunStore, RunStatus, tempfile::TempDir)> {
    consult_with(
        events,
        Delegate::Codex {
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            effort: Effort::parse("xhigh").or_fail()?,
            sandbox: CodexSandbox::ReadOnly,
            account: None,
            isolation: None,
        },
    )
}

/// Drive one scripted Claude consultation against a named model.
///
/// Claude rather than Codex because only Claude's stream reports the model it ran, which is the
/// asymmetry the model tests are about.
fn consult_claude(events: &str, model: &str) -> Result<(RunStore, RunStatus, tempfile::TempDir)> {
    consult_with(
        events,
        Delegate::Claude {
            model: ModelId::parse(model).or_fail()?,
            effort: Effort::parse("high").or_fail()?,
            account: None,
            isolation: None,
        },
    )
}

/// Drive one scripted consultation of the given delegate.
fn consult_with(
    events: &str,
    delegate: Delegate,
) -> Result<(RunStore, RunStatus, tempfile::TempDir)> {
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
            delegate,
            question: "Review the diff for correctness bugs.".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
        })
        .or_fail()?;
    Ok((store, status, dir))
}

/// A blocked closing message must read as "the work survived", not as "the tool broke".
///
/// This is the case the errors-versus-outcomes split exists for: a delegate that analysed for an
/// hour and had only its write-up refused.
/// A caller that reads this as a broken tool discards the findings, so the header has to say the
/// failure did not take the work with it — above the transcript, where a truncating host cannot
/// cut it off.
#[gtest]
fn a_content_flagged_failure_says_the_work_survived() -> Result<()> {
    let (store, status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"an hour of findings"}}
        {"type":"turn.failed","error":{"message":"ERROR: This content was flagged for possible cybersecurity risk"}}
    "#})?;
    let page = store.page(&status.run_id, 0, 100_000).or_fail()?;
    let rendered = agentmux_mcp::render::transcript(&status, &page);

    let (header, body) = rendered.split_once("\n---\n").or_fail()?;
    assert_that!(header, contains_substring("failed (content flagged)"));
    assert_that!(header, contains_substring("did not discard the work"));
    // The advice must steer away from a retry, because retrying reproduces the block.
    assert_that!(header, contains_substring("read them before retrying"));
    assert_that!(body, contains_substring("an hour of findings"));
    Ok(())
}

/// The advice varies by failure, because the right next move does.
#[gtest]
fn an_unavailable_model_is_told_to_fix_the_model_not_to_wait() -> Result<()> {
    let (store, status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"The 'gpt-9' model is not supported for this account"}}
    "#})?;
    let page = store.page(&status.run_id, 0, 100_000).or_fail()?;
    let rendered = agentmux_mcp::render::transcript(&status, &page);

    assert_that!(rendered, contains_substring("failed (model unavailable)"));
    assert_that!(
        rendered,
        contains_substring("Correct `model` and try again")
    );
    // And it must not be advertised as resumable, or the caller pays for a turn that cannot work.
    assert_that!(status.resumable, eq(false));
    assert_that!(rendered, not(contains_substring("follow_up")));
    Ok(())
}

/// A vendor's error text is unbounded; the header line it lands on is not.
#[gtest]
fn a_long_failure_detail_does_not_break_the_header() -> Result<()> {
    let sprawling = "line one\\nline two\\n".repeat(60);
    let (store, status, _dir) = consult(&format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"01a0\"}}\n\
         {{\"type\":\"turn.failed\",\"error\":{{\"message\":\"{sprawling}\"}}}}\n"
    ))?;
    let page = store.page(&status.run_id, 0, 100_000).or_fail()?;
    let rendered = agentmux_mcp::render::transcript(&status, &page);
    let header = rendered.split("\n---\n").next().or_fail()?;

    // Every header line still reads as `key: value`, so `transcript:` and `bytes:` stay findable.
    for key in [
        "run_id:",
        "state:",
        "delegate:",
        "reading:",
        "transcript:",
        "bytes:",
    ] {
        assert_that!(header, contains_substring(key));
    }
    let state_line = header.lines().find(|l| l.starts_with("state:")).or_fail()?;
    assert_that!(state_line.chars().count(), le(220));
    Ok(())
}

/// A hook that reopened a finished turn must be announced before the transcript, not after it.
#[gtest]
fn the_hook_warning_lands_above_the_transcript() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let env: BTreeMap<String, String> = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([Script::completed(
            agentmux::testing::fixtures::CLAUDE_STOP_HOOK,
        )])),
        env,
    )
    .or_fail()?;
    let status = store
        .start(&StartRequest {
            delegate: Delegate::Claude {
                model: ModelId::parse("claude-opus-5").or_fail()?,
                effort: Effort::parse("xhigh").or_fail()?,
                account: None,
                isolation: None,
            },
            question: "Reply with exactly: THE_REPORT_BODY".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
        })
        .or_fail()?;

    let page = store.page(&status.run_id, 0, 100_000).or_fail()?;
    let rendered = agentmux_mcp::render::transcript(&status, &page);
    let (header, body) = rendered.split_once("\n---\n").or_fail()?;

    assert_that!(header, contains_substring("is NOT the answer"));
    assert_that!(body, contains_substring("THE_REPORT_BODY"));
    Ok(())
}

/// Every result names the next call with its arguments already filled in.
#[gtest]
fn a_finished_consultation_names_the_next_call() -> Result<()> {
    let (_store, status, _dir) = consult(agentmux::testing::fixtures::CODEX_HAPPY)?;
    let rendered = agentmux_mcp::render::status(&status);

    let expected_id = format!("\"run_id\": \"{}\"", status.run_id);
    assert_that!(rendered, contains_substring(expected_id.as_str()));
    assert_that!(rendered, contains_substring("result {"));
    assert_that!(rendered, contains_substring("follow_up {"));
    Ok(())
}

/// A finished transcript says the delegate's session is still open, and how to use it.
///
/// `result` is where a caller arrives holding the answer, so it is the page on which a missing
/// next call strands them, and every other renderer fills one in.
#[gtest]
fn a_finished_transcript_offers_the_follow_up() -> Result<()> {
    let (store, started, _dir) = consult(agentmux::testing::fixtures::CODEX_HAPPY)?;
    let (status, page) = store.view(&started.run_id, 0, 100_000).or_fail()?;

    let rendered = agentmux_mcp::render::transcript(&status, &page);

    assert_that!(rendered, contains_substring("follow_up {"));
    let expected_id = format!("\"run_id\": \"{}\"", status.run_id);
    assert_that!(rendered, contains_substring(expected_id.as_str()));
    Ok(())
}

/// A running consultation is told how to watch it and how to stop it, with a live cursor.
#[gtest]
fn a_running_consultation_hands_back_a_usable_cursor() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let env: BTreeMap<String, String> = [("PATH", "/usr/bin")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([Script::running(
            "{\"type\":\"thread.started\",\"thread_id\":\"01a0\"}\n",
        )])),
        env,
    )
    .or_fail()?;
    let status = store
        .start(&StartRequest {
            delegate: Delegate::Codex {
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                effort: Effort::parse("xhigh").or_fail()?,
                sandbox: CodexSandbox::ReadOnly,
                account: None,
                isolation: None,
            },
            question: "A long review.".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
        })
        .or_fail()?;

    let rendered = agentmux_mcp::render::status(&status);
    assert_that!(rendered, contains_substring("Still working"));
    let expected_cursor = format!("\"cursor\": {}", status.transcript_bytes);
    assert_that!(rendered, contains_substring(expected_cursor.as_str()));
    assert_that!(rendered, contains_substring("cancel {"));
    // A running consultation has nothing to continue, so it must not offer one.
    assert_that!(rendered, not(contains_substring("follow_up")));
    Ok(())
}

/// No rendered result offers a single answer to read instead of the transcript.
#[gtest]
fn no_rendered_result_names_an_answer_field() -> Result<()> {
    let (store, status, _dir) = consult(agentmux::testing::fixtures::CODEX_HAPPY)?;
    let page = store.page(&status.run_id, 0, 100_000).or_fail()?;

    for rendered in [
        agentmux_mcp::render::status(&status),
        agentmux_mcp::render::transcript(&status, &page),
        agentmux_mcp::render::tail(&status, &page),
    ] {
        for forbidden in ["final_message", "the answer is:", "summary:"] {
            assert_that!(rendered.as_str(), not(contains_substring(forbidden)));
        }
    }
    Ok(())
}

/// A rate limit whose window reopens soon must not send the caller to the other vendor.
///
/// Switching vendor costs a whole consultation against a different model, so it is the wrong
/// advice when the window reopens inside what a single `start` can already wait for.
/// The reopening time is the only thing that separates this case from the next one, which is why
/// discarding it made both cases produce the same sentence.
#[gtest]
fn a_window_reopening_soon_advises_waiting_rather_than_switching() -> Result<()> {
    let (_store, mut status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"rate limit reached"}}
    "#})?;
    status.outcome = Outcome::Failed {
        kind: FailureKind::RateLimited,
        detail: "rate limit reached".to_owned(),
    };
    status.rate_limit = Some(RateLimit {
        resets_at: Utc::now() + chrono::Duration::minutes(4),
        window: Some("five_hour".to_owned()),
    });

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, contains_substring("five_hour"));
    assert_that!(warnings, contains_substring("wait_seconds"));
    assert_that!(warnings, not(contains_substring("other vendor")));
    Ok(())
}

/// A rate limit whose window is hours away must send the caller elsewhere.
///
/// This is the recorded case: an exhausted seven-day window with nine hours left on it.
/// Advising "wait" there strands the calling agent on a run that cannot progress within any wait
/// it is allowed to ask for.
#[gtest]
fn a_window_hours_away_advises_switching_rather_than_waiting() -> Result<()> {
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
        window: Some("seven_day_overage_included".to_owned()),
    });

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, contains_substring("seven_day_overage_included"));
    assert_that!(warnings, contains_substring("other vendor"));
    assert_that!(warnings, not(contains_substring("wait_seconds")));
    Ok(())
}

/// Without a reopening time the advice must stay honest rather than guess a duration.
///
/// Codex reports no usage window with the flags this crate builds, so this is the live path for
/// every Codex rate limit, not a defensive branch.
#[gtest]
fn a_rate_limit_without_a_window_still_advises_something_actionable() -> Result<()> {
    let (_store, mut status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"rate limit reached"}}
    "#})?;
    status.outcome = Outcome::Failed {
        kind: FailureKind::RateLimited,
        detail: "rate limit reached".to_owned(),
    };
    status.rate_limit = None;

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, contains_substring("rate limited"));
    assert_that!(warnings, contains_substring("other vendor"));
    Ok(())
}

/// A model the vendor expanded is named, so two consultations cannot look alike when they are not.
///
/// The requested identifier may be an alias, and a result that echoes only the request reports the
/// alias as though it were the model that answered.
#[gtest]
fn a_model_the_vendor_expanded_is_reported_beside_the_one_requested() -> Result<()> {
    let (_store, status, _dir) = consult_claude(
        agentmux::testing::fixtures::CLAUDE_TOOL_USE,
        "claude-opus-5",
    )?;

    let rendered = agentmux_mcp::render::status(&status);

    // Both identifiers survive: what was asked for, and what the stream said actually answered.
    assert_that!(rendered, contains_substring("claude-opus-5"));
    assert_that!(rendered, contains_substring("answered by claude-sonnet-5"));
    Ok(())
}

/// A model this machine rewrote is named beside the one that ran, in the same line as the
/// vendor's own expansion.
///
/// The calling agent pinned a model and is reading back another. Told nothing, its next move is to
/// pin harder or to report agentmux as ignoring its arguments; told which of its own machine's
/// rules moved it, there is nothing to debug.
#[gtest]
fn a_model_this_machine_rewrote_is_named_beside_the_one_that_ran() -> Result<()> {
    let home = tempfile::tempdir().or_fail()?;
    std::fs::write(
        home.path().join("agentmux.toml"),
        indoc::indoc! {r#"
            [models.claude]
            "sonnet" = "claude-opus-5"
        "#},
    )
    .or_fail()?;
    let dir = tempfile::tempdir().or_fail()?;
    let env: BTreeMap<String, String> = [
        ("PATH".to_owned(), "/usr/bin".to_owned()),
        (
            "HOME".to_owned(),
            home.path().to_string_lossy().into_owned(),
        ),
    ]
    .into_iter()
    .collect();
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([Script::completed(
            agentmux::testing::fixtures::CLAUDE_TOOL_USE,
        )])),
        env,
    )
    .or_fail()?;
    let status = store
        .start(&StartRequest {
            delegate: Delegate::Claude {
                model: ModelId::parse("sonnet").or_fail()?,
                effort: Effort::parse("high").or_fail()?,
                account: None,
                isolation: None,
            },
            question: "Review the diff for correctness bugs.".to_owned(),
            cwd: home.path().to_path_buf(),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
        })
        .or_fail()?;

    let rendered = agentmux_mcp::render::status(&status);

    // What ran, what was asked for, and what the stream said actually answered.
    assert_that!(rendered, contains_substring("claude claude-opus-5"));
    assert_that!(rendered, contains_substring("(asked for sonnet)"));
    assert_that!(rendered, contains_substring("answered by claude-sonnet-5"));
    Ok(())
}

/// A model that answered as asked adds nothing, because there is no ambiguity to resolve.
#[gtest]
fn a_model_that_matches_the_request_is_not_repeated() -> Result<()> {
    let (_store, status, _dir) = consult_claude(
        agentmux::testing::fixtures::CLAUDE_TOOL_USE,
        "claude-sonnet-5",
    )?;

    let rendered = agentmux_mcp::render::status(&status);

    assert_that!(rendered, not(contains_substring("answered by")));
    Ok(())
}

/// A dollar figure is labelled as list price, and a vendor reporting none says so.
///
/// An unlabelled amount reads as money charged, which it is not on a subscription account, and a
/// blank reads as a consultation that cost nothing rather than one whose vendor reported no
/// figure at all.
#[gtest]
fn cost_says_which_kind_of_number_it_is() -> Result<()> {
    let (_store, priced, _dir) =
        consult_claude(agentmux::testing::fixtures::CLAUDE_HAPPY, "claude-opus-5")?;
    let (_store, unpriced, _dir2) = consult(agentmux::testing::fixtures::CODEX_HAPPY)?;

    let priced = agentmux_mcp::render::status(&priced);
    let unpriced = agentmux_mcp::render::status(&unpriced);

    assert_that!(priced, contains_substring("list price"));
    assert_that!(unpriced, contains_substring("no cost reported"));
    Ok(())
}
