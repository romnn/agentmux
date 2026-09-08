//! A consultation end to end, through the scripted launcher: no credentials, no network.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentmux::delegate::{AccountAlias, CodexSandbox, Delegate, Effort, Isolation, ModelId};
use agentmux::run::{
    HookReopening, NotResumable, Retention, RunError, RunId, RunStore, StartRequest,
};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use agentmux::transcript::{FailureKind, Outcome};
use googletest::prelude::*;

fn env() -> BTreeMap<String, String> {
    [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn claude() -> Result<Delegate> {
    Ok(Delegate::Claude {
        model: ModelId::parse("claude-opus-5").or_fail()?,
        effort: Some(Effort::parse("xhigh").or_fail()?),
        account: None,
        isolation: None,
    })
}

fn codex() -> Result<Delegate> {
    Ok(Delegate::Codex {
        model: ModelId::parse("gpt-6-astra").or_fail()?,
        effort: Some(Effort::parse("high").or_fail()?),
        sandbox: CodexSandbox::ReadOnly,
        account: None,
        isolation: None,
    })
}

struct Harness {
    store: RunStore,
    launcher: Arc<ScriptedLauncher>,
    _dir: tempfile::TempDir,
}

fn harness(scripts: impl IntoIterator<Item = Script>) -> Result<Harness> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new(scripts));
    let store = RunStore::open(dir.path(), launcher.clone(), env()).or_fail()?;
    Ok(Harness {
        store,
        launcher,
        _dir: dir,
    })
}

fn request(delegate: Delegate, question: &str) -> StartRequest {
    StartRequest {
        delegate,
        question: question.to_owned(),
        cwd: PathBuf::from("/work/project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
    }
}

/// Path traversal through a tool argument is a state that cannot be constructed.
#[gtest]
fn a_run_id_cannot_escape_the_run_directory() {
    for hostile in [
        "..",
        "../..",
        "../../etc/passwd",
        "/etc/passwd",
        "runs/../../etc",
        r"..\..\windows",
        "a/b",
        "a\0b",
        "a\nb",
        "a b",
        ".",
        "",
        "run.id",
        "run~id",
    ] {
        assert_that!(
            RunId::parse(hostile),
            err(anything()),
            "{hostile:?} must be rejected"
        );
    }
    assert_that!(
        RunId::parse(&"a".repeat(RunId::MAX_LEN + 1)),
        err(anything())
    );

    // What a real id looks like, and what a caller may legitimately hand back.
    assert_that!(RunId::parse("20260906T195411-a3f9c1d2"), ok(anything()));
    assert_that!(RunId::parse(RunId::generate().as_str()), ok(anything()));
}

/// A consultation is a conversation: the run id does not change when it continues.
#[gtest]
fn a_follow_up_appends_a_turn_to_the_same_run() -> Result<()> {
    let second_turn = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"BANANAPHONE"}}
        {"type":"turn.completed","usage":{"input_tokens":19000,"output_tokens":8}}
    "#};
    let h = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(second_turn),
    ])?;

    let started = h.store.start(&request(
        codex()?,
        "Remember BANANAPHONE. Reply FIXTURE_OK.",
    ))?;
    assert_that!(started.turns, eq(1));
    assert_that!(started.resumable, eq(true));

    let followed = h
        .store
        .follow_up(&started.run_id, "What was the secret word?")?;
    assert_that!(followed.run_id, eq(&started.run_id));
    assert_that!(followed.turns, eq(2));

    // The second launch resumes the delegate's own session rather than restating the brief.
    let launches = h.launcher.launches();
    assert_that!(launches.len(), eq(2));
    let resume = launches.get(1).expect("second launch");
    assert_that!(resume.has_flag("resume"), eq(true));
    assert_that!(
        resume.has_flag_with("resume", "01a0784a-915b-7d92-a381-b53d765296c4"),
        eq(true)
    );
    assert_that!(resume.question, eq("What was the secret word?"));

    // Both turns are in one transcript, in order.
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("FIXTURE_OK"));
    assert_that!(page.text, contains_substring("BANANAPHONE"));
    assert_that!(page.text, contains_substring("Turn 1 — question"));
    assert_that!(page.text, contains_substring("Turn 2 — question"));
    Ok(())
}

/// The transcript length taken before a follow-up is exactly where the new turn begins.
///
/// This is what lets `follow_up` page the new turn alone instead of re-sending every earlier one
/// to a caller that already has them.
/// The rendered transcript is append-only, so bytes measured before the turn is claimed cut
/// between turns rather than through the middle of one.
///
/// Were that to stop holding, a follow-up would answer with the tail of the previous turn glued
/// to the front of the new one.
#[gtest]
fn the_bytes_before_a_follow_up_cut_cleanly_between_turns() -> Result<()> {
    let second_turn = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"BANANAPHONE"}}
        {"type":"turn.completed","usage":{"input_tokens":19000,"output_tokens":8}}
    "#};
    let h = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(second_turn),
    ])?;

    let started = h.store.start(&request(
        codex()?,
        "Remember BANANAPHONE. Reply FIXTURE_OK.",
    ))?;
    let before = started.transcript_bytes;

    h.store
        .follow_up(&started.run_id, "What was the secret word?")?;
    let page = h.store.page(&started.run_id, before, 1_000_000)?;

    assert_that!(page.text, contains_substring("Turn 2 — question"));
    assert_that!(page.text, contains_substring("BANANAPHONE"));
    assert_that!(page.text, not(contains_substring("Turn 1 — question")));
    assert_that!(page.text, not(contains_substring("FIXTURE_OK")));
    Ok(())
}

/// A follow-up must not silently start a fresh conversation.
#[gtest]
fn a_follow_up_is_refused_when_there_is_nothing_to_resume() -> Result<()> {
    let h = harness([Script::exited("", "claude: command not found\n", 127)])?;
    let started = h.store.start(&request(claude()?, "review this"))?;
    assert_that!(
        started.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::LaunchFailed),
            detail: anything()
        })
    );

    let refused = h.store.follow_up(&started.run_id, "and now?");
    assert_that!(refused.map(|_| ()), err(anything()));
    // Only the original launch happened; no second child was started against a dead session.
    assert_that!(h.launcher.launches().len(), eq(1));
    Ok(())
}

/// A child that dies without a terminal event must say what the CLI said.
#[gtest]
fn a_child_that_dies_reports_its_stderr() -> Result<()> {
    let h = harness([Script::exited(
        "",
        "error: unexpected argument '--sandbox' found\n",
        2,
    )])?;
    let status = h.store.start(&request(codex()?, "review this"))?;

    let Outcome::Failed { detail, .. } = &status.outcome else {
        panic!("expected a failure, got {:?}", status.outcome);
    };
    assert_that!(detail.as_str(), contains_substring("unexpected argument"));
    Ok(())
}

/// A CLI that exits cleanly but says nothing is drift, and must not read as a silent success.
#[gtest]
fn a_clean_exit_with_no_completion_event_is_not_a_success() -> Result<()> {
    let h = harness([Script::completed("{\"type\":\"turn.started\"}\n")])?;
    let status = h.store.start(&request(codex()?, "review this"))?;
    assert_that!(
        status.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::Unclassified),
            detail: anything()
        })
    );
    Ok(())
}

/// Cancelling keeps everything collected so far.
#[gtest]
#[tokio::test]
async fn cancelling_keeps_what_was_collected() -> Result<()> {
    let partial = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"partial findings"}}
    "#};
    let h = harness([Script::running(partial)])?;
    let started = h.store.start(&request(codex()?, "long review"))?;
    assert_that!(started.outcome, matches_pattern!(Outcome::Running));

    let cancelled = h.store.cancel(&started.run_id).await?;
    assert_that!(cancelled.outcome, matches_pattern!(Outcome::Cancelled));
    assert_that!(h.launcher.terminated().len(), eq(1));

    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("partial findings"));
    assert_that!(page.text, contains_substring("cancelled"));
    Ok(())
}

/// `tail` pages the same rendered bytes `result` returns, and the cursor only ever moves forward.
#[gtest]
fn a_transcript_cursor_only_moves_forward() -> Result<()> {
    let h = harness([Script::completed(fixtures::CLAUDE_STOP_HOOK)])?;
    let started = h
        .store
        .start(&request(claude()?, "Reply with exactly: THE_REPORT_BODY"))?;

    let mut cursor = 0;
    let mut assembled = String::new();
    loop {
        let page = h.store.page(&started.run_id, cursor, 512)?;
        assembled.push_str(&page.text);
        assert_that!(page.next_offset >= cursor, eq(true));
        if page.at_end {
            break;
        }
        assert_that!(
            page.next_offset > cursor,
            eq(true),
            "a cursor that does not advance loops"
        );
        cursor = page.next_offset;
    }

    let whole = h.store.page(&started.run_id, 0, 10_000_000)?;
    assert_that!(assembled, eq(&whole.text));
    assert_that!(assembled, contains_substring("THE_REPORT_BODY"));
    Ok(())
}

/// A byte cursor must not split a multi-byte character.
#[gtest]
fn a_transcript_cursor_never_splits_a_character() -> Result<()> {
    let unicode = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"— ünïcödé — 日本語 — 🎉"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#};
    let h = harness([Script::completed(unicode)])?;
    let started = h.store.start(&request(codex()?, "q"))?;

    let mut cursor = 0;
    let mut assembled = String::new();
    loop {
        let page = h.store.page(&started.run_id, cursor, 7)?;
        assembled.push_str(&page.text);
        if page.at_end {
            break;
        }
        cursor = page.next_offset;
    }
    assert_that!(assembled, contains_substring("— ünïcödé — 日本語 — 🎉"));
    Ok(())
}

/// A consultation can be found again without having kept its id.
#[gtest]
fn recent_consultations_are_listed_newest_first() -> Result<()> {
    let h = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let first = h
        .store
        .start(&request(codex()?, "the first question\nwith a second line"))?;
    let second = h.store.start(&request(codex()?, "the second question"))?;

    let listed = h.store.list(10)?;
    assert_that!(listed.len(), eq(2));
    let ids: Vec<String> = listed.iter().map(|r| r.run_id.to_string()).collect();
    assert_that!(
        ids,
        elements_are![
            eq(&second.run_id.to_string()),
            eq(&first.run_id.to_string())
        ]
    );
    assert_that!(
        listed.last().map(|r| r.question.clone()),
        some(eq("the first question"))
    );
    Ok(())
}

/// Everything a caller needs to poll a long review is in `status`.
#[gtest]
fn status_surfaces_drift_and_hook_interference() -> Result<()> {
    let h = harness([Script::completed(fixtures::CLAUDE_STOP_HOOK)])?;
    let started = h.store.start(&request(claude()?, "q"))?;
    let status = h.store.status(&started.run_id)?;

    assert_that!(status.hook_reopening, some(eq(HookReopening::Unexpected)));
    assert_that!(status.unrecognised.is_empty(), eq(true));
    assert_that!(status.message_count, gt(2));
    assert_that!(status.transcript_bytes, gt(0));
    assert_that!(status.transcript_path.is_file(), eq(true));
    assert_that!(status.events_path.is_file(), eq(true));
    Ok(())
}

/// Waiting is capped and never destroys a run.
#[gtest]
#[tokio::test]
async fn waiting_past_the_cap_leaves_the_consultation_running() -> Result<()> {
    let h = harness([Script::running("{\"type\":\"turn.started\"}\n")])?;
    let started = h
        .store
        .start(&request(codex()?, "a ninety minute review"))?;

    h.store
        .wait_until_terminal(&started.run_id, Duration::from_millis(50))
        .await?;
    let waited = h.store.status(&started.run_id)?;
    assert_that!(waited.outcome, matches_pattern!(Outcome::Running));
    assert_that!(waited.run_id, eq(&started.run_id));
    Ok(())
}

/// A finished `ttl` run is swept; a kept one and a running one are not.
#[gtest]
fn the_sweep_keeps_what_must_be_kept() -> Result<()> {
    let h = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
        Script::running("{\"type\":\"turn.started\"}\n"),
    ])?;
    let expiring = h.store.start(&request(codex()?, "old"))?;
    let kept = h.store.start(&StartRequest {
        retention: Retention::UntilReleased,
        ..request(codex()?, "kept")
    })?;
    let running = h.store.start(&request(codex()?, "still going"))?;

    // Age all three past the TTL: the recorded creation time, and the capture files' own
    // timestamps, because the clock runs from the last thing the delegate wrote to any of them.
    let long_ago = std::time::SystemTime::now() - Duration::from_hours(48);
    for id in [&expiring.run_id, &kept.run_id, &running.run_id] {
        let run = h.store.root().join("runs").join(id.as_str());
        let path = run.join("meta.json");
        let mut meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        meta["created_at"] =
            serde_json::json!((chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339());
        std::fs::write(&path, serde_json::to_string(&meta)?)?;
        for capture in ["events.jsonl", "stderr.log"] {
            std::fs::File::options()
                .write(true)
                .open(run.join("turns/0000").join(capture))?
                .set_modified(long_ago)?;
        }
    }

    assert_that!(h.store.sweep()?, eq(1));
    assert_that!(
        h.store.status(&expiring.run_id).map(|_| ()),
        err(anything())
    );
    assert_that!(h.store.status(&kept.run_id).map(|_| ()), ok(anything()));
    assert_that!(h.store.status(&running.run_id).map(|_| ()), ok(anything()));
    Ok(())
}

/// A cancelled consultation must not offer a follow-up.
///
/// Its session id survives cancellation and resuming it would technically work — it would
/// continue a delegate that was killed part-way through a thought, which is never what a caller
/// means.
#[gtest]
#[tokio::test]
async fn a_cancelled_consultation_is_not_resumable() -> Result<()> {
    let partial = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"halfway through"}}
    "#};
    let h = harness([Script::running(partial)])?;
    let started = h.store.start(&request(codex()?, "long review"))?;

    let cancelled = h.store.cancel(&started.run_id).await?;
    assert_that!(cancelled.resumable, eq(false));
    assert_that!(
        h.store.follow_up(&started.run_id, "and now?").map(|_| ()),
        err(anything())
    );
    // Everything collected is still there.
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("halfway through"));
    Ok(())
}

/// Drift must reach `status`, not only the fold.
#[gtest]
fn status_reports_an_unrecognised_event() -> Result<()> {
    let drifted = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"a_shape_nobody_has_seen","payload":1}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"the answer"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#};
    let h = harness([Script::completed(drifted)])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    let status = h.store.status(&started.run_id)?;

    assert_that!(status.unrecognised.total(), eq(1));
    assert_that!(
        status.unrecognised.summary(),
        some(contains_substring("a_shape_nobody_has_seen"))
    );
    // And the transcript tells a reader the same thing, so it survives being read from the file.
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("did not recognise"));
    Ok(())
}

/// A report the event stream lost is recovered from the file the CLI writes itself.
#[gtest]
fn a_report_missing_from_the_stream_is_recovered_from_the_last_message_file() -> Result<()> {
    // A turn that completed but whose reply never appeared as an event, which is what a renamed
    // `agent_message` produces.
    // The rename is also counted as drift, and the two always arrive together: recovery only
    // fires when the stream carried no reply, and the only ways that happens — a renamed item
    // type or malformed lines — are both counted.
    // A fixture without the drift note would leave the ordering this test exists to check
    // untested.
    let h = harness([Script::completed(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"assistant_message","text":"the findings, in full"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#})])?;
    let started = h.store.start(&request(codex()?, "q"))?;

    // What a caller would already have seen, and paged past, before the file appeared.
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let before_recovery = page.text;

    // The codex process writes this file directly, so it survives what the stream does not.
    let last = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000/last-message.md");
    std::fs::write(&last, "the findings, in full")?;

    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("the findings, in full"));
    assert_that!(page.text, contains_substring("recovered"));
    // The drift note is part of what was already handed out, so it must still be above.
    assert_that!(page.text, contains_substring("did not recognise"));

    // Codex writes that file about 228 ms after its terminal event reaches stdout, so a caller
    // can already hold a cursor past everything whose content was final at that event — the
    // footer and the drift note both.
    // The recovered text must therefore append, never appear above bytes already handed out.
    assert_that!(
        page.text.starts_with(&before_recovery),
        eq(true),
        "recovering a report rewrote bytes a caller may already have paged past"
    );
    Ok(())
}

/// A child that flushes its terminal event as it exits must not be recorded as a failure.
///
/// agentmux reads the capture file and then observes the process, and those are two different
/// instants.
/// A child that writes its `turn.completed` in between would be settled on the older read, which
/// records `failed (launch failed)` and renders a failure footer.
/// The next call, reading the complete stream, would render `completed` with the report where
/// that footer had been.
/// Every byte offset a `tail` loop had already taken would then point into rewritten text.
#[gtest]
fn a_terminal_event_flushed_as_the_child_exits_is_not_settled_as_a_failure() -> Result<()> {
    let started_only = "{\"type\":\"thread.started\",\"thread_id\":\"01a0\"}\n";
    let flushed_at_exit = indoc::indoc! {r#"
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"the findings"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#};
    let h = harness([Script::flushes_as_it_exits(started_only, flushed_at_exit)])?;

    // The very first status is the one that races: the store folds a stream with no terminal
    // event, then asks about a child that has just finished writing one.
    let status = h.store.start(&request(codex()?, "q"))?;
    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));

    let page = h.store.page(&status.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("the findings"));
    assert_that!(page.text, not(contains_substring("launch failed")));

    // And the second read agrees with the first, byte for byte.
    let again = h.store.page(&status.run_id, 0, 1_000_000)?;
    assert_that!(again.text, eq(&page.text));
    Ok(())
}

/// The rendered transcript may only ever grow at its end, because `tail` hands out byte offsets.
#[gtest]
fn the_rendered_transcript_only_grows_at_its_end() -> Result<()> {
    let h = harness([Script::running(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"first finding"}}
    "#})])?;
    let started = h.store.start(&request(codex()?, "q"))?;

    let page = h.store.page(&started.run_id, 0, usize::MAX)?;
    let mut previous = page.text;
    let events = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000/events.jsonl");

    // Grow the stream the way a live child would, one event at a time.
    for extra in [
        r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"second finding"}}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#,
    ] {
        let mut stream = std::fs::read_to_string(&events)?;
        stream.push_str(extra);
        stream.push('\n');
        std::fs::write(&events, &stream)?;

        let page = h.store.page(&started.run_id, 0, usize::MAX)?;
        let now = page.text;
        assert_that!(
            now.starts_with(&previous),
            eq(true),
            "the render rewrote bytes a caller may already hold a cursor past"
        );
        previous = now;
    }
    assert_that!(previous, contains_substring("second finding"));
    Ok(())
}

/// A capture file that ends mid-character must not lose the messages before it.
///
/// A live child is appending, so the file routinely ends mid-line and sometimes mid-character.
/// Decoding the whole file as UTF-8 fails on that fragment, and treating the failure as "no
/// events" would shrink the rendered transcript and clamp an outstanding cursor backwards — the
/// messages would vanish and then reappear when the child finished the character.
#[gtest]
fn a_capture_file_ending_mid_character_keeps_everything_before_it() -> Result<()> {
    let h = harness([Script::running(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"the first finding"}}
    "#})])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let before = page.text;
    assert_that!(before, contains_substring("the first finding"));

    // Append a line that stops halfway through a four-byte character, as a partial write does.
    let events = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000/events.jsonl");
    let mut bytes = std::fs::read(&events)?;
    bytes.extend_from_slice(br#"{"type":"item.completed","item":{"text":"celebra"#);
    bytes.extend_from_slice(&"🎉".as_bytes()[..2]);
    std::fs::write(&events, &bytes)?;

    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let after = page.text;
    assert_that!(after, contains_substring("the first finding"));
    assert_that!(
        after.starts_with(&before),
        eq(true),
        "the transcript shrank"
    );
    Ok(())
}

/// A page size smaller than one character must still advance the cursor.
///
/// Every turn heading contains an em dash, so a caller that picks a small `max_bytes` would page
/// forever on a three-byte character: an empty slice, an unchanged cursor, and a loop that never
/// reaches the report.
#[gtest]
fn a_page_smaller_than_one_character_still_makes_progress() -> Result<()> {
    let h = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    let started = h.store.start(&request(codex()?, "q"))?;

    let mut cursor = 0;
    let mut steps = 0;
    loop {
        let page = h.store.page(&started.run_id, cursor, 1)?;
        assert_that!(
            page.next_offset > cursor,
            eq(true),
            "the cursor stalled at byte {cursor}"
        );
        cursor = page.next_offset;
        steps += 1;
        if page.at_end || steps > 20_000 {
            break;
        }
    }
    assert_that!(page_reached_end(&h, &started.run_id, cursor)?, eq(true));
    Ok(())
}

fn page_reached_end(h: &Harness, run_id: &RunId, cursor: u64) -> Result<bool> {
    let page = h.store.page(run_id, cursor, 1)?;
    Ok(page.at_end)
}

/// Each turn owns its own capture files, and a claimed turn is never launched into twice.
///
/// Turn directories are how `fold_run` counts turns, so a caller that creates one is already
/// visible to the next fold.
/// What remains is the window where two callers fold before either creates anything: both compute
/// the same index, and a forgiving directory create would let both spawn a paid child into one
/// capture file — mixed replies under one question, and everything after the first terminal event
/// lost from the fold.
/// `launch_turn` uses `create_dir`, so the create is the claim and the loser gets an error
/// instead of a second bill.
///
/// That interleaving cannot be forced from outside the crate without a seam that exists only for
/// the test, so what is asserted here is the invariant it protects: separate turns, separate
/// files, never shared.
#[gtest]
fn every_turn_owns_its_own_capture_files() -> Result<()> {
    let h = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let started = h.store.start(&request(codex()?, "the original brief"))?;
    h.store.follow_up(&started.run_id, "first follow-up")?;
    let third = h.store.follow_up(&started.run_id, "second follow-up")?;
    assert_that!(third.turns, eq(3));

    let turns = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns");
    let questions: Vec<String> = (0..3)
        .map(|index| std::fs::read_to_string(turns.join(format!("{index:04}/question.md"))))
        .collect::<std::result::Result<_, _>>()?;
    assert_that!(
        questions,
        elements_are![
            eq("the original brief"),
            eq("first follow-up"),
            eq("second follow-up")
        ]
    );

    // Three children, three capture files, no sharing.
    let events: std::collections::BTreeSet<_> = h
        .launcher
        .launches()
        .into_iter()
        .map(|launch| launch.events)
        .collect();
    assert_that!(events.len(), eq(3));
    Ok(())
}

/// A rate limit on a follow-up must not lock a caller out of a conversation that still exists.
///
/// Failure category says what went wrong, not when.
/// Turn one succeeded and its session is intact, so deciding resumability from the newest turn's
/// category alone would refuse every further follow-up permanently.
#[gtest]
fn a_rate_limited_follow_up_leaves_the_consultation_resumable() -> Result<()> {
    let rate_limited = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
        {"type":"turn.failed","error":{"message":"rate limit reached for this account"}}
    "#};
    let h = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(rate_limited),
    ])?;
    let started = h.store.start(&request(codex()?, "the original brief"))?;

    let followed = h.store.follow_up(&started.run_id, "and now?")?;
    assert_that!(
        followed.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::RateLimited),
            detail: anything()
        })
    );
    // Turn one's answer is still there and its session is still open, so trying again is allowed.
    assert_that!(followed.resumable, eq(true));
    // And the earlier success is not hidden by the newer failure, nor the reverse.
    assert_that!(followed.turns, eq(2));
    Ok(())
}

/// A child that finished in the moment before the signal landed is reported as having finished.
///
/// Cancellation records the child's departure, not the caller's wish, so whatever the child wrote
/// before it went is part of the consultation — and a delegate that had already delivered its
/// answer is not made to look interrupted.
#[gtest]
#[tokio::test]
async fn a_child_that_finishes_as_it_is_stopped_is_reported_finished() -> Result<()> {
    let h = harness([Script::flushes_when_stopped(
        indoc::indoc! {r#"
            {"type":"thread.started","thread_id":"01a0"}
            {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"partial"}}
        "#},
        indoc::indoc! {r#"
            {"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"the rest"}}
            {"type":"turn.completed","usage":{"input_tokens":1}}
        "#},
    )])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    assert_that!(started.outcome, matches_pattern!(Outcome::Running));

    let cancelled = h.store.cancel(&started.run_id).await?;
    assert_that!(
        cancelled.outcome,
        matches_pattern!(Outcome::Completed { .. })
    );
    assert_that!(cancelled.resumable, eq(true));
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("the rest"));
    assert_that!(page.text, not(contains_substring("cancelled")));
    Ok(())
}

/// Nothing written after a cancellation was recorded is part of the consultation.
///
/// The cancelled footer is the last thing the transcript says, and a caller may already hold a
/// cursor past it.
/// A child that somehow outlives the kill and keeps writing must not be able to move that footer,
/// so the fold reads only as much of the capture as existed when the cancellation was recorded.
#[gtest]
#[tokio::test]
async fn a_cancelled_transcript_is_frozen_at_the_cancellation() -> Result<()> {
    let h = harness([Script::running(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"partial"}}
    "#})])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    let cancelled = h.store.cancel(&started.run_id).await?;
    assert_that!(cancelled.outcome, matches_pattern!(Outcome::Cancelled));
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let published = page.text;

    // Something survived the kill and kept writing.
    let events = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000/events.jsonl");
    let mut stream = std::fs::read_to_string(&events)?;
    stream.push_str(indoc::indoc! {r#"
        {"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"from beyond"}}
        {"type":"turn.completed","usage":{"input_tokens":1}}
    "#});
    std::fs::write(&events, stream)?;

    let after = h.store.status(&started.run_id)?;
    assert_that!(after.outcome, matches_pattern!(Outcome::Cancelled));
    assert_that!(after.resumable, eq(false));
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let again = page.text;
    assert_that!(again, eq(&published));
    assert_that!(again, not(contains_substring("from beyond")));
    Ok(())
}

/// A start that is refused leaves nothing behind for `list` to report.
///
/// The run directory is made before the delegate is launched, and a launch can be refused for a
/// mistyped alias.
/// Left in place, that directory would fold to a consultation with no turns — which is "running"
/// as far as the sweep is concerned — and `list` would show a phantom for ever.
#[gtest]
fn a_refused_start_leaves_no_phantom_consultation() -> Result<()> {
    let h = harness([Script::completed(fixtures::CLAUDE_HAPPY)])?;
    let refused = h.store.start(&request(
        Delegate::Claude {
            model: ModelId::parse("claude-opus-5").or_fail()?,
            effort: Some(Effort::parse("xhigh").or_fail()?),
            account: Some(AccountAlias::parse("nobody").or_fail()?),
            isolation: None,
        },
        "q",
    ));
    assert_that!(refused.map(|_| ()), err(anything()));
    assert_that!(h.launcher.launches().len(), eq(0));
    assert_that!(h.store.list(10)?, is_empty());
    assert_that!(h.store.sweep()?, eq(0));
    Ok(())
}

/// A follow-up that is refused after claiming its turn gives the claim back.
///
/// Otherwise the transcript gains a phantom turn that folds to a launch failure, the consultation
/// reads as failed, and the next real turn is told an earlier one failed.
#[gtest]
fn a_refused_follow_up_leaves_no_phantom_turn() -> Result<()> {
    let h = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    let started = h.store.start(&request(codex()?, "q"))?;

    // The environment the follow-up will re-apply now names something no request may set.
    let meta_path = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("meta.json");
    let mut meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&meta_path)?)?;
    meta["env"] = serde_json::json!({"PATH": "/tmp/attacker"});
    std::fs::write(&meta_path, serde_json::to_string(&meta)?)?;

    let refused = h.store.follow_up(&started.run_id, "again");
    assert_that!(refused.map(|_| ()), err(anything()));
    let status = h.store.status(&started.run_id)?;
    assert_that!(status.turns, eq(1));
    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(status.earlier_failure, none());
    Ok(())
}

/// A turn that ended without the delegate saying anything is not continued.
///
/// There is nothing to continue, and there is a second reason: a reply Codex writes to its own
/// file after the terminal event is appended to that turn when it lands, and a turn allowed to
/// follow it would then have bytes inserted above it.
#[gtest]
fn a_turn_that_said_nothing_is_not_resumable_until_it_has() -> Result<()> {
    let h = harness([
        Script::completed(indoc::indoc! {r#"
            {"type":"thread.started","thread_id":"01a0"}
            {"type":"item.completed","item":{"id":"i0","type":"assistant_message","text":"renamed away"}}
            {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
        "#}),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    assert_that!(started.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(started.resumable, eq(false));
    let refused = h.store.follow_up(&started.run_id, "again");
    assert_that!(
        refused.map(|_| ()),
        err(matches_pattern!(RunError::NotResumable(matches_pattern!(
            NotResumable::NothingSaid { .. }
        ))))
    );

    // Once the CLI's own file lands, the turn has said something and may be continued.
    let last = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000/last-message.md");
    std::fs::write(&last, "the findings, in full")?;
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let before = page.text;
    let followed = h.store.follow_up(&started.run_id, "again")?;
    assert_that!(followed.turns, eq(2));
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let after = page.text;
    assert_that!(after.starts_with(&before), eq(true));
    Ok(())
}

/// Deleting a running consultation would orphan its child, so it is refused.
#[gtest]
fn a_running_consultation_cannot_be_removed() -> Result<()> {
    let h = harness([Script::running("{\"type\":\"turn.started\"}\n")])?;
    let started = h.store.start(&request(codex()?, "long"))?;
    assert_that!(
        h.store.remove(&started.run_id),
        err(matches_pattern!(RunError::StillRunning { .. }))
    );
    assert_that!(h.store.status(&started.run_id).map(|_| ()), ok(anything()));
    Ok(())
}

/// A finished child must be reaped before its liveness is trusted.
///
/// A child agentmux still parents stays a zombie until it is waited on, and a zombie keeps its
/// process group.
/// Asking about liveness before reaping would report it alive for as long as agentmux lives, so a
/// consultation whose delegate died without emitting a terminal event would poll for ever instead
/// of settling.
#[gtest]
fn a_child_that_died_without_a_terminal_event_settles_rather_than_polling_forever() -> Result<()> {
    let h = harness([Script::exited(
        "{\"type\":\"thread.started\",\"thread_id\":\"01a0\"}\n",
        "error: unexpected argument '--sandbox' found\n",
        2,
    )])?;
    let status = h.store.start(&request(codex()?, "q"))?;

    assert_that!(status.outcome.is_terminal(), eq(true));
    let Outcome::Failed { detail, .. } = &status.outcome else {
        panic!("expected a failure, got {:?}", status.outcome);
    };
    assert_that!(detail.as_str(), contains_substring("unexpected argument"));
    Ok(())
}

/// An older render must never replace a newer one on the advertised transcript path.
///
/// Two callers can fold at different instants and finish in the other order.
/// Renaming is atomic but not ordered, so the slower caller's older snapshot would overwrite the
/// newer one and the advertised path would lose already-published content until some later read
/// repaired it.
/// The render is append-only, which is what makes "is this snapshot a prefix of what is already
/// there?" a sound test for staleness.
#[gtest]
fn a_stale_render_does_not_overwrite_a_newer_transcript() -> Result<()> {
    let h = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    let started = h.store.start(&request(codex()?, "q"))?;

    let path = h.store.status(&started.run_id)?.transcript_path;
    let complete = std::fs::read_to_string(&path)?;
    assert_that!(complete.as_str(), contains_substring("FIXTURE_OK"));

    // Stand in for a newer render published by another caller: the same bytes plus more, which is
    // the only way the render can grow.
    let newer = format!("{complete}\n## Turn 2 — question\n\na later turn\n");
    std::fs::write(&path, &newer)?;

    // A read that folds the older state must leave the newer publication alone.
    h.store.view(&started.run_id, 0, 1_000_000)?;
    assert_that!(std::fs::read_to_string(&path)?, eq(&newer));

    // A file that is not a prefix of the render is not a newer snapshot, so it is repaired.
    std::fs::write(&path, "corrupted")?;
    h.store.view(&started.run_id, 0, 1_000_000)?;
    assert_that!(std::fs::read_to_string(&path)?, eq(&complete));
    Ok(())
}

/// agentmux refuses to consult a delegate when it is itself running as one.
///
/// Only reachable once a delegate inherits its account's MCP servers and finds agentmux among
/// them, which is precisely the case isolation would otherwise have prevented.
/// Refusing here is what lets the opt-out exist without a consultation being able to spawn
/// consultations until something runs out.
#[gtest]
fn agentmux_running_as_a_delegate_refuses_to_start_another_consultation() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let env: BTreeMap<String, String> = [
        ("PATH".to_owned(), "/usr/bin".to_owned()),
        ("HOME".to_owned(), "/home/dev".to_owned()),
        ("AGENTMUX_DELEGATE".to_owned(), "1".to_owned()),
    ]
    .into_iter()
    .collect();
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([Script::completed(
            fixtures::CODEX_HAPPY,
        )])),
        env,
    )
    .or_fail()?;

    let error = store
        .start(&request(codex()?, "q"))
        .expect_err("a delegate may not start a consultation");

    assert_that!(
        error.to_string(),
        contains_substring("running inside a delegate")
    );
    Ok(())
}

/// An account that asks to inherit is recorded as having done so, and stays recorded.
///
/// Nothing else pins this down, and the consequence of losing it is not a missing label.
/// A hook in the account's own settings reopens the finished turn, which is exactly what the
/// operator configured, and a run whose isolation was not recorded reports that as "should not be
/// possible: treat this transcript as untrusted", on every consultation, forever.
///
/// The second half is the follow-up: a configuration edited mid-consultation must not move turn two
/// to a different isolation than turn one, in one transcript.
#[gtest]
fn an_inheriting_account_is_recorded_and_stays_recorded() -> Result<()> {
    let home = tempfile::tempdir().or_fail()?;
    let config_path = home.path().join("agentmux.toml");
    let account_dir = home.path().join(".claude-personal");
    std::fs::create_dir_all(&account_dir).or_fail()?;
    let write_config = |inherit: bool| -> Result<()> {
        std::fs::write(
            &config_path,
            indoc::formatdoc! {r#"
                [defaults.claude]
                account = "personal"

                [accounts.claude.personal]
                config_dir = {:?}
                inherit_settings = {inherit}
            "#, account_dir.display().to_string()},
        )
        .or_fail()?;
        Ok(())
    };
    write_config(true)?;

    let dir = tempfile::tempdir().or_fail()?;
    let env: BTreeMap<String, String> = [
        ("PATH".to_owned(), "/usr/bin".to_owned()),
        (
            "HOME".to_owned(),
            home.path().to_string_lossy().into_owned(),
        ),
        (
            "AGENTMUX_CONFIG".to_owned(),
            config_path.to_string_lossy().into_owned(),
        ),
    ]
    .into_iter()
    .collect();
    let store = RunStore::open(
        dir.path(),
        Arc::new(ScriptedLauncher::new([
            Script::completed(fixtures::CLAUDE_HAPPY),
            Script::completed(fixtures::CLAUDE_RESUME),
        ])),
        env,
    )
    .or_fail()?;

    let started = store
        .start(&StartRequest {
            delegate: Delegate::Claude {
                model: ModelId::parse("claude-opus-5").or_fail()?,
                effort: Some(Effort::parse("xhigh").or_fail()?),
                // The caller names neither the account nor the isolation.
                account: None,
                isolation: None,
            },
            question: "q".to_owned(),
            cwd: home.path().to_path_buf(),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
        })
        .or_fail()?;

    // The account's preference was resolved and written down, not left for each launch to redo.
    assert_that!(
        started.delegate.isolation(),
        some(eq(Isolation::Inherit)),
        "an account-driven inherit was not recorded"
    );
    assert_that!(
        started.delegate.summary(),
        contains_substring("settings=inherited")
    );

    // Changing the machine's mind must not change a consultation already under way.
    write_config(false)?;
    let resumed = store.follow_up(&started.run_id, "again").or_fail()?;

    assert_that!(
        resumed.delegate.isolation(),
        some(eq(Isolation::Inherit)),
        "a follow-up re-resolved isolation against an edited configuration"
    );
    Ok(())
}

/// A turn directory claimed by a caller that died before recording its child is taken over.
///
/// The claim is the directory; the launch record is written before the lock is released.
/// A directory with neither record can only be an abandoned claim, and left alone it would refuse
/// every later follow-up as "already in flight" for good.
#[gtest]
fn an_abandoned_turn_claim_is_taken_over() -> Result<()> {
    let h = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    let abandoned = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0001");
    std::fs::create_dir_all(&abandoned)?;
    std::fs::write(abandoned.join("question.md"), "never launched")?;

    // Not a turn: the consultation still reads as one finished turn, ready to continue.
    let status = h.store.status(&started.run_id)?;
    assert_that!(status.turns, eq(1));
    assert_that!(status.resumable, eq(true));

    let followed = h.store.follow_up(&started.run_id, "again")?;
    assert_that!(followed.turns, eq(2));
    assert_that!(abandoned.join("launch.json").is_file(), eq(true));
    assert_that!(
        std::fs::read_to_string(abandoned.join("question.md"))?,
        eq("again")
    );
    // The claim's own files are set aside, not destroyed: a child spawned by the caller that
    // died may still be writing to them.
    let set_aside = std::fs::read_dir(abandoned.parent().or_fail()?)?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("0001.abandoned."))
        .or_fail()?;
    assert_that!(
        std::fs::read_to_string(
            abandoned
                .parent()
                .or_fail()?
                .join(set_aside)
                .join("question.md")
        )?,
        eq("never launched")
    );
    Ok(())
}

/// Cancelling a consultation that never recorded a turn returns, rather than waiting on itself.
///
/// The run lock is per file and a second lock from the same process waits for the first, so
/// nothing taken under it may call back into anything that takes it.
#[gtest]
#[tokio::test]
async fn cancelling_a_consultation_with_no_recorded_turn_returns() -> Result<()> {
    let h = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    let turn = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000");
    std::fs::remove_dir_all(&turn)?;

    let cancelled = tokio::time::timeout(Duration::from_secs(5), h.store.cancel(&started.run_id))
        .await
        .or_fail()??;
    assert_that!(cancelled.turns, eq(0));
    Ok(())
}

/// Where agentmux keeps its runs reaches the delegate, as the store resolved it.
///
/// The recursion guard's second witness is a launch record in that store; an agentmux started
/// underneath the delegate that opened the platform default instead would find nothing and serve.
#[gtest]
fn the_delegate_is_told_where_this_store_keeps_its_runs() -> Result<()> {
    let h = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    h.store.start(&request(codex()?, "q"))?;
    let launch = h.launcher.launches().into_iter().next().or_fail()?;
    assert_that!(
        launch.env.get(RunStore::STATE_DIR_ENV).map(PathBuf::from),
        some(eq(&h.store.root().to_path_buf()))
    );
    Ok(())
}

/// An account a checkout selects does not bring its own `request_env`, even once the
/// consultation has pinned that account by name.
///
/// A consultation records the alias it resolved to, so every launch after the first names it
/// as if the caller had; the checkout's choice has to be recognised by the file, not by who
/// happened to spell the alias out.
#[gtest]
fn a_checkout_selected_account_does_not_widen_request_env_through_the_store() -> Result<()> {
    let home = tempfile::tempdir().or_fail()?;
    let repo = home.path().join("work/client");
    std::fs::create_dir_all(&repo)?;
    std::fs::write(
        home.path().join("agentmux.toml"),
        indoc::indoc! {r#"
            [accounts.claude.debug]
            api_key_env = "DEBUG_KEY"
            request_env = ["NODE_OPTIONS"]
        "#},
    )?;
    let mut host = env();
    host.insert(
        "HOME".to_owned(),
        home.path().to_string_lossy().into_owned(),
    );
    host.insert("DEBUG_KEY".to_owned(), "sk-debug".to_owned());
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([
        Script::completed(fixtures::CLAUDE_HAPPY),
        Script::completed(fixtures::CLAUDE_HAPPY),
    ]));
    let store = RunStore::open(dir.path(), launcher, host).or_fail()?;
    let with_env = |account: Option<&str>| -> Result<StartRequest> {
        Ok(StartRequest {
            delegate: Delegate::Claude {
                model: ModelId::parse("claude-opus-5").or_fail()?,
                effort: Some(Effort::parse("xhigh").or_fail()?),
                account: account.map(AccountAlias::parse).transpose().or_fail()?,
                isolation: None,
            },
            question: "q".to_owned(),
            cwd: repo.clone(),
            retention: Retention::Ttl,
            env: [("NODE_OPTIONS".to_owned(), "--require /tmp/x.js".to_owned())]
                .into_iter()
                .collect(),
        })
    };

    // Named by the caller with no project file in play: the account's own list applies.
    assert_that!(
        store.start(&with_env(Some("debug"))?).map(|_| ()),
        ok(anything())
    );

    // Selected by the checkout: refused, and refused just the same when named outright.
    std::fs::write(
        repo.join("agentmux.toml"),
        "[defaults.claude]\naccount = \"debug\"\n",
    )?;
    assert_that!(
        store.start(&with_env(None)?).map(|_| ()),
        err(displays_as(contains_substring("cannot be set per request")))
    );
    assert_that!(
        store.start(&with_env(Some("debug"))?).map(|_| ()),
        err(displays_as(contains_substring("cannot be set per request")))
    );
    Ok(())
}

/// What a settled failure says is frozen with the record, not re-read from a file something may
/// still be writing to.
///
/// A tool subprocess the delegate started inherits its stderr and can outlive it; a footer that
/// re-read the file would move under a reader who had already been handed it.
#[gtest]
fn a_settled_failure_does_not_move_with_its_stderr() -> Result<()> {
    let h = harness([Script::exited(
        "{\"type\":\"thread.started\",\"thread_id\":\"01a0\"}\n",
        "boom: the first word\n",
        2,
    )])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    let Outcome::Failed { detail, .. } = &started.outcome else {
        panic!("expected a failure, got {:?}", started.outcome);
    };
    assert_that!(detail.as_str(), contains_substring("the first word"));
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let published = page.text;

    let stderr = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000/stderr.log");
    let mut text = std::fs::read_to_string(&stderr)?;
    text.push_str("and a second word, from beyond\n");
    std::fs::write(&stderr, text)?;

    let (again, page) = h.store.view(&started.run_id, 0, 1_000_000)?;
    assert_that!(again.outcome, eq(&started.outcome));
    assert_that!(page.text, eq(&published));
    Ok(())
}

/// A reply the CLI writes out for an earlier turn after a later turn exists is left where it is.
///
/// A follow-up is allowed once the consultation has said something, so a turn that finished
/// silently can be followed; its late reply must then not be inserted above the turn that
/// followed, whose bytes a reader may already hold.
#[gtest]
fn a_reply_landing_after_a_later_turn_is_not_recovered_into_the_earlier_one() -> Result<()> {
    let silent = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"assistant_message","text":"renamed away"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#};
    let h = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(silent),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    h.store.follow_up(&started.run_id, "again")?;
    let followed = h.store.follow_up(&started.run_id, "and again")?;
    assert_that!(followed.turns, eq(3));
    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let before = page.text;

    let earlier = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0001");
    std::fs::write(earlier.join("last-message.md"), "late findings")?;

    let page = h.store.page(&started.run_id, 0, 1_000_000)?;
    let after = page.text;
    assert_that!(after, eq(&before));
    assert_that!(earlier.join("recovered.md").exists(), eq(false));
    Ok(())
}

/// A consultation whose child is still there after its stream finished is not deleted.
///
/// Both CLIs outlive their closing event by a little, and one told to stop may outlive that by
/// more; deleting the run would leave the child writing into unlinked files and unreachable.
#[gtest]
#[tokio::test]
async fn a_consultation_whose_child_still_lives_is_not_removed() -> Result<()> {
    let h = harness([Script::running(fixtures::CODEX_HAPPY)])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    assert_that!(started.outcome, matches_pattern!(Outcome::Completed { .. }));

    assert_that!(
        h.store.remove(&started.run_id),
        err(matches_pattern!(RunError::StillRunning { .. }))
    );
    // Once the child has been stopped, the run is an ordinary finished one.
    h.store.cancel(&started.run_id).await?;
    assert_that!(h.store.remove(&started.run_id), ok(anything()));
    Ok(())
}

/// The machine's rewrite decides which model runs, and the record names both.
///
/// The name a caller types is often one a person chose weeks ago in a prompt — "fable 5", the
/// series rather than the point release — and the machine is where the two are reconciled.
/// What the delegate is launched with, what the transcript header says and what `status` reports
/// have to agree, because a caller that pinned a model and reads another back has no other way to
/// tell a rule of its own machine from a pin agentmux dropped.
#[gtest]
fn a_configured_rewrite_decides_which_model_runs_and_is_recorded() -> Result<()> {
    let home = tempfile::tempdir().or_fail()?;
    let config = home.path().join("agentmux.toml");
    std::fs::write(
        &config,
        indoc::indoc! {r#"
            [models.codex]
            "gpt-6" = "gpt-6-astra"
        "#},
    )?;
    let mut host = env();
    host.insert(
        "HOME".to_owned(),
        home.path().to_string_lossy().into_owned(),
    );
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ]));
    let store = RunStore::open(dir.path(), launcher.clone(), host).or_fail()?;
    let asked = Delegate::Codex {
        model: ModelId::parse("gpt-6").or_fail()?,
        effort: Some(Effort::parse("high").or_fail()?),
        sandbox: CodexSandbox::ReadOnly,
        account: None,
        isolation: None,
    };

    let started = store.start(&StartRequest {
        delegate: asked,
        question: "q".to_owned(),
        cwd: home.path().to_path_buf(),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
    })?;

    let launch = launcher.launches().into_iter().next().or_fail()?;
    assert_that!(launch.has_flag_with("--model", "gpt-6-astra"), eq(true));
    assert_that!(started.delegate.model().as_str(), eq("gpt-6-astra"));
    assert_that!(
        started.rewritten_from.as_ref().map(ModelId::as_str),
        some(eq("gpt-6"))
    );
    let page = store.page(&started.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("codex gpt-6-astra"));
    assert_that!(page.text, contains_substring("(asked for gpt-6)"));

    // The consultation keeps the model it started with: a file edited between two questions must
    // not answer the second half of one transcript with a different model.
    std::fs::write(
        &config,
        indoc::indoc! {r#"
            [models.codex]
            "gpt-6" = "gpt-5.6-sol"
        "#},
    )?;
    store.follow_up(&started.run_id, "and now?")?;
    let launches = launcher.launches();
    let second = launches.get(1).or_fail()?;
    assert_that!(second.has_flag_with("--model", "gpt-6-astra"), eq(true));
    Ok(())
}

/// The machine's default effort fills in only what the caller left out, keyed by the model that
/// runs.
///
/// Three consultations against one file: a caller naming no effort gets the file's default for
/// the model its request was rewritten to; a caller naming one keeps it; and a model the file says
/// nothing about is launched with no effort at all, so the CLI's own default runs rather than a
/// value agentmux made up.
/// In every case the record names what the CLI was actually given.
#[gtest]
fn a_default_effort_fills_in_only_what_the_caller_left_out() -> Result<()> {
    let home = tempfile::tempdir().or_fail()?;
    std::fs::write(
        home.path().join("agentmux.toml"),
        indoc::indoc! {r#"
            [models.codex]
            "gpt-6" = "gpt-6-astra"

            [efforts.codex]
            "gpt-6-astra" = "medium"
        "#},
    )?;
    let mut host = env();
    host.insert(
        "HOME".to_owned(),
        home.path().to_string_lossy().into_owned(),
    );
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ]));
    let store = RunStore::open(dir.path(), launcher.clone(), host).or_fail()?;
    let codex = |model: &str, effort: Option<&str>| -> Result<Delegate> {
        Ok(Delegate::Codex {
            model: ModelId::parse(model).or_fail()?,
            effort: effort.map(Effort::parse).transpose().or_fail()?,
            sandbox: CodexSandbox::ReadOnly,
            account: None,
            isolation: None,
        })
    };
    let start = |delegate: Delegate| {
        store.start(&StartRequest {
            delegate,
            question: "q".to_owned(),
            cwd: home.path().to_path_buf(),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
        })
    };
    let reasoning = |effort: &str| format!("model_reasoning_effort=\"{effort}\"");

    // Asked by the name the file rewrites, with no effort: the default for the model that runs.
    let defaulted = start(codex("gpt-6", None)?)?;
    // The caller's own effort is never overridden.
    let pinned = start(codex("gpt-6", Some("high"))?)?;
    // A model the file names no effort for: nothing is passed, and the CLI decides.
    let unnamed = start(codex("gpt-5.6-sol", None)?)?;

    let launches = launcher.launches();
    let [first, second, third] = launches.as_slice() else {
        return fail!("expected three launches, saw {}", launches.len());
    };
    assert_that!(
        first.has_flag_with("--config", &reasoning("medium")),
        eq(true)
    );
    assert_that!(
        defaulted.delegate.effort().map(Effort::as_str),
        some(eq("medium"))
    );
    assert_that!(
        second.has_flag_with("--config", &reasoning("high")),
        eq(true)
    );
    assert_that!(
        pinned.delegate.effort().map(Effort::as_str),
        some(eq("high"))
    );
    assert_that!(
        third
            .args
            .iter()
            .any(|arg| arg.starts_with("model_reasoning_effort=")),
        eq(false),
        "an effort nobody named reached the argv: {:?}",
        third.args
    );
    assert_that!(unnamed.delegate.effort(), none());
    assert_that!(
        unnamed.delegate.summary(),
        contains_substring("effort=cli-default")
    );
    Ok(())
}
