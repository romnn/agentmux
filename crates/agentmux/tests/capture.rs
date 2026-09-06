//! What the delegate said, and whether agentmux kept all of it.
//!
//! Green tests on a happy path prove nothing here, because every historical failure exited zero.
//! These are the tests that make the design's claims true.

use agentmux::delegate::Vendor;
use agentmux::stream::{self, Fold};
use agentmux::testing::fixtures;
use agentmux::transcript::{FailureKind, MessageSource, Outcome, Role};
use googletest::prelude::*;

/// The reason this project exists.
///
/// A `Stop` hook that returns `{"decision":"block"}` reopens a finished turn, and whatever the
/// reopened turn produces becomes the CLI's own `result.result`.
/// In the recorded run the delegate answered `THE_REPORT_BODY`, the hook reopened the turn eight
/// times, and `result.result` came back as the **empty string** with `is_error: false` and
/// `terminal_reason: "completed"`.
/// A reader that trusted the CLI's last word would have reported a clean run and lost the answer.
///
/// So: the report must be present, the injection must be present and marked, and the report must
/// not be what `final_message` returns.
#[gtest]
fn a_blocking_stop_hook_does_not_replace_the_report() {
    let Fold { turn, .. } = stream::fold(
        Vendor::Claude,
        0,
        "Reply with exactly: THE_REPORT_BODY",
        fixtures::CLAUDE_STOP_HOOK,
    );

    let report = turn
        .messages
        .iter()
        .find(|m| m.text.trim() == "THE_REPORT_BODY")
        .expect("the report must survive the fold");
    assert_that!(report.role, eq(Role::Assistant));
    assert_that!(report.source, eq(MessageSource::Delegate));

    let injections: Vec<_> = turn
        .messages
        .iter()
        .filter(|m| m.source == MessageSource::HookInjection)
        .collect();
    assert_that!(injections, not(is_empty()));
    assert_that!(
        injections.first().map(|m| m.text.as_str()),
        some(starts_with("Stop hook feedback:"))
    );
    assert_that!(turn.was_reopened_by_hook(), eq(true));

    // The hook's continuation is the last thing the delegate said, and it is not the answer.
    let last = turn.messages.iter().rev().find(|m| m.is_report_content());
    assert_that!(
        last.map(|m| m.text.as_str()),
        some(not(eq("THE_REPORT_BODY")))
    );

    // And the turn still reads as a success, exactly as it did in the failure that motivated this.
    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
}

/// The transcript is the deliverable, and nothing stores a single "answer" in its place.
#[gtest]
fn the_report_is_recoverable_even_though_the_cli_reported_an_empty_result() {
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", fixtures::CLAUDE_STOP_HOOK);
    let mut transcript = agentmux::transcript::Transcript::default();
    transcript.turns.push(turn);

    let rendered = agentmux::transcript::render_turn(transcript.turns.first().expect("one turn"));
    assert_that!(rendered, contains_substring("THE_REPORT_BODY"));
    // The render must warn a reader that the tail of the turn is not the answer.
    assert_that!(rendered, contains_substring("reopened this finished turn"));
    assert_that!(rendered, contains_substring("not the answer"));
    assert_that!(transcript.was_reopened_by_hook(), eq(true));
}

/// Schema drift must be loud rather than lossy.
#[gtest]
fn an_unknown_event_type_is_counted_not_skipped() {
    let stream = format!(
        "{}\n{}\n",
        r#"{"type":"a_shape_nobody_has_seen","payload":{"anything":1}}"#,
        fixtures::CLAUDE_HAPPY.trim_end(),
    );
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", &stream);

    assert_that!(turn.unrecognised.total(), ge(1));
    assert_that!(
        turn.unrecognised.summary(),
        some(contains_substring("a_shape_nobody_has_seen"))
    );
}

/// A renamed message type would otherwise silently drop every report.
#[gtest]
fn a_renamed_codex_item_type_is_counted_not_skipped() {
    let stream = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i0","type":"assistant_message","text":"the report"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", stream);

    assert_that!(
        turn.unrecognised.summary(),
        some(contains_substring("item.completed.assistant_message"))
    );
}

/// A line that is not JSON at all must not vanish either.
#[gtest]
fn a_non_json_line_is_counted() {
    let stream = "warning: your CLI is out of date\n{\"type\":\"turn.started\"}\n";
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", stream);
    assert_that!(
        turn.unrecognised.summary(),
        some(contains_substring("malformed"))
    );
}

/// A half-written trailing line is normal while a child runs and must not be reported as drift.
#[gtest]
fn a_partial_trailing_line_is_not_counted_as_drift() {
    let stream = "{\"type\":\"turn.started\"}\n{\"type\":\"item.comp";
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", stream);
    assert_that!(turn.unrecognised.is_empty(), eq(true));
}

/// A failure sits beside the messages, never in place of them.
#[gtest]
fn a_failed_turn_keeps_what_it_collected() {
    let stream = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"two hours of analysis"}}
        {"type":"error","message":"This content was flagged for possible cybersecurity risk"}
        {"type":"turn.failed","error":{"message":"This content was flagged for possible cybersecurity risk"}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", stream);

    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::ContentFlagged),
            detail: anything()
        })
    );
    let kept: Vec<&str> = turn
        .messages
        .iter()
        .filter(|m| m.is_report_content())
        .map(|m| m.text.as_str())
        .collect();
    assert_that!(kept, elements_are![eq(&"two hours of analysis")]);
}

/// One failure arrives as three events.
/// They fold into one outcome.
#[gtest]
fn three_codex_failure_events_fold_into_one_outcome() {
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", fixtures::CODEX_UNSUPPORTED_MODEL);

    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::ModelUnavailable),
            detail: anything()
        })
    );
    // The advisory item error is kept as a note, not promoted to a second failure.
    assert_that!(
        turn.messages
            .iter()
            .filter(|m| m.is_report_content())
            .count(),
        eq(0)
    );
}

/// The trap: `item.completed` with `item.type: "error"` is a warning, and the turn succeeds.
///
/// Measured against the real CLI by resuming a thread with a different model.
/// Folding this into a failure would fail a consultation that in fact delivered its answer.
#[gtest]
fn a_codex_item_error_is_a_warning_not_a_failure() {
    let Fold { turn, session } = stream::fold(
        Vendor::Codex,
        1,
        "What was the secret word?",
        fixtures::CODEX_RESUME_WARNING,
    );

    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(
        turn.messages
            .iter()
            .filter(|m| m.is_report_content())
            .map(|m| m.text.clone())
            .collect::<Vec<_>>(),
        elements_are![eq("BANANAPHONE")]
    );
    assert_that!(
        turn.messages
            .iter()
            .filter(|m| m.source == MessageSource::Synthetic)
            .count(),
        eq(1)
    );
    assert_that!(session.map(|s| s.as_str().to_owned()), some(anything()));
}

/// `subtype` says `success` while `is_error` says otherwise.
/// Branch on `is_error`.
#[gtest]
fn an_unknown_claude_model_is_a_failure_despite_subtype_success() {
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "hi", fixtures::CLAUDE_UNKNOWN_MODEL);
    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::ModelUnavailable),
            detail: anything()
        })
    );
}

/// Both vendors' happy paths yield the answer and a resumable session.
#[gtest]
fn a_clean_run_yields_the_answer_and_a_session_to_resume() {
    for (vendor, fixture) in [
        (Vendor::Claude, fixtures::CLAUDE_HAPPY),
        (Vendor::Codex, fixtures::CODEX_HAPPY),
    ] {
        let Fold { turn, session } = stream::fold(vendor, 0, "q", fixture);
        assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
        assert_that!(
            turn.messages
                .iter()
                .filter(|m| m.is_report_content())
                .map(|m| m.text.clone())
                .collect::<Vec<_>>(),
            elements_are![eq("FIXTURE_OK")],
            "vendor {vendor}"
        );
        assert_that!(
            session,
            some(anything()),
            "vendor {vendor} must announce a session"
        );
    }
}

/// A rate limit must be classified from the status code, not from the vendor's wording.
///
/// The recorded refusal says "You've reached your Fable limit", which no rule could match for long:
/// the model name is in it, and the sentence is marketing copy that changes.
/// `api_error_status: 429` is the part that does not change, and the same event carries
/// `subtype: "success"`, so anything reading the subtype would have called this a clean run.
#[gtest]
fn a_rate_limited_run_is_classified_from_the_status_code() {
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", fixtures::CLAUDE_RATE_LIMITED);

    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::RateLimited),
            detail: contains_substring("Fable limit"),
        })
    );
    // The delegate's own explanation is kept as a message rather than collapsed into the failure,
    // because it is the only place the remedy ("switch to another model") is stated.
    assert_that!(
        turn.messages
            .iter()
            .any(|m| m.text.contains("Switch to another model")),
        eq(true)
    );
}

/// The reopening time decides whether waiting or switching is correct, so it must survive the fold.
///
/// A caller told only "rate limited" cannot choose between the two, and the difference is nine
/// hours in the recorded run.
#[gtest]
fn a_rate_limited_run_keeps_the_window_reopening_time() {
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", fixtures::CLAUDE_RATE_LIMITED);

    let limit = turn
        .rate_limit
        .clone()
        .expect("the recorded stream carries one");
    assert_that!(limit.resets_at.timestamp(), eq(1_788_768_000));
    assert_that!(
        limit.window.as_deref(),
        some(eq("seven_day_overage_included"))
    );

    // The transcript states it too, so a reader of the rendered text is not worse off than a
    // caller reading the struct.
    assert_that!(
        agentmux::transcript::render_turn(&turn),
        contains_substring("seven_day_overage_included window reopens at")
    );
}

/// The drift guard: no committed fixture may contain an event type the parser cannot name.
///
/// This is what turns "we captured a fixture once" into a standing obligation.
/// When a CLI upgrade introduces a new event and someone re-records the fixtures, this test fails
/// until the parser names it.
#[gtest]
fn no_committed_fixture_contains_an_unnamed_event_type() {
    for (vendor, name, fixture) in fixtures::all() {
        let Fold { turn, .. } = stream::fold(vendor, 0, "q", fixture);
        assert_that!(
            turn.unrecognised.summary(),
            none(),
            "fixture {name} carries an event type the {vendor} parser does not name"
        );
    }
}

/// No committed fixture may carry a path or identifier from the machine that recorded it.
///
/// The fixtures are verbatim captures, and a real capture arrives full of operator identity: the
/// recording user's home directory, the uid in a temp path, the absolute path of the checkout, and
/// the session UUID encoded into the memory-directory slug.
/// None of it is read by either parser, so it is replaced with mocked values before committing.
/// This test exists because the scrub is invisible once done — a re-recording drops the real values
/// straight back in, and without a guard the next commit publishes them.
#[gtest]
fn no_committed_fixture_carries_a_recording_machine_path() {
    // Anything that only appears in a capture taken on a developer's own machine.
    // The mocked stand-ins deliberately avoid every one of these.
    let machine_identity = [
        "/Users/",
        "/Volumes/",
        "/private/tmp/claude-",
        "-scratchpad",
        // A memory slug encodes the absolute checkout path, so any slug that starts at a
        // filesystem root came from a real machine rather than the mocked `-work-project`.
        ".claude-personal/projects/-Users",
        ".claude-personal/projects/-Volumes",
        ".claude-personal/projects/-home",
        ".claude-personal/projects/-private",
    ];

    for (_, name, fixture) in fixtures::all() {
        for marker in machine_identity {
            // Asserting on the boolean rather than the fixture keeps a failure readable; matching
            // the fixture itself would print the whole recorded stream.
            assert_that!(
                fixture.contains(marker),
                eq(false),
                "fixture {name} still carries {marker} from the machine that recorded it; \
                 replace it with a mocked value before committing"
            );
        }
    }
}

/// Detection of a reopened turn must not depend on the CLI's wording.
///
/// The wording is used to name the injection, never to find it.
/// This drives a stream whose injected message says nothing about hooks at all and asserts it is
/// still caught, because the day the vendor rewords that string is the day a text-matching parser
/// starts losing reports again — silently, with a clean exit.
#[gtest]
fn a_reopened_turn_is_detected_without_the_vendors_wording() {
    let stream = indoc::indoc! {r#"
        {"type":"system","subtype":"init","session_id":"85ff2d6a-ec24-47cf-a4f8-486a5bb06814"}
        {"type":"assistant","message":{"content":[{"type":"text","text":"THE_REPORT_BODY"}]},"parent_tool_use_id":null}
        {"type":"user","message":{"content":[{"type":"text","text":"Reconsider your answer and reply with a summary only."}]},"parent_tool_use_id":null}
        {"type":"assistant","message":{"content":[{"type":"text","text":"Summary: looks fine."}]},"parent_tool_use_id":null}
        {"type":"result","is_error":false,"subtype":"success","terminal_reason":"completed","result":"Summary: looks fine."}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", stream);

    assert_that!(turn.was_reopened_by_hook(), eq(true));
    assert_that!(
        turn.messages
            .iter()
            .filter(|m| m.source == MessageSource::HookInjection)
            .count(),
        eq(1)
    );
    assert_that!(
        agentmux::transcript::render_turn(&turn),
        contains_substring("not the answer")
    );
}

/// A delegate's own tool results are not injections.
#[gtest]
fn tool_results_are_not_mistaken_for_a_reopening() {
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", fixtures::CLAUDE_TOOL_USE);

    assert_that!(turn.was_reopened_by_hook(), eq(false));
    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(
        turn.messages
            .iter()
            .filter(|m| m.is_report_content())
            .count(),
        gt(0)
    );
}

/// The derived final message is a view over the transcript, never a stored answer.
#[gtest]
fn the_final_message_is_derived_and_is_not_the_report_when_a_hook_reopened_the_turn() {
    let mut transcript = agentmux::transcript::Transcript::default();
    transcript
        .turns
        .push(stream::fold(Vendor::Claude, 0, "q", fixtures::CLAUDE_STOP_HOOK).turn);

    let final_message = transcript.final_message().expect("a final message exists");
    // It is the last thing the delegate said, which after a reopening is not the report.
    assert_that!(final_message, not(eq("THE_REPORT_BODY")));
    // And the transcript says loudly that the shortcut cannot be trusted here.
    assert_that!(transcript.was_reopened_by_hook(), eq(true));

    // On a clean run it is exactly the answer, which is the only case it may be used for.
    let mut clean = agentmux::transcript::Transcript::default();
    clean
        .turns
        .push(stream::fold(Vendor::Claude, 0, "q", fixtures::CLAUDE_HAPPY).turn);
    assert_that!(clean.was_reopened_by_hook(), eq(false));
    assert_that!(clean.final_message(), some(eq("FIXTURE_OK")));
}

/// A reply recovered from outside the event stream still counts as the final message.
///
/// A derived view that disagreed with the record would be this crate's own failure mode at small
/// scale: a visibly non-empty transcript reporting that the delegate said nothing.
#[gtest]
fn the_final_message_falls_back_to_a_recovered_reply() {
    let mut turn = stream::fold(Vendor::Codex, 0, "q", fixtures::CODEX_HAPPY).turn;
    turn.messages.clear();
    turn.recovered = Some("the findings, recovered".to_owned());

    let mut transcript = agentmux::transcript::Transcript::default();
    transcript.turns.push(turn);
    assert_that!(
        transcript.final_message(),
        some(eq("the findings, recovered"))
    );
}

/// A genuine Claude follow-up must not be mistaken for a reopened turn.
///
/// The reopening rule fires on any main-conversation `user` text block after the delegate has
/// spoken, so a resumed stream that replayed earlier turns would make every follow-up warn that
/// its own answer is not the answer.
/// Measured against the real CLI: a resume carries only the new turn, and reports the session id
/// it was asked to resume.
#[gtest]
fn a_genuine_resume_neither_warns_nor_looks_like_a_new_session() {
    let Fold { turn, session } = stream::fold(
        Vendor::Claude,
        1,
        "What was the word?",
        fixtures::CLAUDE_RESUME,
    );

    assert_that!(turn.was_reopened_by_hook(), eq(false));
    assert_that!(
        turn.messages
            .iter()
            .filter(|m| m.is_report_content())
            .map(|m| m.text.clone())
            .collect::<Vec<_>>(),
        elements_are![eq("FLAMINGO")]
    );
    // The id it reports is the one a follow-up passed to `--resume`, which is what lets agentmux
    // tell a real continuation from a silently fresh conversation.
    assert_that!(
        session.map(|s| s.as_str().to_owned()),
        some(eq("850f440b-be0f-411e-aaf3-bc8fdcc6b6b8"))
    );
}
