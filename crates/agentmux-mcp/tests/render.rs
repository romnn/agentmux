//! What a calling model actually reads.
//!
//! These are the product surface: the header a host may truncate, the warnings that change how a
//! transcript must be read, and the next call a caller will make because the tool named it.
//! They run against a real [`RunStore`] driven by the scripted launcher, so what is asserted here
//! is what a client receives.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, ClaudePermissions, CodexPermissions, CodexSandbox, Delegate, Effort, Isolation,
    ModelId,
};
use agentmux::run::{AutoResume, Retention, RunStatus, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher};
use agentmux::transcript::{FailureKind, Outcome, UsageWindow};
use chrono::Utc;
use googletest::prelude::*;

/// Drive one scripted consultation and return the status a tool would render.
fn consult(events: &str) -> Result<(RunStore, RunStatus, tempfile::TempDir)> {
    consult_with(
        events,
        Delegate::Codex {
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            effort: Some(Effort::parse("xhigh").or_fail()?),
            permissions: CodexPermissions {
                sandbox: Some(CodexSandbox::ReadOnly),
                network_access: None,
            },
            account: AccountChoice::Default,
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
            permissions: ClaudePermissions::default(),
            model: ModelId::parse(model).or_fail()?,
            effort: Some(Effort::parse("high").or_fail()?),
            account: AccountChoice::Default,
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
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
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
            billing: None,
            force_codex_exec: false,
            label: None,
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

/// A content filter that refuses the session turn after turn is answered with a fresh session,
/// not another rephrased follow-up, and once one has taken over, the old consultation points at
/// it.
#[gtest]
fn a_session_the_filter_keeps_refusing_is_pointed_at_a_fresh_one() -> Result<()> {
    let flagged = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"partial"}}
        {"type":"turn.failed","error":{"message":"This content was flagged for possible cybersecurity risk"}}
    "#};
    let dir = tempfile::tempdir().or_fail()?;
    let env: BTreeMap<String, String> = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let launcher = ScriptedLauncher::new([
        Script::completed(flagged),
        Script::completed(flagged),
        Script::completed(agentmux::testing::fixtures::CODEX_HAPPY),
    ]);
    let store = RunStore::open(dir.path(), Arc::new(launcher), env).or_fail()?;
    let status = store
        .start(&StartRequest {
            delegate: Delegate::Codex {
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                effort: Some(Effort::parse("xhigh").or_fail()?),
                permissions: CodexPermissions {
                    sandbox: Some(CodexSandbox::ReadOnly),
                    network_access: None,
                },
                account: AccountChoice::Default,
                isolation: None,
            },
            question: "Review the parser.".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })
        .or_fail()?;
    // One refusal: rephrase.
    let once = agentmux_mcp::render::status(&status);
    assert_that!(once, contains_substring("rephrase rather than escalate"));
    assert_that!(once, not(contains_substring("continue_fresh {")));

    // Two in a row: the session itself is refused, so the next call is a fresh session.
    let twice = store
        .follow_up(&status.run_id, "Only the limit check, please.")
        .or_fail()?;
    let rendered = agentmux_mcp::render::status(&twice);
    assert_that!(
        rendered,
        all![
            contains_substring("refused the last 2 turns in a row"),
            contains_substring("likely to be refused too"),
            contains_substring("continue_fresh {\"run_id\""),
            not(contains_substring("follow_up {")),
        ]
    );

    let fresh = store.continue_fresh(&status.run_id, None).or_fail()?;
    let old = agentmux_mcp::render::status(&store.status(&status.run_id).or_fail()?);
    assert_that!(
        old,
        contains_substring(format!("continued:  in {}", fresh.run_id).as_str())
    );
    assert_that!(old, not(contains_substring("continue_fresh {")));
    assert_that!(
        old,
        contains_substring(format!("status {{\"run_id\": \"{}\"}}", fresh.run_id).as_str())
    );
    assert_that!(old, not(contains_substring("follow_up {")));
    assert_that!(
        agentmux_mcp::render::status(&fresh),
        contains_substring(format!("continues:  {}", status.run_id).as_str())
    );
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

/// A Codex turn that said something and then failed because the model was at capacity.
const OVERLOADED: &str = indoc::indoc! {r#"
    {"type":"thread.started","thread_id":"01a0"}
    {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"halfway through"}}
    {"type":"turn.failed","error":{"message":"Selected model is at capacity. Please try a different model."}}
"#};

/// An overloaded model with nothing resuming it is a pause the caller ends: nothing to fix, and a
/// `follow_up` that continues the session.
#[gtest]
fn an_overloaded_model_is_told_to_continue_the_session() -> Result<()> {
    let (_store, status, _dir) = consult(OVERLOADED)?;
    let rendered = agentmux_mcp::render::status(&status);

    assert_that!(rendered, contains_substring("failed (model overloaded)"));
    assert_that!(
        rendered,
        contains_substring("a `follow_up` asking it to continue")
    );
    assert_that!(rendered, contains_substring("follow_up {"));
    Ok(())
}

/// While agentmux is about to resume the consultation itself, the caller is told when and which
/// turn to wait for, and how to stop it, and is not offered a follow-up that would take the
/// resume's place.
#[gtest]
fn a_pending_automatic_resume_replaces_the_advice_to_follow_up() -> Result<()> {
    let (store, mut status, _dir) = consult(OVERLOADED)?;
    status.auto_resume = Some(AutoResume::Scheduled {
        turn: 2,
        attempt: 3,
        attempts: 12,
        due_at: Utc::now() + chrono::TimeDelta::minutes(2),
    });
    let rendered = agentmux_mcp::render::status(&status);

    assert_that!(
        rendered,
        contains_substring("agentmux resumes this automatically at")
    );
    assert_that!(rendered, contains_substring("as turn 2"));
    assert_that!(rendered, contains_substring("(attempt 3 of 12)"));
    let wait = format!("wait {{\"run_id\": \"{}\", \"turn\": 2}}", status.run_id);
    assert_that!(rendered, contains_substring(wait.as_str()));
    assert_that!(rendered, contains_substring("cancel {"));
    assert_that!(rendered, not(contains_substring("follow_up {")));

    // Every result that carries the warnings carries the resume: `result` and `tail` too.
    let page = store.page(&status.run_id, 0, 100_000).or_fail()?;
    for rendered in [
        agentmux_mcp::render::transcript(&status, &page),
        agentmux_mcp::render::tail(&status, &page),
    ] {
        assert_that!(
            rendered,
            contains_substring("agentmux resumes this automatically at")
        );
        assert_that!(rendered, not(contains_substring("follow_up {")));
    }
    Ok(())
}

/// Once agentmux has stopped resuming, the caller is told so, and the advice for the failure
/// itself applies again.
#[gtest]
fn an_exhausted_automatic_resume_hands_the_consultation_back() -> Result<()> {
    let (_store, mut status, _dir) = consult(OVERLOADED)?;
    status.auto_resume = Some(AutoResume::Exhausted { attempts: 12 });
    let rendered = agentmux_mcp::render::status(&status);

    assert_that!(
        rendered,
        contains_substring("12 times in a row and the model was overloaded every time")
    );
    assert_that!(rendered, contains_substring("follow_up {"));
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
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
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
                permissions: ClaudePermissions::default(),
                model: ModelId::parse("claude-opus-5").or_fail()?,
                effort: Some(Effort::parse("xhigh").or_fail()?),
                account: AccountChoice::Default,
                // Isolated, so the hook that reopened the turn is the alarming kind.
                isolation: Some(Isolation::Isolated),
            },
            question: "Reply with exactly: THE_REPORT_BODY".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
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
    let env: BTreeMap<String, String> = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
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
                effort: Some(Effort::parse("xhigh").or_fail()?),
                permissions: CodexPermissions {
                    sandbox: Some(CodexSandbox::ReadOnly),
                    network_access: None,
                },
                account: AccountChoice::Default,
                isolation: None,
            },
            question: "A long review.".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
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
/// advice when the window reopens within the longest wait one call can block for.
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
    status.usage_window = Some(UsageWindow {
        resets_at: Utc::now() + chrono::Duration::minutes(4),
        window: Some("five_hour".to_owned()),
        exhausted: true,
        used_percent: None,
    });

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, contains_substring("five_hour"));
    assert_that!(warnings, contains_substring("wait that long"));
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
    status.usage_window = Some(UsageWindow {
        resets_at: Utc::now() + chrono::Duration::hours(9),
        window: Some("seven_day_overage_included".to_owned()),
        exhausted: true,
        used_percent: None,
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
    status.usage_window = None;

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
    agentmux::testing::write_config_file(
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
                permissions: ClaudePermissions::default(),
                model: ModelId::parse("sonnet").or_fail()?,
                effort: Some(Effort::parse("high").or_fail()?),
                account: AccountChoice::Default,
                isolation: None,
            },
            question: "Review the diff for correctness bugs.".to_owned(),
            cwd: home.path().to_path_buf(),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
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
/// The input counts every prompt token, Claude's cached ones included, so the two vendors'
/// figures read alike.
#[gtest]
fn usage_says_which_kind_of_number_it_is() -> Result<()> {
    let (_store, priced, _dir) =
        consult_claude(agentmux::testing::fixtures::CLAUDE_HAPPY, "claude-opus-5")?;
    let (_store, unpriced, _dir2) = consult(agentmux::testing::fixtures::CODEX_HAPPY)?;

    let priced = agentmux_mcp::render::status(&priced);
    let unpriced = agentmux_mcp::render::status(&unpriced);

    assert_that!(
        priced,
        contains_substring(
            "usage:      run total, all turns: 29442 in (18574 cached, 10866 cache write) / 128 \
             out (116 reasoning) · $0.0100 list price\n"
        )
    );
    assert_that!(
        unpriced,
        contains_substring(
            "usage:      run total, all turns: 14003 in (4480 cached) / 31 out (22 reasoning) · no \
             price reported\n"
        )
    );
    // One turn needs no breakdown, and the progress line no longer carries token counts.
    assert_that!(unpriced, not(contains_substring("turn 1:")));
    let progress = unpriced
        .lines()
        .find(|line| line.starts_with("progress:"))
        .or_fail()?;
    assert_that!(progress, not(contains_substring(" in ")));
    Ok(())
}

/// A follow-up's figures are taken apart into what each turn spent, a turn that reported nothing
/// is covered by the next running total, and a running turn says it has reported nothing yet.
#[gtest]
fn usage_is_broken_down_by_turn() -> Result<()> {
    let completed = |input: u64, output: u64| {
        Script::completed(indoc::formatdoc! {r#"
            {{"type":"thread.started","thread_id":"01a0"}}
            {{"type":"item.completed","item":{{"id":"i0","type":"agent_message","text":"done"}}}}
            {{"type":"turn.completed","usage":{{"input_tokens":{input},"cached_input_tokens":0,"output_tokens":{output}}}}}
        "#})
    };
    let dir = tempfile::tempdir().or_fail()?;
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([
            completed(100, 10),
            Script::completed(indoc::indoc! {r#"
                {"type":"thread.started","thread_id":"01a0"}
                {"type":"turn.failed","error":{"message":"stream disconnected"}}
            "#}),
            completed(250, 30),
            Script::running(indoc::indoc! {r#"
                {"type":"thread.started","thread_id":"01a0"}
            "#}),
        ])),
        BTreeMap::new(),
    )
    .or_fail()?;
    let started = store
        .start(&StartRequest {
            delegate: Delegate::Codex {
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                effort: Some(Effort::parse("xhigh").or_fail()?),
                permissions: CodexPermissions {
                    sandbox: Some(CodexSandbox::ReadOnly),
                    network_access: None,
                },
                account: AccountChoice::Default,
                isolation: None,
            },
            question: "Review the diff for correctness bugs.".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })
        .or_fail()?;
    for question in ["again", "once more", "and the tests"] {
        store.follow_up(&started.run_id, question).or_fail()?;
    }
    let status = store.status(&started.run_id).or_fail()?;

    // Turn 3's 250 is the session's total, so turns 2 and 3 together spent 150 of it.
    let rendered = agentmux_mcp::render::status(&status);
    assert_that!(
        rendered,
        contains_substring(indoc::indoc! {"
            usage:      run total, all turns: 250 in / 30 out · no price reported
                        turn 1: 100 in / 10 out
                        turns 2–3: 150 in / 20 out
        "})
    );
    // Turn 4 is still running, so it has not reported yet rather than reported nothing.
    assert_that!(rendered, not(contains_substring("turn 4")));
    // The CLI's `list` and the MCP server's carry the same compact total.
    let listed = store
        .list(10, &agentmux::run::ListFilter::default())
        .or_fail()?;
    assert_that!(
        agentmux_mcp::render::list(&listed, false),
        contains_substring("  250 in / 30 out\n")
    );
    Ok(())
}

/// A price that could not be counted qualifies the price alone, never the tokens beside it.
///
/// A Claude upgraded across 2.1.277 mid-consultation reports a running price total that
/// includes the turns it already priced one by one, so that one total is left out, while every
/// turn's tokens are exact.
#[gtest]
fn an_uncounted_price_qualifies_the_price_alone() -> Result<()> {
    let claude_turn = |release: &str, cost: f64| {
        Script::completed(indoc::formatdoc! {r#"
            {{"type":"system","subtype":"init","session_id":"850f440b-be0f-411e-aaf3-bc8fdcc6b6b8","claude_code_version":"{release}"}}
            {{"type":"assistant","message":{{"model":"claude-opus-5","id":"m","type":"message","role":"assistant","content":[{{"type":"text","text":"done"}}]}},"parent_tool_use_id":null,"session_id":"850f440b-be0f-411e-aaf3-bc8fdcc6b6b8"}}
            {{"type":"result","subtype":"success","is_error":false,"session_id":"850f440b-be0f-411e-aaf3-bc8fdcc6b6b8","usage":{{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":1}},"total_cost_usd":{cost}}}
        "#})
    };
    let dir = tempfile::tempdir().or_fail()?;
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([
            claude_turn("2.1.261", 0.10),
            claude_turn("2.1.277", 0.30),
            claude_turn("2.1.277", 0.45),
        ])),
        BTreeMap::new(),
    )
    .or_fail()?;
    let started = store
        .start(&StartRequest {
            delegate: Delegate::Claude {
                permissions: ClaudePermissions::default(),
                model: ModelId::parse("claude-opus-5").or_fail()?,
                effort: Some(Effort::parse("high").or_fail()?),
                account: AccountChoice::Default,
                isolation: None,
            },
            question: "Review the diff for correctness bugs.".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })
        .or_fail()?;
    for question in ["again", "once more"] {
        store.follow_up(&started.run_id, question).or_fail()?;
    }
    let status = store.status(&started.run_id).or_fail()?;

    // Turn 2's total is left out and only taken apart against by turn 3, so the price is at
    // least turn 1's and turn 3's, and the tokens stay exact.
    assert_that!(
        agentmux_mcp::render::status(&status),
        contains_substring(indoc::indoc! {"
            usage:      run total, all turns: 300 in / 3 out · at least $0.2500 list price
                        turn 1: 100 in / 1 out · $0.1000
                        turn 2: 100 in / 1 out · price unknown
                        turn 3: 100 in / 1 out · $0.1500
        "})
    );
    let listed = store
        .list(10, &agentmux::run::ListFilter::default())
        .or_fail()?;
    assert_that!(
        agentmux_mcp::render::list(&listed, false),
        contains_substring("  300 in / 3 out · at least $0.25\n")
    );
    Ok(())
}

/// A failure the stream leaves unexplained quotes the delegate CLI's stderr.
///
/// The resume refusal the claude CLI emits carries nothing but `is_error`; the reason is on
/// stderr, and a caller told only that the delegate reported an error has nothing to act on.
#[gtest]
fn a_failure_the_stream_leaves_unexplained_quotes_the_cli_stderr() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([Script::exited(
            indoc::indoc! {r#"
                {"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0}
            "#},
            indoc::indoc! {"
                No conversation found with session ID: 9f1c21f8-1b3d-4a7e-9a0c-2d6f4b8e5c10
            "},
            1,
        )])),
        BTreeMap::new(),
    )
    .or_fail()?;
    let status = store
        .start(&StartRequest {
            delegate: Delegate::Claude {
                permissions: ClaudePermissions::default(),
                model: ModelId::parse("claude-opus-5").or_fail()?,
                effort: Some(Effort::parse("high").or_fail()?),
                account: AccountChoice::Default,
                isolation: None,
            },
            question: "q".to_owned(),
            cwd: dir.path().to_path_buf(),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })
        .or_fail()?;

    let rendered = agentmux_mcp::render::status(&status);

    assert_that!(
        rendered,
        contains_substring("Its CLI wrote to stderr: No conversation found")
    );
    Ok(())
}

/// A Codex turn whose app-server sent something agentmux could not read says how to run the
/// consultation without the app-server protocol, and other drift does not.
#[gtest]
fn protocol_drift_points_at_codex_exec() -> Result<()> {
    let protocol_drift_capture = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"th"}
        {"type":"agentmux.undecoded","method":"item/completed","reason":"missing field","payload":{}}
        {"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}}
    "#};
    let (_store, status, _dir) = consult(protocol_drift_capture)?;
    assert_that!(
        agentmux_mcp::render::warnings(&status),
        contains_substring("`codex_exec: true`")
    );

    let unknown_event_capture = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"th"}
        {"type":"some.future.event"}
        {"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}}
    "#};
    let (_store, status, _dir) = consult(unknown_event_capture)?;
    assert_that!(
        agentmux_mcp::render::warnings(&status),
        all![
            contains_substring("did not recognise"),
            not(contains_substring("codex_exec"))
        ]
    );
    Ok(())
}

/// A turn that left processes running is named in a warning above the transcript, with how many
/// it left, how many still ran once signalled, and that their work may be incomplete.
#[gtest]
fn what_a_turn_left_running_is_a_warning() -> Result<()> {
    use agentmux::left_running::{LeftProcess, LeftRunning, ProcessFate};
    use agentmux::run::TurnLeftRunning;

    let (_store, mut status, _dir) = consult(agentmux::testing::fixtures::CODEX_HAPPY)?;
    assert_that!(
        agentmux_mcp::render::warnings(&status),
        not(contains_substring("processes running"))
    );
    let process = |pid: u32, fate: ProcessFate| LeftProcess {
        pid,
        start_time_secs: 1_790_000_000,
        name: "python3".to_owned(),
        fate,
    };
    status.left_running = vec![TurnLeftRunning {
        turn: 2,
        report: LeftRunning::of(vec![
            process(1, ProcessFate::StillRunning),
            process(2, ProcessFate::NoLongerRunning),
        ])
        .or_fail()?,
    }];
    assert_that!(
        agentmux_mcp::render::warnings(&status),
        contains_substring(indoc::indoc! {"
            warning:    turn 2 left 2 processes running outside the delegate CLI's process tree after the CLI exited; agentmux signalled them to stop, and 1 is no longer running and 1 still is — work in them may be incomplete.
                        The transcript names them at the end of that turn: do not rely on work the
                        delegate left running there having finished.
        "})
    );
    Ok(())
}

/// Earlier failures sit beside the newest turn's state, never inside it.
///
/// Written into the state line, a failure from turn one reads as the state of the turn running
/// now: `running (turn 1 failed …)` looks like the current turn failed.
/// Every earlier failure is listed, not only the first, so a later one is not hidden either.
#[gtest]
fn earlier_failures_do_not_read_as_the_newest_turns_state() -> Result<()> {
    let flagged = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"findings"}}
        {"type":"turn.failed","error":{"message":"ERROR: This content was flagged for possible cybersecurity risk"}}
    "#};
    let dir = tempfile::tempdir().or_fail()?;
    let env: BTreeMap<String, String> = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([
            Script::completed(flagged),
            Script::completed(flagged),
            Script::running(indoc::indoc! {r#"
                {"type":"thread.started","thread_id":"01a0"}
            "#}),
        ])),
        env,
    )
    .or_fail()?;
    let started = store
        .start(&StartRequest {
            delegate: Delegate::Codex {
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                effort: Some(Effort::parse("xhigh").or_fail()?),
                permissions: CodexPermissions {
                    sandbox: Some(CodexSandbox::ReadOnly),
                    network_access: None,
                },
                account: AccountChoice::Default,
                isolation: None,
            },
            question: "Review the diff for correctness bugs.".to_owned(),
            cwd: PathBuf::from("/work/project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })
        .or_fail()?;
    store.follow_up(&started.run_id, "try again").or_fail()?;
    let status = store.follow_up(&started.run_id, "and again").or_fail()?;

    let rendered = agentmux_mcp::render::status(&status);
    let state_line = rendered
        .lines()
        .find(|line| line.starts_with("state:"))
        .or_fail()?;
    // The state is the third turn's alone, and both failed turns are named on their own line.
    assert_that!(state_line, eq("state:      running"));
    assert_that!(
        rendered,
        contains_substring(
            "earlier:    turn 1 failed (content flagged), turn 2 failed (content flagged)"
        )
    );
    // The machine-readable form numbers the turns as the transcript's headings do.
    let json = serde_json::to_value(&status).or_fail()?;
    assert_that!(
        json["earlier_failures"],
        eq(&serde_json::json!([
            {"turn": 1, "kind": "content_flagged"},
            {"turn": 2, "kind": "content_flagged"},
        ]))
    );
    Ok(())
}

/// A rate-limited failure whose recorded window still has room is not timed by that window.
///
/// The window did not refuse the turn, so it says nothing about when a retry would succeed, and
/// advising to wait for its reset would park the caller for a week on a window that is open.
#[gtest]
fn a_window_with_room_left_does_not_time_the_retry() -> Result<()> {
    let (_store, mut status, _dir) = consult(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"rate limit reached"}}
    "#})?;
    status.outcome = Outcome::Failed {
        kind: FailureKind::RateLimited,
        detail: "rate limit reached".to_owned(),
    };
    status.usage_window = Some(UsageWindow {
        resets_at: Utc::now() + chrono::Duration::minutes(4),
        window: Some("codex".to_owned()),
        exhausted: false,
        used_percent: Some(28.0),
    });

    let warnings = agentmux_mcp::render::warnings(&status);

    assert_that!(warnings, not(contains_substring("wait that long")));
    assert_that!(warnings, not(contains_substring("reopen")));
    Ok(())
}
