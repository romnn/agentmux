//! When a turn settles, what its `settled.json` commits, and waiting for it, through the scripted
//! launcher: no credentials, no network, no processes.
//!
//! The real supervisor is exercised against real processes in `agentmux-cli`'s tests; here the
//! store settles turns on read, as one without a supervisor does, which is also how a turn whose
//! supervisor died is repaired.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentmux::delegate::{
    AccountChoice, CodexPermissions, CodexSandbox, Delegate, Effort, ModelId,
};
use agentmux::run::{Retention, RunError, RunId, RunStore, SettledBy, StartRequest, WaitFor};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use agentmux::transcript::{Outcome, TurnState};
use googletest::prelude::*;

fn env() -> BTreeMap<String, String> {
    [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn codex() -> Result<Delegate> {
    Ok(Delegate::Codex {
        model: ModelId::parse("gpt-6-astra").or_fail()?,
        effort: Some(Effort::parse("high").or_fail()?),
        permissions: CodexPermissions {
            sandbox: Some(CodexSandbox::ReadOnly),
            network_access: None,
        },
        account: AccountChoice::Default,
        isolation: None,
    })
}

struct Harness {
    store: Arc<RunStore>,
    launcher: Arc<ScriptedLauncher>,
    _dir: tempfile::TempDir,
}

fn harness(scripts: impl IntoIterator<Item = Script>) -> Result<Harness> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new(scripts));
    let store = RunStore::open(dir.path(), launcher.clone(), env()).or_fail()?;
    Ok(Harness {
        store: Arc::new(store),
        launcher,
        _dir: dir,
    })
}

fn request(question: &str) -> Result<StartRequest> {
    Ok(StartRequest {
        delegate: codex()?,
        question: question.to_owned(),
        cwd: PathBuf::from("/work/project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
        label: None,
    })
}

impl Harness {
    /// The directory of turn `number`, counted from 1 as the directories are named.
    fn turn_dir(&self, run_id: &RunId, number: u32) -> PathBuf {
        self.store
            .root()
            .join("runs")
            .join(run_id.as_str())
            .join(format!("turns/{number:04}"))
    }

    fn transcript(&self, run_id: &RunId) -> Result<String> {
        std::fs::read_to_string(
            self.store
                .root()
                .join("runs")
                .join(run_id.as_str())
                .join("transcript.md"),
        )
        .or_fail()
    }
}

/// A turn whose stream has ended settles only once its delegate has gone, because both CLIs save
/// the session, and Codex writes the reply a lost one is recovered from, on their way out; then
/// its marker commits exactly the transcript that was published.
#[gtest]
fn a_turn_settles_once_its_delegate_has_gone() -> Result<()> {
    let harness = harness([Script::running(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request("q")?)?;
    let marker = harness.turn_dir(&started.run_id, 1).join("settled.json");

    // The stream says completed, but the delegate is still there.
    assert_that!(started.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(started.settled, eq(false));
    assert_that!(harness.store.finalize_turn(&started.run_id, 1)?, none());
    assert_that!(marker.exists(), eq(false));

    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(0));
    let settled = harness.store.finalize_turn(&started.run_id, 1)?.or_fail()?;

    // The marker vouches for the transcript as published: the turn's section ends where the file
    // does, and starts after the header.
    let transcript = harness.transcript(&started.run_id)?;
    assert_that!(
        settled.outcome(),
        matches_pattern!(Outcome::Completed { .. })
    );
    assert_that!(settled.settled_by(), eq(SettledBy::TerminalEvent));
    assert_that!(settled.turn_number(), eq(1));
    assert_that!(
        settled.transcript_end(),
        eq(u64::try_from(transcript.len()).or_fail()?)
    );
    let (header, section) = transcript
        .split_at_checked(usize::try_from(settled.transcript_start()).or_fail()?)
        .or_fail()?;
    assert_that!(header, starts_with("# Consultation"));
    assert_that!(section, starts_with("\n## Turn 1"));
    assert_that!(section, contains_substring("_Turn 1 completed"));
    assert_that!(marker.exists(), eq(true));
    assert_that!(harness.store.status(&started.run_id)?.settled, eq(true));
    Ok(())
}

/// The marker is written once and every later settlement adopts it, byte for byte.
///
/// One lost between publishing the transcript and committing the marker — the crash window
/// between the two — is written again from the same frozen input, so it commits the same bytes.
#[gtest]
fn a_settlement_is_committed_once_and_rewritten_the_same() -> Result<()> {
    let harness = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request("q")?)?;
    let marker = harness.turn_dir(&started.run_id, 1).join("settled.json");
    let first = harness.store.finalize_turn(&started.run_id, 1)?.or_fail()?;
    let committed = std::fs::read(&marker).or_fail()?;
    let transcript = harness.transcript(&started.run_id)?;

    // Adopted, not rewritten.
    let again = harness.store.finalize_turn(&started.run_id, 1)?.or_fail()?;
    harness.store.status(&started.run_id)?;
    assert_that!(again, eq(&first));
    assert_that!(std::fs::read(&marker).or_fail()?, eq(&committed));

    // Lost after the transcript was published, then repaired by the next reader.
    std::fs::remove_file(&marker).or_fail()?;
    harness.store.status(&started.run_id)?;
    assert_that!(std::fs::read(&marker).or_fail()?, eq(&committed));
    assert_that!(harness.transcript(&started.run_id)?, eq(&transcript));
    Ok(())
}

/// A delegate that left without a terminal event settles on its exit record, as an older store's
/// turn does the first time it is read.
#[gtest]
fn a_turn_its_exit_record_settles_commits_the_failure() -> Result<()> {
    let harness = harness([Script::exited(
        indoc::indoc! {r#"
            {"type":"thread.started","thread_id":"01a0"}
        "#},
        "boom",
        3,
    )])?;
    let started = harness.store.start(&request("q")?)?;

    let settled = harness.store.finalize_turn(&started.run_id, 1)?.or_fail()?;
    assert_that!(settled.settled_by(), eq(SettledBy::ExitRecord));
    assert_that!(
        settled.outcome(),
        matches_pattern!(Outcome::Failed {
            detail: contains_substring("boom"),
            ..
        })
    );
    Ok(())
}

/// A follow-up is admitted only after the turn it continues is committed, and a wait pinned to
/// that turn returns it, not the follow-up, with a transcript that ends where it did.
#[gtest]
#[tokio::test]
async fn a_pinned_wait_returns_its_own_turn_across_a_follow_up() -> Result<()> {
    let harness = harness([
        Script::running(fixtures::CODEX_HAPPY),
        Script::running(fixtures::CODEX_HAPPY),
    ])?;
    let started = harness.store.start(&request("first")?)?;
    let pinned = harness.store.pin_turn(&started.run_id, None)?;
    assert_that!(pinned.turn_number(), eq(1));

    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(0));
    let followed = harness.store.follow_up(&started.run_id, "second")?;
    assert_that!(followed.turns, eq(2));
    // Committed before the follow-up was claimed.
    assert_that!(
        harness
            .turn_dir(&started.run_id, 1)
            .join("settled.json")
            .exists(),
        eq(true)
    );

    let settled = harness.store.wait_for_settlement(&pinned).await?;
    assert_that!(settled.turn_number(), eq(1));
    let page = settled.page(0, usize::MAX)?;
    assert_that!(page.text, contains_substring("_Turn 1 completed"));
    assert_that!(page.text, not(contains_substring("## Turn 2")));
    assert_that!(page.total_bytes, eq(settled.transcript_end()));

    // The follow-up's own section starts where the first turn's settlement ended.
    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(1));
    let second = harness
        .store
        .wait_for_settlement(&harness.store.pin_turn(&started.run_id, Some(2))?)
        .await?;
    assert_that!(second.transcript_start(), eq(settled.transcript_end()));
    Ok(())
}

/// A turn is chosen by its one-based number, and one there is not is refused rather than waited
/// for.
#[gtest]
fn a_turn_that_does_not_exist_is_refused() -> Result<()> {
    let harness = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request("q")?)?;
    for turn in [0, 2] {
        assert_that!(
            harness.store.pin_turn(&started.run_id, Some(turn)),
            err(matches_pattern!(RunError::NoSuchTurn { turns: eq(&1), .. }))
        );
    }
    let missing = RunId::parse("20260101T000000-00000000").or_fail()?;
    assert_that!(
        harness.store.pin_turn(&missing, None),
        err(matches_pattern!(RunError::NotFound(_)))
    );
    Ok(())
}

/// A cancelled turn is committed as cancelled by the cancellation itself.
#[gtest]
#[tokio::test]
async fn a_cancelled_turn_settles_cancelled() -> Result<()> {
    let harness = harness([Script::running(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"turn.started"}
    "#})])?;
    let started = harness.store.start(&request("q")?)?;
    let pinned = harness.store.pin_turn(&started.run_id, None)?;
    harness.store.cancel(&started.run_id).await?;

    let settled = harness.store.wait_for_settlement(&pinned).await?;
    assert_that!(settled.outcome(), eq(&Outcome::Cancelled));
    Ok(())
}

/// A marker that does not hold for the files it vouches for is refused loudly, never handed out.
#[gtest]
fn a_settlement_that_does_not_hold_is_refused() -> Result<()> {
    let harness = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request("q")?)?;
    let marker = harness.turn_dir(&started.run_id, 1).join("settled.json");
    let committed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker).or_fail()?).or_fail()?;
    let with = |field: &str, value: serde_json::Value| {
        let mut record = committed.clone();
        if let Some(object) = record.as_object_mut() {
            object.insert(field.to_owned(), value);
        }
        record
    };

    // Unsupported version, running outcome, reversed range, bytes the transcript does not have
    for broken in [
        with("version", serde_json::json!(3)),
        with("outcome", serde_json::json!({"state": "running"})),
        with("transcript_start", serde_json::json!(1_000_000)),
    ] {
        std::fs::write(&marker, broken.to_string()).or_fail()?;
        assert_that!(
            harness.store.finalize_turn(&started.run_id, 1),
            err(matches_pattern!(RunError::InvalidSettlement { .. })),
            "{broken}"
        );
    }

    // Bytes the transcript does not have, which only reading a page of it can tell: the
    // transcript is a cache a fold may yet write again.
    std::fs::write(
        &marker,
        with("transcript_end", serde_json::json!(1_000_000)).to_string(),
    )
    .or_fail()?;
    let settled = harness.store.finalize_turn(&started.run_id, 1)?.or_fail()?;
    assert_that!(
        settled.page(0, usize::MAX),
        err(matches_pattern!(RunError::InvalidSettlement { .. }))
    );

    // Not a record at all
    std::fs::write(&marker, "{\"version\":").or_fail()?;
    assert_that!(
        harness.store.finalize_turn(&started.run_id, 1),
        err(matches_pattern!(RunError::CorruptRecord { .. }))
    );
    Ok(())
}

/// A deadline ends a wait with what had settled and what had not, and leaves the consultation
/// running; under `Any` the first settled one asked for is returned.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_deadline_ends_a_wait_and_says_what_is_pending() -> Result<()> {
    let harness = harness([
        Script::running(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let running = harness.store.start(&request("slow")?)?;
    let finished = harness.store.start(&request("quick")?)?;
    let targets = [
        harness.store.pin_turn(&running.run_id, None)?,
        harness.store.pin_turn(&finished.run_id, None)?,
    ];

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let all = harness
        .store
        .wait_for(&targets, WaitFor::All, Some(deadline))
        .await?;
    assert_that!(all.timed_out, eq(true));
    assert_that!(
        all.pending
            .iter()
            .map(|target| target.run_id().clone())
            .collect::<Vec<_>>(),
        elements_are![eq(&running.run_id)]
    );
    assert_that!(all.settled.len(), eq(1));
    assert_that!(tokio::time::Instant::now() >= deadline, eq(true));

    let any = harness.store.wait_for(&targets, WaitFor::Any, None).await?;
    assert_that!(any.timed_out, eq(false));
    assert_that!(
        any.settled
            .iter()
            .map(|settled| settled.run_id().clone())
            .collect::<Vec<_>>(),
        elements_are![eq(&finished.run_id)]
    );
    assert_that!(
        harness.store.status(&running.run_id)?.outcome.state(),
        eq(TurnState::Completed)
    );
    Ok(())
}

/// A wait sees a settlement within half a second of it becoming possible, however long it has
/// already waited: its interval backs off to that ceiling and no further.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_wait_returns_within_its_polling_ceiling() -> Result<()> {
    let harness = harness([Script::running(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request("q")?)?;
    let pinned = harness.store.pin_turn(&started.run_id, None)?;
    let waiting = tokio::spawn({
        let store = Arc::clone(&harness.store);
        async move { store.wait_for_settlement(&pinned).await }
    });

    // Long enough for the interval to have backed off fully.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_that!(waiting.is_finished(), eq(false));

    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(0));
    let gone_at = tokio::time::Instant::now();
    let settled = waiting.await.or_fail()??;
    assert_that!(
        gone_at.elapsed() <= Duration::from_millis(500),
        eq(true),
        "returned {:?} after the delegate left",
        gone_at.elapsed()
    );
    assert_that!(settled.turn_number(), eq(1));
    Ok(())
}

/// Write a supervisor record into a turn whose delegate it never recorded, as a supervisor that
/// died in `phase` leaves it.
///
/// The pid is one no scripted launcher hands out, which the launcher reports gone.
fn leave_a_dead_supervisor(harness: &Harness, run_id: &RunId, phase: &str) -> Result<()> {
    let turn = harness.turn_dir(run_id, 1);
    std::fs::remove_file(turn.join("launch.json")).or_fail()?;
    let record = serde_json::json!({
        "launch_id": "0123456789abcdef0123456789abcdef",
        "launched": {"pid": 4_100_000_000_u32},
        "started_at": "2026-10-20T00:00:00Z",
        "phase": phase,
    });
    std::fs::write(turn.join("supervisor.json"), record.to_string()).or_fail()
}

/// A supervisor that died before spawning anything leaves a turn that never started, which the
/// next reader records as a failure with what the supervisor printed; one that died while spawning
/// may have left a delegate nothing recorded, so its turn is neither settled, nor followed up, nor
/// cancelled, nor removed.
#[gtest]
#[tokio::test]
async fn a_dead_supervisor_leaves_a_failed_start_or_an_uncertain_one() -> Result<()> {
    let harness = harness([Script::running(""), Script::running("")])?;

    // Died before spawning
    let prepared = harness.store.start(&request("q")?)?;
    leave_a_dead_supervisor(&harness, &prepared.run_id, "prepared")?;
    std::fs::write(
        harness.turn_dir(&prepared.run_id, 1).join("supervisor.log"),
        "agentmux: out of file descriptors",
    )
    .or_fail()?;
    let status = harness.store.status(&prepared.run_id)?;
    assert_that!(
        status.outcome,
        matches_pattern!(Outcome::Failed {
            detail: contains_substring("out of file descriptors"),
            ..
        })
    );
    assert_that!(status.settled, eq(true));

    // Died while spawning
    let spawning = harness.store.start(&request("q")?)?;
    leave_a_dead_supervisor(&harness, &spawning.run_id, "spawning")?;
    assert_that!(
        harness.store.finalize_turn(&spawning.run_id, 1),
        err(matches_pattern!(RunError::UncertainStart { .. }))
    );
    assert_that!(
        harness.store.status(&spawning.run_id)?.outcome,
        eq(&Outcome::Running)
    );
    assert_that!(
        harness.store.remove(&spawning.run_id),
        err(matches_pattern!(RunError::StillRunning { .. }))
    );
    assert_that!(
        harness.store.follow_up(&spawning.run_id, "again"),
        err(anything())
    );

    // Nor can a cancellation end it: nothing it could stop is known, and a cancelled turn would
    // read as over while a delegate may still be billing, so its files are still kept.
    assert_that!(
        harness.store.cancel(&spawning.run_id).await,
        err(matches_pattern!(RunError::UncertainStart { .. }))
    );
    assert_that!(
        harness.store.remove(&spawning.run_id),
        err(matches_pattern!(RunError::StillRunning { .. }))
    );
    harness.store.sweep(Duration::ZERO)?;
    assert_that!(
        harness
            .store
            .root()
            .join("runs")
            .join(spawning.run_id.as_str())
            .is_dir(),
        eq(true)
    );
    Ok(())
}

/// A follow-up is admitted only once the turn it continues is committed, with the reply its CLI
/// wrote on its way out, wherever the delegate's departure falls among the observations one
/// reading of the consultation makes.
///
/// The turn being continued said nothing on its stream, so its reply exists only in the file its
/// CLI writes as it leaves; were the follow-up claimed first, that reply could never be recovered
/// into it.
#[gtest]
fn a_follow_up_waits_for_the_turn_it_continues_to_be_committed() -> Result<()> {
    let silent = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
        {"type":"item.completed","item":{"id":"i0","type":"assistant_message","text":"renamed away"}}
        {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
    "#};
    for observations in 0..16 {
        let harness = harness([
            Script::completed(fixtures::CODEX_HAPPY),
            Script {
                events: silent.to_owned(),
                alive_for_observations: Some(observations),
                last_message_on_departure: Some("the late findings".to_owned()),
                ..Script::default()
            },
            Script::completed(fixtures::CODEX_HAPPY),
        ])?;
        let started = harness.store.start(&request("q")?)?;
        harness.store.follow_up(&started.run_id, "the silent one")?;

        // Refused while the delegate may still be writing; each try observes it again.
        let mut admitted = false;
        for _ in 0..32 {
            match harness.store.follow_up(&started.run_id, "and then") {
                Ok(_) => {
                    admitted = true;
                    break;
                }
                Err(RunError::NotResumable(
                    agentmux::run::NotResumable::AwaitingDelegateExit { .. },
                )) => {}
                Err(err) => return Err(err.into()),
            }
        }
        assert_that!(admitted, eq(true), "after {observations} observation(s)");
        let page = harness.store.page(&started.run_id, 0, 1_000_000)?;
        let second_turn = page
            .text
            .split_once("## Turn 2")
            .and_then(|(_, rest)| rest.split_once("## Turn 3"))
            .map(|(second, _)| second)
            .or_fail()?;
        assert_that!(
            second_turn,
            contains_substring("the late findings"),
            "after {observations} observation(s)"
        );
    }
    Ok(())
}

/// A marker that does not hold is refused by every reader, not only by a wait: nothing folds,
/// renders or continues a turn from it.
#[gtest]
fn every_reader_refuses_a_settlement_that_does_not_hold() -> Result<()> {
    let harness = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request("q")?)?;
    let turn = harness.turn_dir(&started.run_id, 1);
    let marker = turn.join("settled.json");
    let committed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker).or_fail()?).or_fail()?;
    let with = |field: &str, value: serde_json::Value| {
        let mut record = committed.clone();
        if let Some(object) = record.as_object_mut() {
            object.insert(field.to_owned(), value);
        }
        record
    };

    // Unsupported version, running outcome, reversed range, a recovered reply that is not there
    for broken in [
        with("version", serde_json::json!(3)),
        with("outcome", serde_json::json!({"state": "running"})),
        with("transcript_start", serde_json::json!(1_000_000)),
        with("recovered_bytes", serde_json::json!(10)),
    ] {
        std::fs::write(&marker, broken.to_string()).or_fail()?;
        let invalid = || err(matches_pattern!(RunError::InvalidSettlement { .. }));
        assert_that!(
            harness.store.status(&started.run_id).map(|_| ()),
            invalid(),
            "{broken}"
        );
        assert_that!(
            harness.store.view(&started.run_id, 0, 1000).map(|_| ()),
            invalid(),
            "{broken}"
        );
        assert_that!(
            harness
                .store
                .follow_up(&started.run_id, "again")
                .map(|_| ()),
            invalid(),
            "{broken}"
        );
    }
    Ok(())
}

/// A wait that finds its turn settled but the transcript gone, or cut short, writes it again
/// from the frozen inputs, keeping the marker, rather than handing out a settlement whose bytes
/// are not there.
#[gtest]
#[tokio::test]
async fn a_wait_writes_a_lost_transcript_again() -> Result<()> {
    let harness = harness([Script::completed(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request("q")?)?;
    let pinned = harness.store.pin_turn(&started.run_id, None)?;
    let transcript_path = harness
        .store
        .root()
        .join("runs")
        .join(started.run_id.as_str())
        .join("transcript.md");
    let marker = harness.turn_dir(&started.run_id, 1).join("settled.json");
    let transcript = harness.transcript(&started.run_id)?;
    let committed = std::fs::read(&marker).or_fail()?;

    // Gone
    std::fs::remove_file(&transcript_path).or_fail()?;
    let settled = harness.store.wait_for_settlement(&pinned).await?;
    assert_that!(settled.page(0, usize::MAX)?.text, eq(&transcript));

    // Cut short
    std::fs::write(&transcript_path, "# Consultation").or_fail()?;
    let waited = harness
        .store
        .wait_for(std::slice::from_ref(&pinned), WaitFor::All, None)
        .await?;
    let settled = waited.settled.first().or_fail()?;
    assert_that!(settled.page(0, usize::MAX)?.text, eq(&transcript));
    assert_that!(std::fs::read(&marker).or_fail()?, eq(&committed));
    Ok(())
}

/// A wait chooses its turn without reading another turn's records, and under `Any` returns the
/// first settled consultation before looking at a later one, so damage elsewhere stops neither.
#[gtest]
#[tokio::test]
async fn damage_elsewhere_does_not_stop_a_wait() -> Result<()> {
    let harness = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let good = harness.store.start(&request("good")?)?;
    harness.store.follow_up(&good.run_id, "again")?;
    let bad = harness.store.start(&request("bad")?)?;
    for (run_id, number) in [(&good.run_id, 2), (&bad.run_id, 1)] {
        std::fs::write(
            harness.turn_dir(run_id, number).join("settled.json"),
            "{\"version\":",
        )
        .or_fail()?;
    }

    // Turn 1 is waited for although turn 2's marker is damaged.
    let first = harness.store.pin_turn(&good.run_id, Some(1))?;
    let settled = harness.store.wait_for_settlement(&first).await?;
    assert_that!(settled.turn_number(), eq(1));

    // The damaged consultation is pinned, and never looked at once the first has settled.
    let targets = [first, harness.store.pin_turn(&bad.run_id, None)?];
    let waited = harness.store.wait_for(&targets, WaitFor::Any, None).await?;
    assert_that!(
        waited
            .settled
            .iter()
            .map(|settled| settled.run_id().clone())
            .collect::<Vec<_>>(),
        elements_are![eq(&good.run_id)]
    );
    Ok(())
}

/// A wait never sits behind another process's hold of the run lock: settling the turn itself is
/// one attempt, so its deadline still ends it, and it settles the turn once the lock is free.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_held_lock_neither_blocks_a_wait_nor_its_deadline() -> Result<()> {
    let harness = harness([Script::running(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request("q")?)?;
    let pinned = harness.store.pin_turn(&started.run_id, None)?;
    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(0));

    // Held as another process would hold it.
    let lock_path = harness
        .store
        .root()
        .join("runs")
        .join(format!("{}.lock", started.run_id));
    let holder = std::fs::File::options()
        .write(true)
        .open(&lock_path)
        .or_fail()?;
    holder.lock().or_fail()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let held = harness
        .store
        .wait_for(std::slice::from_ref(&pinned), WaitFor::All, Some(deadline))
        .await?;
    assert_that!(held.timed_out, eq(true));

    drop(holder);
    let settled = harness.store.wait_for_settlement(&pinned).await?;
    assert_that!(settled.turn_number(), eq(1));
    Ok(())
}

/// A delegate that finishes its turn as it is cancelled, and then survives being stopped, leaves
/// a turn the cancellation's record still settles — so its supervisor commits it at once — but
/// one no follow-up continues until the delegate has actually gone.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_cancellation_the_delegate_survives_still_settles_the_turn() -> Result<()> {
    let harness = harness([Script {
        events: indoc::indoc! {r#"
            {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
            {"type":"turn.started"}
        "#}
        .to_owned(),
        flush_on_departure: Some(
            indoc::indoc! {r#"
                {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"done"}}
                {"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
            "#}
            .to_owned(),
        ),
        survives_stop: true,
        ..Script::default()
    }])?;
    let started = harness.store.start(&request("q")?)?;

    let cancelled = harness.store.cancel(&started.run_id).await?;
    assert_that!(
        cancelled.outcome,
        matches_pattern!(Outcome::Completed { .. })
    );
    assert_that!(cancelled.settled, eq(true));
    assert_that!(
        harness.store.finalize_turn(&started.run_id, 1)?,
        some(anything())
    );
    assert_that!(
        harness.store.follow_up(&started.run_id, "again"),
        err(matches_pattern!(RunError::NotResumable(matches_pattern!(
            agentmux::run::NotResumable::AwaitingDelegateExit { .. }
        ))))
    );
    Ok(())
}
