//! A consultation end to end, through the scripted launcher: no credentials, no network.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentmux::delegate::{CodexSandbox, Delegate, Effort, ModelId};
use agentmux::run::{Retention, RunId, RunStore, StartRequest};
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
        effort: Effort::parse("xhigh").or_fail()?,
        account: None,
    })
}

fn codex() -> Result<Delegate> {
    Ok(Delegate::Codex {
        model: ModelId::parse("gpt-6-astra").or_fail()?,
        effort: Effort::parse("high").or_fail()?,
        sandbox: CodexSandbox::ReadOnly,
        account: None,
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
    let page = h.store.read_transcript(&started.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("FIXTURE_OK"));
    assert_that!(page.text, contains_substring("BANANAPHONE"));
    assert_that!(page.text, contains_substring("Turn 1 — question"));
    assert_that!(page.text, contains_substring("Turn 2 — question"));
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
fn cancelling_keeps_what_was_collected() -> Result<()> {
    let partial = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"partial findings"}}
    "#};
    let h = harness([Script::running(partial)])?;
    let started = h.store.start(&request(codex()?, "long review"))?;
    assert_that!(started.outcome, matches_pattern!(Outcome::Running));

    let cancelled = h.store.cancel(&started.run_id)?;
    assert_that!(cancelled.outcome, matches_pattern!(Outcome::Cancelled));
    assert_that!(h.launcher.terminated().len(), eq(1));

    let page = h.store.read_transcript(&started.run_id, 0, 1_000_000)?;
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
        let page = h.store.read_transcript(&started.run_id, cursor, 512)?;
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

    let whole = h.store.read_transcript(&started.run_id, 0, 10_000_000)?;
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
        let page = h.store.read_transcript(&started.run_id, cursor, 7)?;
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

    assert_that!(status.reopened_by_hook, eq(true));
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

    let waited = h
        .store
        .wait_until_terminal(&started.run_id, Duration::from_millis(50))
        .await?;
    assert_that!(waited.outcome, matches_pattern!(Outcome::Running));
    assert_that!(h.store.status(&started.run_id)?.run_id, eq(&started.run_id));
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

    // Age all three past the TTL by rewriting their recorded creation time on disk.
    // The sweep reads the clock, so there is nothing else to move.
    for id in [&expiring.run_id, &kept.run_id, &running.run_id] {
        let path = h
            .store
            .root()
            .join("runs")
            .join(id.as_str())
            .join("meta.json");
        let mut meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        meta["created_at"] =
            serde_json::json!((chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339());
        std::fs::write(&path, serde_json::to_string(&meta)?)?;
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
fn a_cancelled_consultation_is_not_resumable() -> Result<()> {
    let partial = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"halfway through"}}
    "#};
    let h = harness([Script::running(partial)])?;
    let started = h.store.start(&request(codex()?, "long review"))?;

    let cancelled = h.store.cancel(&started.run_id)?;
    assert_that!(cancelled.resumable, eq(false));
    assert_that!(
        h.store.follow_up(&started.run_id, "and now?").map(|_| ()),
        err(anything())
    );
    // Everything collected is still there.
    let page = h.store.read_transcript(&started.run_id, 0, 1_000_000)?;
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
    let page = h.store.read_transcript(&started.run_id, 0, 1_000_000)?;
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
    let before_recovery = h.store.read_transcript(&started.run_id, 0, 1_000_000)?.text;

    // The codex process writes this file directly, so it survives what the stream does not.
    let last = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000/last-message.md");
    std::fs::write(&last, "the findings, in full")?;

    let page = h.store.read_transcript(&started.run_id, 0, 1_000_000)?;
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

    let page = h.store.read_transcript(&status.run_id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("the findings"));
    assert_that!(page.text, not(contains_substring("launch failed")));

    // And the second read agrees with the first, byte for byte.
    let again = h.store.read_transcript(&status.run_id, 0, 1_000_000)?;
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

    let mut previous = h
        .store
        .read_transcript(&started.run_id, 0, usize::MAX)?
        .text;
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

        let now = h
            .store
            .read_transcript(&started.run_id, 0, usize::MAX)?
            .text;
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
    let before = h.store.read_transcript(&started.run_id, 0, 1_000_000)?.text;
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

    let after = h.store.read_transcript(&started.run_id, 0, 1_000_000)?.text;
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
        let page = h.store.read_transcript(&started.run_id, cursor, 1)?;
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
    Ok(h.store.read_transcript(run_id, cursor, 1)?.at_end)
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

/// A cancellation outranks a terminal event the child managed to emit afterwards.
#[gtest]
fn a_child_that_outruns_cancellation_stays_cancelled() -> Result<()> {
    let h = harness([Script::running(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"partial"}}
    "#})])?;
    let started = h.store.start(&request(codex()?, "q"))?;
    let cancelled = h.store.cancel(&started.run_id)?;
    assert_that!(cancelled.outcome, matches_pattern!(Outcome::Cancelled));

    // The child ignored SIGTERM and finished anyway.
    let events = h
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000/events.jsonl");
    let mut stream = std::fs::read_to_string(&events)?;
    stream.push_str("{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1}}\n");
    std::fs::write(&events, stream)?;

    let after = h.store.status(&started.run_id)?;
    assert_that!(after.outcome, matches_pattern!(Outcome::Cancelled));
    assert_that!(after.resumable, eq(false));
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
    h.store.read_transcript(&started.run_id, 0, 1_000_000)?;
    assert_that!(std::fs::read_to_string(&path)?, eq(&newer));

    // A file that is not a prefix of the render is not a newer snapshot, so it is repaired.
    std::fs::write(&path, "corrupted")?;
    h.store.read_transcript(&started.run_id, 0, 1_000_000)?;
    assert_that!(std::fs::read_to_string(&path)?, eq(&complete));
    Ok(())
}
