//! What a turn left running, as its `left-running.json` reaches the transcript and `status`,
//! through the scripted launcher: the record is written into the turn directory as a driver
//! over `codex app-server` writes it before it exits.
//!
//! The driver's own side — finding the processes and writing the record — is exercised against
//! real processes in `agentmux-cli`'s tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, CodexPermissions, CodexSandbox, Delegate, Effort, ModelId,
};
use agentmux::left_running::{LeftProcess, LeftRunning, ProcessFate};
use agentmux::report::Warning;
use agentmux::run::{Retention, RunId, RunStore, StartRequest, TurnLeftRunning};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use agentmux::transcript::Outcome;
use googletest::prelude::*;

fn env() -> BTreeMap<String, String> {
    [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn request() -> Result<StartRequest> {
    Ok(StartRequest {
        delegate: Delegate::Codex {
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            effort: Some(Effort::parse("high").or_fail()?),
            permissions: CodexPermissions {
                sandbox: Some(CodexSandbox::ReadOnly),
                network_access: None,
            },
            account: AccountChoice::Default,
            isolation: None,
        },
        question: "q".to_owned(),
        cwd: PathBuf::from("/work/project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
        label: None,
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

impl Harness {
    /// The directory of turn `number`, counted from 1 as the directories are named.
    fn turn_dir(&self, run_id: &RunId, number: u32) -> PathBuf {
        self.store
            .root()
            .join("runs")
            .join(run_id.as_str())
            .join(format!("turns/{number:04}"))
    }

    /// Keep `record` as what turn `number` left running, as its driver does before it exits.
    fn keep_left_running(&self, run_id: &RunId, number: u32, record: &str) -> Result<()> {
        std::fs::write(
            self.turn_dir(run_id, number).join("left-running.json"),
            record,
        )
        .or_fail()
    }

    fn transcript(&self, run_id: &RunId) -> Result<String> {
        Ok(self.store.page(run_id, 0, usize::MAX)?.text)
    }
}

/// Three processes, one of them still running after it was signalled.
fn three_left() -> Result<LeftRunning> {
    let process = |pid: u32, name: &str, fate: ProcessFate| LeftProcess {
        pid,
        start_time_secs: 1_790_000_000,
        name: name.to_owned(),
        fate,
    };
    LeftRunning::of(vec![
        process(120, "sh", ProcessFate::NoLongerRunning),
        process(123, "python3", ProcessFate::StillRunning),
        process(130, "sleep", ProcessFate::NoLongerRunning),
    ])
    .or_fail()
}

const THREE_LEFT_NOTE: &str = indoc::indoc! {"

    > [!WARNING]
    > **When this turn ended, 3 processes outside the delegate CLI's process tree were still running after the CLI exited**: `python3` (pid 123), `sh` (pid 120) and `sleep` (pid 130).
    > agentmux signalled them to stop; 2 are no longer running and 1 still is.
    > Work in them may be incomplete.
"};

/// What a turn left running is said once its delegate has gone, after the footer, and from then
/// on every read says the same: the transcript only grew, a follow-up's turn renders below it,
/// and `status` names the turn in its report and its warnings.
#[gtest]
fn what_a_turn_left_running_is_said_once_its_delegate_has_gone() -> Result<()> {
    let harness = harness([
        Script::running(fixtures::CODEX_HAPPY),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;
    let started = harness.store.start(&request()?)?;
    let record = serde_json::to_string(&three_left()?).or_fail()?;

    // The stream has ended and the record is there, but the driver has not exited yet: nothing
    // is said, because the record is final only once it has.
    harness.keep_left_running(&started.run_id, 1, &record)?;
    let before = harness.transcript(&started.run_id)?;
    assert_that!(before, contains_substring("_Turn 1 completed"));
    assert_that!(before, not(contains_substring("[!WARNING]")));
    assert_that!(
        harness.store.status(&started.run_id)?.left_running,
        is_empty()
    );

    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(0));
    harness.store.finalize_turn(&started.run_id, 1)?.or_fail()?;

    // The note is appended after everything already published, and is the turn's last word.
    let settled = harness.transcript(&started.run_id)?;
    assert_that!(settled, starts_with(before.as_str()));
    assert_that!(settled, ends_with(THREE_LEFT_NOTE));
    assert_that!(harness.transcript(&started.run_id)?, eq(&settled));

    let status = harness.store.status(&started.run_id)?;
    let expected = TurnLeftRunning {
        turn: 1,
        report: three_left()?,
    };
    assert_that!(status.left_running, elements_are![eq(&expected)]);
    assert_that!(
        status.warnings(),
        contains(eq(&Warning::LeftRunning(&expected)))
    );
    assert_that!(
        agentmux::report::left_running_line(&expected),
        eq(
            "turn 1 left 3 processes running outside the delegate CLI's process tree after the CLI \
             exited; agentmux signalled them to stop, and 2 are no longer running and 1 still is \
             — work in them may be incomplete"
        )
    );
    let json = serde_json::to_value(&status).or_fail()?;
    assert_that!(
        json.pointer("/left_running/0/turn"),
        some(eq(&serde_json::json!(1)))
    );
    assert_that!(
        json.pointer("/left_running/0/processes/0/fate"),
        some(eq(&serde_json::json!("still_running")))
    );

    // A follow-up's turn renders below it, and the settled turn reads exactly as it did.
    let followed = harness.store.follow_up(&started.run_id, "again")?;
    assert_that!(followed.left_running, elements_are![eq(&expected)]);
    let after_follow_up = harness.transcript(&started.run_id)?;
    assert_that!(after_follow_up, starts_with(settled.as_str()));
    assert_that!(after_follow_up, contains_substring("## Turn 2 — question"));
    Ok(())
}

/// A cancelled turn's leftovers are the cancellation's doing: a record its driver kept while it
/// was stopped is never said, even when the turn got its terminal event out as it went.
#[gtest]
#[tokio::test]
async fn a_cancelled_turn_never_says_what_it_left_running() -> Result<()> {
    let record = serde_json::to_string(&three_left()?).or_fail()?;
    let partial = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"partial"}}
    "#};
    let tail = indoc::indoc! {r#"
        {"type":"turn.completed","usage":{"input_tokens":1}}
    "#};
    for (script, outcome_is_cancelled) in [
        (Script::running(partial), true),
        (Script::flushes_when_stopped(partial, tail), false),
    ] {
        let harness = harness([script])?;
        let started = harness.store.start(&request()?)?;
        harness.keep_left_running(&started.run_id, 1, &record)?;

        let cancelled = harness.store.cancel(&started.run_id).await?;
        assert_that!(
            matches!(cancelled.outcome, Outcome::Cancelled),
            eq(outcome_is_cancelled)
        );
        assert_that!(cancelled.settled, eq(true));
        assert_that!(cancelled.left_running, is_empty());
        let transcript = harness.transcript(&started.run_id)?;
        assert_that!(transcript, not(contains_substring("[!WARNING]")));
        // Every later read agrees.
        assert_that!(harness.transcript(&started.run_id)?, eq(&transcript));
        assert_that!(
            harness.store.status(&started.run_id)?.left_running,
            is_empty()
        );
    }
    Ok(())
}

/// A turn being cancelled says nothing of what it left running from the moment `cancel` marks
/// it, before the cancellation is recorded: a read in between, after the driver finished and
/// left, must not publish a note that the cancellation's record would then take back.
#[gtest]
fn a_turn_marked_for_cancelling_never_says_what_it_left_running() -> Result<()> {
    let harness = harness([Script::running(indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"01a0"}
        {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"partial"}}
    "#})])?;
    let started = harness.store.start(&request()?)?;
    let dir = harness.turn_dir(&started.run_id, 1);

    // `cancel` found the turn running and marked it; then, before it could record anything, the
    // driver ended the turn, kept what it left running, and exited.
    std::fs::write(dir.join("cancelling"), "").or_fail()?;
    let mut events = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("events.jsonl"))
        .or_fail()?;
    std::io::Write::write_all(
        &mut events,
        b"{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1}}\n",
    )
    .or_fail()?;
    harness.keep_left_running(
        &started.run_id,
        1,
        &serde_json::to_string(&three_left()?).or_fail()?,
    )?;
    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(0));

    // A read in between settles the turn from its terminal event, and says nothing of the
    // record.
    let between = harness.store.status(&started.run_id)?;
    assert_that!(between.settled, eq(true));
    assert_that!(between.left_running, is_empty());
    let published = harness.transcript(&started.run_id)?;
    assert_that!(published, not(contains_substring("[!WARNING]")));

    // The cancellation's record arrives, as `cancel` writes it, and the transcript stays as
    // published.
    let events_len = std::fs::metadata(dir.join("events.jsonl")).or_fail()?.len();
    std::fs::write(
        dir.join("exit.json"),
        serde_json::json!({ "kind": "cancelled", "events_len": events_len }).to_string(),
    )
    .or_fail()?;
    assert_that!(harness.transcript(&started.run_id)?, eq(&published));
    assert_that!(
        harness.store.status(&started.run_id)?.left_running,
        is_empty()
    );
    Ok(())
}

/// A record that is there but cannot be read fails the read rather than publishing the turn
/// without the note an earlier read may have published.
#[gtest]
fn a_record_that_cannot_be_read_fails_the_read() -> Result<()> {
    let harness = harness([Script::running(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request()?)?;
    std::fs::create_dir(
        harness
            .turn_dir(&started.run_id, 1)
            .join("left-running.json"),
    )
    .or_fail()?;
    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(0));
    assert_that!(
        harness.store.status(&started.run_id),
        err(displays_as(contains_substring("left-running.json")))
    );
    Ok(())
}

/// A record that does not parse is no record: the turn reads as if it left nothing, and reading
/// the consultation does not fail.
#[gtest]
fn a_corrupt_record_is_no_record() -> Result<()> {
    let harness = harness([Script::running(fixtures::CODEX_HAPPY)])?;
    let started = harness.store.start(&request()?)?;
    harness.keep_left_running(&started.run_id, 1, "{not json")?;
    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(0));
    let status = harness.store.status(&started.run_id)?;
    assert_that!(status.settled, eq(true));
    assert_that!(status.left_running, is_empty());
    assert_that!(
        harness.transcript(&started.run_id)?,
        not(contains_substring("[!WARNING]"))
    );
    Ok(())
}
