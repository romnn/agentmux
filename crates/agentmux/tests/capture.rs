//! What the delegate said, and whether agentmux kept all of it.
//!
//! Green tests on a happy path prove nothing here, because every historical failure exited zero.
//! These are the tests that make the design's claims true.

use agentmux::delegate::Vendor;
use agentmux::stream::{self, Fold, StreamInput, TurnInput};
use agentmux::testing::fixtures::{self, Fixture};
use agentmux::transcript::{
    FailureKind, Message, MessageSource, Outcome, Role, Usage, render_turn,
};
use googletest::prelude::*;
use indoc::formatdoc;
use uuid::Uuid;

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
        .find(|message| message.text.trim() == "THE_REPORT_BODY")
        .expect("the report must survive the fold");
    assert_that!(report.role, eq(Role::Assistant));
    assert_that!(report.source, eq(MessageSource::Delegate));

    let injections: Vec<_> = turn
        .messages
        .iter()
        .filter(|message| message.source == MessageSource::HookInjection)
        .collect();
    assert_that!(injections, not(is_empty()));
    assert_that!(
        injections.first().map(|message| message.text.as_str()),
        some(starts_with("Stop hook feedback:"))
    );
    assert_that!(turn.was_reopened_by_hook(), eq(true));

    // The hook's continuation is the last thing the delegate said, and it is not the answer.
    let last = turn
        .messages
        .iter()
        .rev()
        .find(|message| message.is_report_content());
    assert_that!(
        last.map(|message| message.text.as_str()),
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
        .filter(|message| message.is_report_content())
        .map(|message| message.text.as_str())
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
            .filter(|message| message.is_report_content())
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
    let Fold { turn, session, .. } = stream::fold(
        Vendor::Codex,
        1,
        "What was the secret word?",
        fixtures::CODEX_RESUME_WARNING,
    );

    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(
        turn.messages
            .iter()
            .filter(|message| message.is_report_content())
            .map(|message| message.text.clone())
            .collect::<Vec<_>>(),
        elements_are![eq("BANANAPHONE")]
    );
    assert_that!(
        turn.messages
            .iter()
            .filter(|message| message.source == MessageSource::Synthetic)
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
        let Fold { turn, session, .. } = stream::fold(vendor, 0, "q", fixture);
        assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
        assert_that!(
            turn.messages
                .iter()
                .filter(|message| message.is_report_content())
                .map(|message| message.text.clone())
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

/// The session a hand-written Claude stream announces.
const SESSION: &str = "85ff2d6a-ec24-47cf-a4f8-486a5bb06814";

/// The id a hand-written stream's question is sent under.
const QUESTION_ID: &str = "aaaaaaaa-1111-4111-8111-111111111111";

/// The id a hand-written stream's steer is sent under.
const STEER_ID: &str = "bbbbbbbb-2222-4222-8222-222222222222";

/// The ids a hand-written stream's question and steer are sent under, spelled as agentmux
/// records them.
fn stream_input() -> Result<StreamInput> {
    Ok(StreamInput {
        question: Some(Uuid::try_parse(QUESTION_ID).or_fail()?),
        steers: vec![Uuid::try_parse(STEER_ID).or_fail()?],
    })
}

/// A Claude turn whose supervisor held its input reads through its `result`: the question's echo
/// is left out, since the question renders already, and the steer is where Claude's echo says it
/// took it, after the command it waited for and above the answer it changed.
/// The ids are compared as UUIDs, so agentmux spelling them unhyphenated does not matter.
/// The turn runs on past the `result`, whose outcome is left for the turn's exit record to settle.
#[gtest]
fn a_steered_claude_turn_shows_the_steer_where_claude_took_it() {
    let input = fixtures::claude_steered_input();
    let Fold {
        turn,
        input: turn_input,
        last_result,
        steers_taken,
        ..
    } = agentmux::vendor::claude::fold::fold_stream_input(0, "q", fixtures::CLAUDE_STEERED, &input);

    assert_that!(
        turn.messages,
        elements_are![
            matches_pattern!(Message {
                source: eq(&MessageSource::Steer),
                text: eq("Also put the word STEERED at the end of that line."),
                ..
            }),
            matches_pattern!(Message {
                role: eq(&Role::Assistant),
                source: eq(&MessageSource::Delegate),
                text: eq("LIVE_DONE STEERED"),
                ..
            }),
        ]
    );
    assert_that!(steers_taken, eq(&input.steers));
    assert_that!(turn_input, eq(TurnInput::Closed));
    assert_that!(turn.outcome, eq(&Outcome::Running));
    assert_that!(
        last_result,
        some(matches_pattern!(Outcome::Completed { .. }))
    );
    assert_that!(turn.unrecognised.summary(), none());
}

/// An echo whose id names neither the question nor a steer is not a steer, whatever it says: it
/// renders by its content, as any `user` text does, and no steer is taken by it.
#[gtest]
fn a_foreign_echo_is_never_a_steer() {
    let Fold {
        turn, steers_taken, ..
    } = agentmux::vendor::claude::fold::fold_stream_input(
        0,
        "q",
        fixtures::CLAUDE_STEERED,
        &StreamInput::default(),
    );

    assert_that!(steers_taken, is_empty());
    assert_that!(
        turn.messages
            .iter()
            .filter(|message| message.source == MessageSource::Steer)
            .count(),
        eq(0)
    );
    assert_that!(
        turn.messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.text.clone())
            .collect::<Vec<_>>(),
        elements_are![
            starts_with("Run the shell command `sleep 20`"),
            eq("Also put the word STEERED at the end of that line.")
        ]
    );
}

/// A steer Claude takes after its `result` runs as a further CLI turn, with an `init` and a
/// `result` of its own: until it is answered no outcome stands, and once it is, its answer is the
/// turn's, with the tokens of both CLI turns and the process's latest cost.
#[gtest]
fn a_steer_taken_after_the_result_is_answered_as_the_turns_answer() -> Result<()> {
    let first = formatdoc! {r#"
        {{"type":"system","subtype":"init","session_id":"{SESSION}"}}
        {{"type":"user","message":{{"content":[{{"type":"text","text":"q"}}]}},"parent_tool_use_id":null,"uuid":"{QUESTION_ID}","isReplay":true}}
        {{"type":"assistant","message":{{"content":[{{"type":"text","text":"A story."}}]}},"parent_tool_use_id":null}}
        {{"type":"result","is_error":false,"result":"A story.","usage":{{"input_tokens":1,"output_tokens":10}},"total_cost_usd":0.1}}
    "#};
    let went_on = formatdoc! {r#"
        {{"type":"system","subtype":"init","session_id":"{SESSION}"}}
    "#};
    let second = formatdoc! {r#"
        {{"type":"user","message":{{"content":[{{"type":"text","text":"End it with PELICAN."}}]}},"parent_tool_use_id":null,"uuid":"{STEER_ID}","isReplay":true}}
        {{"type":"assistant","message":{{"content":[{{"type":"text","text":"A story. PELICAN"}}]}},"parent_tool_use_id":null}}
        {{"type":"result","is_error":false,"result":"A story. PELICAN","usage":{{"input_tokens":2,"output_tokens":20}},"total_cost_usd":0.3}}
    "#};
    let input = stream_input()?;
    let fold =
        |events: &str| agentmux::vendor::claude::fold::fold_stream_input(0, "q", events, &input);

    // The first `result` closes the input and would stand.
    let answered = fold(&first);
    assert_that!(answered.input, eq(TurnInput::Closed));
    assert_that!(
        answered.last_result,
        some(matches_pattern!(Outcome::Completed { .. }))
    );
    // Claude went on, so it no longer does, and the input stays closed.
    let going_on = fold(&format!("{first}{went_on}"));
    assert_that!(going_on.last_result, none());
    assert_that!(going_on.input, eq(TurnInput::Closed));
    // The second answers the steer, and stands for both.
    let Fold {
        turn, last_result, ..
    } = fold(&format!("{first}{went_on}{second}"));
    assert_that!(
        turn.messages
            .iter()
            .map(|message| (message.source, message.text.as_str()))
            .collect::<Vec<_>>(),
        elements_are![
            eq(&(MessageSource::Delegate, "A story.")),
            eq(&(MessageSource::Steer, "End it with PELICAN.")),
            eq(&(MessageSource::Delegate, "A story. PELICAN")),
        ]
    );
    assert_that!(
        last_result,
        some(matches_pattern!(Outcome::Completed {
            usage: matches_pattern!(Usage {
                input_tokens: some(eq(&3)),
                output_tokens: some(eq(&30)),
                ..
            }),
            cost_usd: some(eq(&0.3)),
        }))
    );
    Ok(())
}

/// A failure Claude reported and then went on past stays in the transcript as a note, so the
/// success after it cannot hide it; and a `result` that cannot be read leaves no earlier one to
/// stand, while it still closes the input.
#[gtest]
fn a_superseded_failure_is_kept_and_an_unreadable_result_stands_for_nothing() -> Result<()> {
    let failed_then_answered = formatdoc! {r#"
        {{"type":"system","subtype":"init","session_id":"{SESSION}","apiKeySource":"ANTHROPIC_API_KEY"}}
        {{"type":"result","is_error":true,"api_error_status":529,"result":"API Error: overloaded_error"}}
        {{"type":"system","subtype":"init","session_id":"{SESSION}","apiKeySource":"ANTHROPIC_API_KEY"}}
        {{"type":"assistant","message":{{"content":[{{"type":"text","text":"The answer."}}]}},"parent_tool_use_id":null}}
        {{"type":"result","is_error":false,"result":"The answer."}}
    "#};
    let input = stream_input()?;
    let Fold {
        turn, last_result, ..
    } = agentmux::vendor::claude::fold::fold_stream_input(0, "q", &failed_then_answered, &input);
    assert_that!(
        last_result,
        some(matches_pattern!(Outcome::Completed { .. }))
    );
    let rendered = render_turn(&turn);
    assert_that!(
        rendered,
        contains_substring("reported a failure here, then went on: model overloaded")
    );
    // Each further CLI turn announces itself again, and the key is noted once.
    assert_that!(rendered.matches("apiKeySource").count(), eq(1));

    let unreadable = formatdoc! {r#"
        {{"type":"system","subtype":"init","session_id":"{SESSION}"}}
        {{"type":"result","is_error":false,"result":"The answer."}}
        {{"type":"result","subtype":{{"kind":"success"}}}}
    "#};
    let Fold {
        turn,
        input: turn_input,
        last_result,
        ..
    } = agentmux::vendor::claude::fold::fold_stream_input(0, "q", &unreadable, &input);
    assert_that!(last_result, none());
    assert_that!(turn_input, eq(TurnInput::Closed));
    assert_that!(turn.unrecognised.summary(), some(anything()));
    Ok(())
}

/// A hook that reopened one CLI turn says nothing of the next: what Claude answered after a
/// steer it took past that turn's `result` is labelled the answer, where what followed the
/// reopening is not.
#[gtest]
fn a_reopening_ends_with_the_cli_turn_it_reopened() -> Result<()> {
    let events = formatdoc! {r#"
        {{"type":"system","subtype":"init","session_id":"{SESSION}"}}
        {{"type":"assistant","message":{{"content":[{{"type":"text","text":"THE_REPORT_BODY"}}]}},"parent_tool_use_id":null}}
        {{"type":"user","message":{{"content":[{{"type":"text","text":"Stop hook feedback: reconsider."}}]}},"parent_tool_use_id":null}}
        {{"type":"assistant","message":{{"content":[{{"type":"text","text":"Reconsidered."}}]}},"parent_tool_use_id":null}}
        {{"type":"result","is_error":false,"result":"Reconsidered."}}
        {{"type":"system","subtype":"init","session_id":"{SESSION}"}}
        {{"type":"user","message":{{"content":[{{"type":"text","text":"Check the tests too."}}]}},"parent_tool_use_id":null,"uuid":"{STEER_ID}","isReplay":true}}
        {{"type":"assistant","message":{{"content":[{{"type":"text","text":"The tests pass."}}]}},"parent_tool_use_id":null}}
        {{"type":"result","is_error":false,"result":"The tests pass."}}
    "#};
    let Fold { turn, .. } =
        agentmux::vendor::claude::fold::fold_stream_input(0, "q", &events, &stream_input()?);
    let rendered = render_turn(&turn);
    assert_that!(
        rendered,
        contains_substring("### assistant (after hook reopen — not the answer)\n\nReconsidered.")
    );
    assert_that!(
        rendered,
        contains_substring("### assistant\n\nThe tests pass.")
    );
    Ok(())
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
            .any(|message| message.text.contains("Switch to another model")),
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
        .usage_window
        .clone()
        .expect("the recorded stream carries one");
    assert_that!(limit.resets_at.timestamp(), eq(1_788_768_000));
    assert_that!(
        limit.window.as_deref(),
        some(eq("seven_day_overage_included"))
    );
    // The stream said `rejected`, so the window is closed until it reopens.
    assert_that!(limit.exhausted, eq(true));

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
    for fixture in fixtures::all() {
        let Fold { turn, .. } = fixture.fold(fixture.stream);
        assert_that!(
            turn.unrecognised.summary(),
            none(),
            "fixture {} carries an event type the {} parser does not name",
            fixture.name,
            fixture.vendor
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
        "/tmp/claude-",
        "-scratchpad",
        // A memory slug encodes the absolute checkout path, so any slug that starts at a
        // filesystem root came from a real machine rather than the mocked `-work-project`.
        ".claude-personal/projects/-Users",
        ".claude-personal/projects/-Volumes",
        ".claude-personal/projects/-home",
        ".claude-personal/projects/-private",
    ];

    for Fixture { name, stream, .. } in fixtures::all() {
        for marker in machine_identity {
            // Asserting on the boolean rather than the fixture keeps a failure readable; matching
            // the fixture itself would print the whole recorded stream.
            assert_that!(
                stream.contains(marker),
                eq(false),
                "fixture {name} still carries {marker} from the machine that recorded it; \
                 replace it with a mocked value before committing"
            );
        }
        // A home directory is the mocked user's, `/home/dev`, or the recording user's.
        let foreign_homes: Vec<&str> = stream
            .match_indices("/home/")
            .map(|(at, _)| stream.get(at..).unwrap_or_default())
            .map(|home| home.split(['/', '"', '\\']).nth(2).unwrap_or_default())
            .filter(|user| *user != "dev")
            .collect();
        assert_that!(
            foreign_homes,
            is_empty(),
            "fixture {name} carries a home directory from the machine that recorded it; \
             replace it with /home/dev before committing"
        );
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
            .filter(|message| message.source == MessageSource::HookInjection)
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
            .filter(|message| message.is_report_content())
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
    let Fold { turn, session, .. } = stream::fold(
        Vendor::Claude,
        1,
        "What was the word?",
        fixtures::CLAUDE_RESUME,
    );

    assert_that!(turn.was_reopened_by_hook(), eq(false));
    assert_that!(
        turn.messages
            .iter()
            .filter(|message| message.is_report_content())
            .map(|message| message.text.clone())
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

/// The render of every fixture is append-only over every prefix of its stream.
///
/// `tail` pages the rendered transcript by byte offset, so appending an event to a stream may only
/// ever append bytes to its render.
/// Each parser holds that by discipline in several places — the `break` on a terminal event, the
/// footer written only once terminal, the labels that depend only on what came before — and this
/// is the one test that covers every path at once: for every recorded stream and every line cut,
/// the render of the shorter prefix is a prefix of the render of the longer one.
#[gtest]
fn every_fixture_renders_monotonically_over_its_prefixes() {
    for fixture in fixtures::all() {
        expect_monotonic_render(&fixture);
    }
    // What the app-server driver writes and `codex exec` never does: questions and a steer.
    expect_monotonic_render(&Fixture {
        vendor: Vendor::Codex,
        name: "driven question capture",
        stream: DRIVEN_QUESTIONS,
        input: None,
    });
}

/// Check that the render of `fixture` at every line cut extends the render of the cut before it.
fn expect_monotonic_render(fixture: &Fixture) {
    let Fixture { name, stream, .. } = fixture;
    let lines: Vec<&str> = stream.lines().collect();
    let mut previous = String::new();
    for cut in 0..=lines.len() {
        let prefix: String = lines
            .get(..cut)
            .unwrap_or_default()
            .iter()
            .flat_map(|line| [*line, "\n"])
            .collect();
        let Fold { turn, .. } = fixture.fold(&prefix);
        let rendered = agentmux::transcript::render_turn(&turn);
        expect_that!(
            rendered.starts_with(&previous),
            eq(true),
            "{name}: the render of {cut} lines does not extend the render of {} lines",
            cut.saturating_sub(1)
        );
        previous = rendered;
    }
}

/// A turn driven over `codex app-server` in which the delegate asked twice, was steered, and
/// answered, as the driver writes it.
const DRIVEN_QUESTIONS: &str = indoc::indoc! {r#"
    {"type":"thread.started","thread_id":"01a1a800-7700-70b2-846b-0ac1e34ddaa7"}
    {"type":"turn.started"}
    {"type":"item.completed","item":{"id":"msg_1","type":"agent_message","text":"A probe found a boundary."}}
    {"type":"agentmux.question","id":"call_1","asked_at":"2026-11-05T03:06:13Z","questions":[{"title":"Assume valid names?","options":["Yes (Recommended)","No"]},{"title":"Which chart?"}]}
    {"type":"item.completed","item":{"id":"exec-1","type":"command_execution","command":"ls","aggregated_output":"","exit_code":0,"status":"completed"}}
    {"type":"agentmux.question","id":"call_2","asked_at":"2026-11-05T03:09:00Z","questions":[{"title":"May I run a Cargo gate?","options":["Allow (Recommended)","Keep the limit"]}]}
    {"type":"agentmux.steer","text":"Assume valid names; keep the limit."}
    {"type":"item.completed","item":{"id":"msg_2","type":"agent_message","text":"Recorded the contract."}}
    {"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}
"#};

/// Questions a delegate asked are kept twice over: as messages where they were asked, in the
/// report, and as numbered questions a caller can act on.
///
/// Numbering runs across every call in the turn, so the second call's question is the turn's
/// third, and a steer after them changes neither: agentmux cannot tell what it answered.
#[gtest]
fn a_codex_question_is_numbered_kept_in_the_report_and_rendered() -> Result<()> {
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", DRIVEN_QUESTIONS);

    // Every question, numbered in the order asked, with its options and when it was asked.
    let asked: Vec<(u32, &str, usize)> = turn
        .delegate_questions
        .iter()
        .map(|question| {
            (
                question.number,
                question.title.as_str(),
                question.options.len(),
            )
        })
        .collect();
    assert_that!(
        asked,
        elements_are![
            eq(&(1, "Assume valid names?", 2)),
            eq(&(2, "Which chart?", 0)),
            eq(&(3, "May I run a Cargo gate?", 2)),
        ]
    );
    assert_that!(
        turn.delegate_questions
            .first()
            .map(|question| question.asked_at.to_rfc3339()),
        some(eq("2026-11-05T03:06:13+00:00"))
    );

    // Each call is one message, in codex's own layout, between the replies around it, and part
    // of the report.
    let sources: Vec<MessageSource> = turn.messages.iter().map(|message| message.source).collect();
    assert_that!(
        sources,
        elements_are![
            eq(&MessageSource::Delegate),
            eq(&MessageSource::Question),
            eq(&MessageSource::Question),
            eq(&MessageSource::Steer),
            eq(&MessageSource::Delegate),
        ]
    );
    let first = turn.messages.get(1).or_fail()?;
    assert_that!(
        first.text,
        eq("Assume valid names?\n- Yes (Recommended)\n- No\n\nWhich chart?")
    );
    assert_that!(first.is_report_content(), eq(true));
    assert_that!(turn.unrecognised.summary(), none());

    // The transcript marks each as a question, where it was asked.
    let rendered = agentmux::transcript::render_turn(&turn);
    assert_that!(
        rendered,
        contains_substring(indoc::indoc! {"
            ### assistant (question)

            Assume valid names?
            - Yes (Recommended)
            - No

            Which chart?
        "})
    );
    Ok(())
}

/// A top-level Codex `error` is a note where it arrived, not the end of the turn.
///
/// codex-cli 0.159.3 writes one for every error its server reports, including one it retries
/// (`codex-rs/exec/src/event_processor_with_jsonl_output.rs`), and the turn then goes on: ending
/// the fold there would freeze a failure over a turn that went on to answer.
#[gtest]
fn a_codex_top_level_error_is_a_note_and_the_turn_goes_on() {
    let retried = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"error","message":"Reconnecting... 1/5 (stream disconnected before completion)"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"the answer"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", retried);

    // The turn completed, the answer is kept, and the retry is said once, before it.
    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
    let message_texts: Vec<&str> = turn
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect();
    assert_that!(
        message_texts,
        elements_are![
            contains_substring("stream disconnected before completion"),
            eq(&"the answer")
        ]
    );

    // A turn that fails after an error ends with the failure `turn.failed` reports.
    let failing = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"error","message":"stream disconnected before completion"}
        {"type":"turn.failed","error":{"message":"the final word"}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", failing);
    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            kind: anything(),
            detail: eq("the final word"),
        })
    );
}

/// A sub-agent tool call, as codex-cli 0.159.3's `codex exec --json` writes it, is narration the
/// fold recognises, while an item kind no codex release names is still counted as drift.
///
/// The event is copied from a real consultation's capture.
#[gtest]
fn a_codex_collab_tool_call_is_recognised_and_an_unknown_kind_is_still_counted() {
    let stream = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.started","item":{"id":"item_47","type":"collab_tool_call","tool":"wait","sender_thread_id":"01a0f977-00af-7a03-8eac-0e28ec77013d","receiver_thread_ids":[],"prompt":null,"agents_states":{},"status":"in_progress"}}
        {"type":"item.completed","item":{"id":"item_47","type":"collab_tool_call","tool":"wait","sender_thread_id":"01a0f977-00af-7a03-8eac-0e28ec77013d","receiver_thread_ids":[],"prompt":null,"agents_states":{},"status":"completed"}}
        {"type":"item.completed","item":{"id":"item_48","type":"telepathy","status":"completed"}}
        {"type":"item.completed","item":{"id":"item_49","type":"agent_message","text":"done"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", stream);

    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(
        turn.unrecognised.summary(),
        some(eq("item.completed.telepathy x1"))
    );
}

/// A text block whose payload field was renamed is counted, not read as an empty message.
///
/// Defaulting the text to empty would drop every report the day the vendor renames the field,
/// with a clean exit and no diagnostic — the silent loss the drift counter exists to prevent.
#[gtest]
fn a_renamed_text_payload_is_counted_not_dropped() {
    let stream = indoc::indoc! {r#"
        {"type":"system","subtype":"init","session_id":"85ff2d6a-ec24-47cf-a4f8-486a5bb06814"}
        {"type":"assistant","message":{"content":[{"type":"text","value":"THE_REPORT_BODY"}]},"parent_tool_use_id":null}
        {"type":"result","is_error":false,"subtype":"success","terminal_reason":"completed","result":""}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", stream);

    assert_that!(
        turn.unrecognised.summary(),
        some(contains_substring("text.<missing>"))
    );
    assert_that!(turn.messages, is_empty());
}

/// A `result` without its success flag is drift, not a clean completion.
///
/// Defaulting `is_error` to false would turn a renamed field into a "completed" turn with no
/// report, and nothing would say so.
#[gtest]
fn a_result_without_its_error_flag_is_drift_not_success() {
    let stream = indoc::indoc! {r#"
        {"type":"system","subtype":"init","session_id":"85ff2d6a-ec24-47cf-a4f8-486a5bb06814"}
        {"type":"assistant","message":{"content":[{"type":"text","text":"THE_REPORT_BODY"}]},"parent_tool_use_id":null}
        {"type":"result","subtype":"success","terminal_reason":"completed","result":""}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", stream);

    assert_that!(turn.outcome, matches_pattern!(Outcome::Running));
    assert_that!(
        turn.unrecognised.summary(),
        some(contains_substring("result.<unparsable>"))
    );
}

/// A hook that adds context while the delegate is still working is not a reopening.
///
/// Under inherited settings a `PostToolUse` hook injects a `user` text message mid-turn, after a
/// message that called a tool.
/// Reading that as "the turn was reopened" would label the delegate's real answer, which comes
/// afterwards, as not the answer.
#[gtest]
fn context_injected_mid_turn_is_not_a_reopening() {
    let stream = indoc::indoc! {r#"
        {"type":"system","subtype":"init","session_id":"85ff2d6a-ec24-47cf-a4f8-486a5bb06814"}
        {"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]},"parent_tool_use_id":null}
        {"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"file contents"}]},"parent_tool_use_id":null}
        {"type":"user","message":{"content":[{"type":"text","text":"PostToolUse hook: remember the house style."}]},"parent_tool_use_id":null}
        {"type":"assistant","message":{"content":[{"type":"text","text":"THE_REPORT_BODY"}]},"parent_tool_use_id":null}
        {"type":"result","is_error":false,"subtype":"success","terminal_reason":"completed","result":"THE_REPORT_BODY"}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", stream);

    assert_that!(turn.was_reopened_by_hook(), eq(false));
    assert_that!(
        turn.messages
            .iter()
            .filter(|message| message.source == MessageSource::HookContext)
            .count(),
        eq(1)
    );
    let rendered = agentmux::transcript::render_turn(&turn);
    assert_that!(rendered, contains_substring("added context here"));
    assert_that!(rendered, not(contains_substring("not the answer")));
}

/// A connection refused is not a content refusal.
///
/// The classifier also reads the tail of a CLI's stderr, and a caller told "content flagged"
/// about a proxy error would rephrase its question instead of fixing its proxy.
#[gtest]
fn an_infrastructure_error_is_not_classified_as_a_refusal() {
    let stream = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.failed","error":{"message":"connect ECONNREFUSED 127.0.0.1:8080 (request id req_4291)"}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", stream);

    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::Unclassified),
            detail: anything()
        })
    );
}

/// A resume against a session the CLI no longer holds still says which session it meant.
///
/// Measured: `claude --resume <id>` against an expired conversation writes
/// `No conversation found with session ID: <id>` to stderr and emits exactly one event, a
/// `result` with `subtype: "error_during_execution"` — no `system`/`init` at all.
/// That event carries `session_id`, so the turn is not one whose continuity is unknowable; read
/// only from `init`, it looked like one, and the run was reported as drift ("session.<not
/// announced>") on top of the failure it already was.
#[gtest]
fn a_resume_whose_session_is_gone_still_names_the_session() {
    let stream = indoc::indoc! {r#"
        {"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"session_id":"9f1c21f8-1b3d-4a7e-9a0c-2d6f4b8e5c10","total_cost_usd":0}
    "#};

    let Fold { turn, session, .. } = stream::fold(Vendor::Claude, 1, "q", stream);

    assert_that!(
        session.map(|s| s.as_str().to_owned()),
        some(eq("9f1c21f8-1b3d-4a7e-9a0c-2d6f4b8e5c10")),
        "the only event carrying the session was not read"
    );
    assert_that!(turn.outcome, matches_pattern!(Outcome::Failed { .. }));
    // The failure is the whole story: nothing about the stream was unrecognised.
    assert_that!(turn.unrecognised.summary(), none());
}

/// An `init` that announced a session outranks the one the terminal event echoes.
///
/// They agree on a healthy turn, so the fallback must not be able to change which conversation a
/// follow-up resumes when both are present.
#[gtest]
fn the_announced_session_outranks_the_one_the_result_echoes() {
    let stream = indoc::indoc! {r#"
        {"type":"system","subtype":"init","session_id":"11111111-1111-4111-8111-111111111111"}
        {"type":"result","subtype":"success","is_error":false,"session_id":"22222222-2222-4222-8222-222222222222"}
    "#};

    let Fold { session, .. } = stream::fold(Vendor::Claude, 0, "q", stream);

    assert_that!(
        session.map(|s| s.as_str().to_owned()),
        some(eq("11111111-1111-4111-8111-111111111111"))
    );
}

/// Backgrounded-task and heartbeat events are named, so a slow tool call is not reported as drift.
///
/// Every one was met in a real capture. None carries any part of the answer — a task's output
/// comes back as an ordinary `tool_result`, and the rest are status patches, task lists and
/// elapsed times — but an event type nobody names is counted, and a consultation that merely ran a
/// long command would tell its caller that agentmux's model of the stream had moved.
#[gtest]
fn task_lifecycle_and_progress_events_are_named_and_silent() {
    let stream = indoc::indoc! {r#"
        {"type":"system","subtype":"init","session_id":"33333333-3333-4333-8333-333333333333"}
        {"type":"system","subtype":"task_started","task_id":"t1","tool_use_id":"toolu_1","description":"Diff stat","task_type":"local_bash","is_backgrounded":false}
        {"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"t1","task_type":"local_bash","description":"Diff stat"}]}
        {"type":"tool_progress","tool_use_id":"toolu_1-heartbeat-0","tool_name":"Bash","parent_tool_use_id":"toolu_1","elapsed_time_seconds":30,"heartbeat":true}
        {"type":"system","subtype":"task_updated","task_id":"t1","patch":{"status":"completed","end_time":1789306550116}}
        {"type":"system","subtype":"task_notification","task_id":"t1","tool_use_id":"toolu_1","status":"completed","output_file":"","summary":"Diff stat"}
        {"type":"assistant","message":{"model":"claude-opus-5","role":"assistant","content":[{"type":"text","text":"ANSWER"}]}}
        {"type":"result","subtype":"success","is_error":false}
    "#};

    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", stream);

    assert_that!(turn.unrecognised.summary(), none());
    // Named, not merely tolerated: none of them is allowed to become transcript content.
    assert_that!(
        turn.messages
            .iter()
            .filter(|message| message.is_report_content())
            .map(|message| message.text.clone())
            .collect::<Vec<_>>(),
        elements_are![eq("ANSWER")]
    );
}

/// A Claude delegate that authenticated with an API key says so in its own transcript.
///
/// agentmux hands a subscription delegate no key, but the CLI has sources of its own — a
/// settings file an inheriting delegate loads, or a login made with a Console key — and its `init`
/// event is its own report of which one it used.
/// A logged-in subscription reports `none`, and says nothing.
#[gtest]
fn a_claude_turn_on_an_api_key_is_marked_in_the_transcript() {
    let keyed = indoc::indoc! {r#"
        {"type":"system","subtype":"init","session_id":"85ff2d6a-ec24-47cf-a4f8-486a5bb06814","apiKeySource":"ANTHROPIC_API_KEY"}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", keyed);
    assert_that!(
        turn.messages
            .iter()
            .map(|message| message.source)
            .collect::<Vec<_>>(),
        elements_are![eq(&MessageSource::Synthetic)]
    );
    assert_that!(
        agentmux::transcript::render_turn(&turn),
        contains_substring("apiKeySource: ANTHROPIC_API_KEY")
    );

    // Every committed fixture ran on a subscription, and none of them carries the note.
    for fixture in fixtures::all() {
        let Fold { turn, .. } = fixture.fold(fixture.stream);
        assert_that!(
            agentmux::transcript::render_turn(&turn),
            not(contains_substring("apiKeySource")),
            "{}",
            fixture.name
        );
    }
}

/// A Codex capture in the shapes an older agentmux wrote folds as it always did.
///
/// The fold re-reads every turn on every `status`, and the render it publishes must not change
/// under a consultation that is already finished, so every line an older translator wrote has to
/// read now exactly as it read then: nothing dropped, nothing newly counted as drift.
#[gtest]
fn a_capture_an_older_agentmux_wrote_folds_as_it_did() {
    // A web search without the action `exec` writes, a tool result without its `_meta`, a
    // command whose fields are not what codex writes now, a note and salvaged replies with no id,
    // a null one or one of another type, and a turn that completed with no usage.
    let completed = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"ws_1","type":"web_search","query":"rust","results":null}}
        {"type":"item.completed","item":{"id":"i2","type":"mcp_tool_call","server":"s","tool":"t","arguments":{},"result":{"content":[],"structured_content":null},"error":null,"status":"completed"}}
        {"type":"item.completed","item":{"id":"i3","type":"command_execution","command":["ls"],"exit_code":"0","status":"timed_out"}}
        {"type":"item.completed","item":{"type":"error","message":"careful"}}
        {"type":"item.completed","item":{"id":null,"type":"agent_message","text":"SALVAGED"}}
        {"type":"item.completed","item":{"id":7,"type":"agent_message","text":"AND THIS"}}
        {"type":"turn.completed","usage":null}
    "#};
    let Fold { turn, session, .. } = stream::fold(Vendor::Codex, 0, "q", completed);

    assert_that!(session, some(anything()));
    assert_that!(turn.unrecognised.summary(), none());
    assert_that!(
        turn.messages
            .iter()
            .map(|message| message.text.clone())
            .collect::<Vec<_>>(),
        elements_are![
            eq("delegate warning: careful"),
            eq("SALVAGED"),
            eq("AND THIS")
        ]
    );
    // No usage is no accounting, not an account of zero tokens.
    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Completed {
            usage: eq(&agentmux::transcript::Usage::default()),
            ..
        })
    );

    // An error and a failure that say nothing readable still say there was one, and the failure
    // still ends the turn.
    let failed = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":5}
        {"type":"error","message":null}
        {"type":"error","message":7}
        {"type":"turn.failed","error":{"message":7}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", failed);
    assert_that!(turn.unrecognised.summary(), none());
    assert_that!(
        turn.messages
            .iter()
            .map(|message| message.text.clone())
            .collect::<Vec<_>>(),
        elements_are![
            eq("delegate error: delegate reported an error"),
            eq("delegate error: delegate reported an error")
        ]
    );
    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            detail: eq("delegate reported an error"),
            ..
        })
    );
}

/// A warning that the window is nearly used is not a refusal: the window is open, and the
/// transcript says when it resets rather than when it reopens.
#[gtest]
fn a_rate_limit_warning_leaves_the_window_open() {
    let events = indoc::indoc! {r#"
        {"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1788727800,"rateLimitType":"five_hour"}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Claude, 0, "q", events);

    let window = turn.usage_window.clone();
    assert_that!(
        window,
        some(field!(
            agentmux::transcript::UsageWindow.exhausted,
            eq(&false)
        ))
    );
    let rendered = agentmux::transcript::render_turn(&turn);
    assert_that!(rendered, contains_substring("five_hour window resets at"));
    assert_that!(rendered, not(contains_substring("reopens")));
}

/// The report-content messages of a turn, in order.
fn report(turn: &agentmux::transcript::Turn) -> Vec<String> {
    turn.messages
        .iter()
        .filter(|message| message.is_report_content())
        .map(|message| message.text.clone())
        .collect()
}

/// agentmux's own notes in a turn, in order.
fn notes(turn: &agentmux::transcript::Turn) -> Vec<String> {
    turn.messages
        .iter()
        .filter(|message| message.source == MessageSource::Synthetic)
        .map(|message| message.text.clone())
        .collect()
}

/// An agy answer streamed in chunks is the step's text, once: the `result` repeating it is not a
/// second copy.
#[gtest]
fn an_agy_answer_is_its_steps_text_once() {
    let Fold { turn, session, .. } = stream::fold(Vendor::Agy, 0, "q", fixtures::AGY_TOOL_USE);

    assert_that!(
        report(&turn),
        elements_are![eq("The exact output is:\n\n```\nPROBE_B\n```\n")]
    );
    assert_that!(
        session.map(|session| session.as_str().to_owned()),
        some(eq("e4c7ded7-2bf0-480b-9326-24191d51e4ba"))
    );
    assert_that!(
        turn.resolved_model.as_deref(),
        some(eq("gemini-3.8-flash-low"))
    );
    // The footer quotes agy's own figures, which count cache reads beside the input.
    assert_that!(
        turn.outcome,
        eq(&Outcome::Completed {
            usage: Usage {
                input_tokens: Some(13646),
                cached_input_tokens: Some(9425),
                cache_write_input_tokens: None,
                output_tokens: Some(53),
                reasoning_output_tokens: Some(0),
            },
            cost_usd: None,
        })
    );
}

/// An agy answer that only the `result` carries, because no step said anything, is the answer.
#[gtest]
fn an_agy_answer_only_its_result_carries_is_the_answer() {
    let stream = indoc::indoc! {r#"
        {"event":"init","init":{"model":"gemini-3.8-flash-low"},"conversation_id":"e4c7ded7-2bf0-480b-9326-24191d51e4ba"}
        {"event":"step_update","step_update":{"conversation_id":"e4c7ded7-2bf0-480b-9326-24191d51e4ba","step_index":1,"state":"DONE","step_type":"agent_response"}}
        {"event":"result","result":{"conversation_id":"e4c7ded7-2bf0-480b-9326-24191d51e4ba","status":"SUCCESS","response":"PELICAN\n"}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Agy, 0, "q", stream);

    assert_that!(report(&turn), elements_are![eq("PELICAN\n")]);
    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
}

/// The reply an agy stream ended in the middle of is not in the turn, whose render must only
/// grow while more of it may arrive, but is handed on for the turn to keep once it has ended.
#[gtest]
fn an_agy_reply_the_stream_ended_in_the_middle_of_is_handed_on() {
    let stream = indoc::indoc! {r#"
        {"event":"init","init":{"model":"gemini-3.8-flash-low"},"conversation_id":"e4c7ded7-2bf0-480b-9326-24191d51e4ba"}
        {"event":"step_update","step_update":{"conversation_id":"e4c7ded7-2bf0-480b-9326-24191d51e4ba","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"Half a "}}
        {"event":"step_update","step_update":{"conversation_id":"e4c7ded7-2bf0-480b-9326-24191d51e4ba","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"reply"}}
    "#};
    let Fold {
        turn, unfinished, ..
    } = stream::fold(Vendor::Agy, 0, "q", stream);

    assert_that!(report(&turn), is_empty());
    assert_that!(unfinished.as_deref(), some(eq("Half a reply")));
}

/// An agy follow-up continues the conversation it resumed.
#[gtest]
fn an_agy_resume_continues_its_conversation() {
    let first = stream::fold(Vendor::Agy, 0, "q", fixtures::AGY_TOOL_USE);
    let second = stream::fold(Vendor::Agy, 1, "q", fixtures::AGY_RESUME);

    assert_that!(second.session, some(anything()));
    assert_that!(second.session, eq(&first.session));
    assert_that!(
        second.turn.outcome,
        matches_pattern!(Outcome::Completed { .. })
    );
    assert_that!(report(&second.turn).len(), eq(1));
}

/// A call agy denied for want of an approval, after which the model said nothing, is a failed
/// turn that names the denial, although agy reported success and exited 0.
#[gtest]
fn an_agy_soft_denial_without_an_answer_fails_the_turn() {
    let Fold { turn, .. } = stream::fold(Vendor::Agy, 0, "q", fixtures::AGY_SOFT_DENIED);

    assert_that!(report(&turn), is_empty());
    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::Unclassified),
            detail: contains_substring("agy denied Bash (`command`)")
        })
    );
    // The transcript says what was denied above the failure.
    assert_that!(
        notes(&turn),
        elements_are![contains_substring("Bash (`command`)")]
    );
}

/// Calls agentmux's deny rules refused are shown where they failed, and a turn that answered
/// anyway completed.
#[gtest]
fn an_agy_rule_denial_is_shown_and_an_answered_turn_completes() {
    let Fold { turn, .. } = stream::fold(Vendor::Agy, 0, "q", fixtures::AGY_RULE_DENIED);

    assert_that!(
        notes(&turn),
        elements_are![
            starts_with("the delegate's `write_file` call failed: permission check failed"),
            starts_with("the delegate's `run_command` call failed: permission check failed"),
        ]
    );
    assert_that!(report(&turn).len(), eq(1));
    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
    // The payload-free `error_message` step the run carries is not drift.
    assert_that!(turn.unrecognised.summary(), none());
}

/// A failed tool call followed by no answer fails the turn, quoting what the call said.
#[gtest]
fn an_agy_failed_call_without_an_answer_fails_the_turn() {
    let stream = indoc::indoc! {r#"
        {"event":"init","conversation_id":"c1","init":{"model":"m"}}
        {"event":"step_update","step_update":{"conversation_id":"c1","step_index":1,"state":"ERROR","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","error":{"type":"TOOL_ERROR","message":"Permission denied for unsandboxed(cargo test)."}}}}
        {"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":""}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Agy, 0, "q", stream);

    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::Unclassified),
            detail: contains_substring("Permission denied for unsandboxed(cargo test).")
        })
    );
}

/// agy waits for a backgrounded command before its one `result`, and every answer it gave on
/// the way is kept, the payload-free `error_message` step in between meaning nothing.
#[gtest]
fn an_agy_background_task_keeps_both_answers() {
    let Fold { turn, .. } = stream::fold(Vendor::Agy, 0, "q", fixtures::AGY_BACKGROUND);

    assert_that!(
        report(&turn),
        elements_are![
            eq("started\n"),
            eq("The background task has finished successfully with output: `BG_DONE`.\n")
        ]
    );
    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
}

/// An unknown agy model is a failure of its own kind, with agy's list of models in the detail,
/// and no session to resume.
#[gtest]
fn an_unknown_agy_model_is_model_unavailable() {
    let Fold { turn, session, .. } = stream::fold(Vendor::Agy, 0, "q", fixtures::AGY_UNKNOWN_MODEL);

    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::ModelUnavailable),
            detail: contains_substring("Available models:")
        })
    );
    assert_that!(session, none());
}

/// Every other agy error stays unclassified, an overload included: a classified overload would
/// have agentmux resume the consultation at a cost, on a guess at agy's wording.
#[gtest]
fn an_agy_error_other_than_an_unknown_model_is_not_classified() {
    for error in [
        // agy's own words for a model out of capacity, as a run reported them.
        "API error (attempt 2): transport error (Unavailable, HTTP 503): No capacity available \
         for model gemini-3.8-flash-low on the server",
        // Words the other vendors' streams are read as an overload by, and agy's are not.
        "upstream: the model is overloaded",
    ] {
        let stream = format!(
            "{}\n",
            serde_json::json!({"event": "result", "result": {
                "conversation_id": "c1", "status": "ERROR", "response": "", "error": error,
            }})
        );
        let Fold { turn, .. } = stream::fold(Vendor::Agy, 0, "q", &stream);

        assert_that!(
            turn.outcome,
            eq(&Outcome::Failed {
                kind: FailureKind::Unclassified,
                detail: error.to_owned(),
            }),
            "{error}"
        );
    }
}

/// A step type the agy parser does not name is counted, not skipped.
#[gtest]
fn an_unknown_agy_step_type_is_counted() {
    let stream = indoc::indoc! {r#"
        {"event":"step_update","step_update":{"conversation_id":"c1","step_index":1,"state":"DONE","step_type":"checkpoint"}}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Agy, 0, "q", stream);

    assert_that!(
        turn.unrecognised.summary(),
        some(eq("step_update.checkpoint x1"))
    );
}

/// A pi turn is the text of each answer pi completed, once, from the session pi saved, by the
/// model that answered; its spend is both model calls', the cache counted beside the input.
#[gtest]
fn a_pi_turn_is_its_completed_answers_once() {
    let Fold {
        turn,
        session,
        input,
        ..
    } = stream::fold(Vendor::Pi, 0, "q", fixtures::PI_TOOL_USE);

    assert_that!(
        report(&turn),
        elements_are![eq("The file mentions the colour teal.")]
    );
    assert_that!(
        session.map(|session| session.as_str().to_owned()),
        some(eq("01a222df-e34a-76d1-9e3f-380c59ae8f6d"))
    );
    assert_that!(
        turn.resolved_model.as_deref(),
        some(eq("ollama/qwen3.6:27b-64k"))
    );
    assert_that!(input, eq(TurnInput::Closed));
    assert_that!(
        turn.outcome,
        eq(&Outcome::Completed {
            usage: Usage {
                input_tokens: Some(4164),
                cached_input_tokens: Some(4252),
                cache_write_input_tokens: Some(0),
                output_tokens: Some(146),
                reasoning_output_tokens: Some(0),
            },
            cost_usd: None,
        })
    );
}

/// A pi turn takes a steer from its run's start until it settles, and an answer still streaming
/// when the capture ends is not in the turn but handed on, for the turn to keep once it has ended.
#[gtest]
fn a_running_pi_turn_takes_input_and_hands_on_its_streaming_answer() -> Result<()> {
    let lines: Vec<&str> = fixtures::PI_TOOL_USE.lines().collect();
    // Up to the closing answer's last delta, before pi completes it.
    let cut = lines
        .iter()
        .rposition(|line| line.contains(r#""type":"text_delta""#))
        .or_fail()?;
    let stream = format!("{}\n", lines.get(..=cut).or_fail()?.join("\n"));
    let Fold {
        turn,
        input,
        unfinished,
        ..
    } = stream::fold(Vendor::Pi, 0, "q", &stream);

    assert_that!(input, eq(TurnInput::Open));
    assert_that!(turn.outcome, eq(&Outcome::Running));
    assert_that!(report(&turn), is_empty());
    assert_that!(
        unfinished.as_deref(),
        some(starts_with("The file mentions"))
    );
    Ok(())
}

/// A pi turn whose last answer is a provider error fails with pi's own message once it settles,
/// after the retries pi announced, which stay in the turn as warnings; one pi aborted fails too.
#[gtest]
fn a_settled_pi_turn_fails_with_its_last_answers_error() {
    let failed = indoc::indoc! {r#"
        {"type":"agent_start"}
        {"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"Connection error.: bad port"}}
        {"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000,"errorMessage":"Connection error.: bad port"}
        {"type":"auto_retry_end","success":false,"attempt":3,"finalError":"Connection error.: bad port"}
        {"type":"agent_settled","aborted":false}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Pi, 0, "q", failed);

    assert_that!(
        turn.outcome,
        eq(&Outcome::Failed {
            kind: FailureKind::Unclassified,
            detail: "Connection error.: bad port".to_owned(),
        })
    );
    assert_that!(
        notes(&turn),
        elements_are![
            contains_substring("pi retries, attempt 1 of 3"),
            contains_substring("pi gave up retrying")
        ]
    );

    let aborted = indoc::indoc! {r#"
        {"type":"agent_start"}
        {"type":"agent_settled","aborted":true}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Pi, 0, "q", aborted);
    assert_that!(
        turn.outcome,
        matches_pattern!(Outcome::Failed {
            detail: eq("pi aborted the turn"),
            ..
        })
    );
}

/// What a pi turn spent is every usage pi reported, each once: a completed answer's, a
/// compaction's and a cache refresh's, never a streaming update's running figure; a steer the
/// driver recorded is the caller's, where pi took it.
#[gtest]
fn every_usage_a_pi_turn_reports_counts_once() {
    let stream = indoc::indoc! {r#"
        {"type":"agent_start"}
        {"type":"message_update","usage":{"input":10,"output":1},"assistantMessageEvent":{"type":"text_delta","delta":"A"}}
        {"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"A"}],"stopReason":"stop","usage":{"input":10,"output":2}}}
        {"type":"message_start","message":{"role":"user","content":"Also B."}}
        {"type":"agentmux.steer","text":"Also B."}
        {"type":"entry_appended","entry":{"type":"context_edit","id":"e1"}}
        {"type":"entry_appended","entry":{"type":"usage","kind":"cache_warm","usage":{"cacheRead":50}}}
        {"type":"compaction_end","reason":"threshold","result":{"summary":"s","usage":{"input":5,"output":1}},"aborted":false,"willRetry":false}
        {"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"B"}],"stopReason":"stop"}}
        {"type":"agent_settled","aborted":false}
    "#};
    let Fold { turn, .. } = stream::fold(Vendor::Pi, 0, "q", stream);

    assert_that!(
        turn.messages
            .iter()
            .filter(|message| message.source == MessageSource::Steer)
            .map(|message| message.text.clone())
            .collect::<Vec<_>>(),
        elements_are![eq("Also B.")]
    );
    assert_that!(
        turn.outcome,
        eq(&Outcome::Completed {
            usage: Usage {
                input_tokens: Some(15),
                cached_input_tokens: Some(50),
                cache_write_input_tokens: None,
                output_tokens: Some(3),
                reasoning_output_tokens: None,
            },
            cost_usd: None,
        })
    );
}
