//! Resuming a consultation after the model was overloaded, through the scripted launcher.
//!
//! The supervisor that waits and sends "continue" is a real process, exercised in
//! `agentmux-cli`'s tests.
//! Here the scripted launcher stands in for it: a store opened with a supervisor launches a
//! scripted one, and each test plays its admission by starting the delegate's script and
//! recording it, so what is decided as the turn settles, and what every reader says about it
//! afterwards, is checked without a process.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, Billing, CodexPermissions, CodexSandbox, Delegate, Effort, Invocation, ModelId,
};
use agentmux::launch::{LaunchSpec, Launcher as _};
use agentmux::run::{AutoResume, Retention, RunId, RunStatus, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher};
use agentmux::transcript::{FailureKind, Outcome};
use googletest::prelude::*;

/// A Codex turn that said something in a session it announced, then failed because the model
/// was at capacity, in the vendor's own words.
const OVERLOADED: &str = indoc::indoc! {r#"
    {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
    {"type":"turn.started"}
    {"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Halfway through."}}
    {"type":"turn.failed","error":{"message":"Selected model is at capacity. Please try a different model."}}
"#};

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
        question: "review this".to_owned(),
        cwd: PathBuf::from("/work/project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
    })
}

struct Harness {
    store: RunStore,
    launcher: Arc<ScriptedLauncher>,
    _dir: tempfile::TempDir,
}

/// A store whose every turn is launched under a supervisor that the launcher plays as a process
/// that stays, followed by the delegate scripts in `delegates`, one per turn.
fn supervised(delegates: impl IntoIterator<Item = Script>) -> Result<Harness> {
    let dir = tempfile::tempdir().or_fail()?;
    let mut scripts = vec![Script::running("")];
    scripts.extend(delegates);
    let launcher = Arc::new(ScriptedLauncher::new(scripts));
    let store = RunStore::open(dir.path(), launcher.clone(), env())
        .or_fail()?
        .with_supervisor("/opt/agentmux/bin/agentmux");
    Ok(Harness {
        store,
        launcher,
        _dir: dir,
    })
}

impl Harness {
    fn turn_dir(&self, run_id: &RunId, number: u32) -> PathBuf {
        self.store
            .root()
            .join("runs")
            .join(run_id.as_str())
            .join(format!("turns/{number:04}"))
    }

    /// Start the delegate of turn `number` as its supervisor would, from the launcher's script
    /// at `launch`, and record it.
    fn admit(&self, run_id: &RunId, number: u32, launch: usize) -> Result<()> {
        let dir = self.turn_dir(run_id, number);
        let invocation = Invocation::for_launcher_test(
            "codex",
            Vec::new(),
            BTreeMap::new(),
            Billing::Subscription,
        );
        let launched = self
            .launcher
            .launch(&LaunchSpec {
                invocation: &invocation,
                cwd: &PathBuf::from("/work/project"),
                question: &dir.join("question.md"),
                events: &dir.join("events.jsonl"),
                stderr: &dir.join("stderr.log"),
            })
            .or_fail()?;
        assert_that!(launched, eq(self.launcher.launch_handle(launch)));
        let record = serde_json::json!({
            "launched": launched,
            "started_at": chrono::Utc::now(),
        });
        std::fs::write(dir.join("launch.json"), record.to_string()).or_fail()
    }

    /// Start a consultation whose first turn fails overloaded, and read its status once the turn
    /// has settled.
    fn overloaded_consultation(&self) -> Result<(RunId, RunStatus)> {
        let run_id = self.store.start(&request()?).or_fail()?.run_id;
        self.admit(&run_id, 1, 1)?;
        let status = self.store.status(&run_id).or_fail()?;
        Ok((run_id, status))
    }
}

/// The turn that failed overloaded settles with a resume scheduled for its supervisor: the next
/// turn, the first of twelve attempts, due thirty seconds after the settlement.
/// The consultation stays failed and continuable, and the decision is on disk beside the
/// settlement.
#[gtest]
fn an_overloaded_turn_is_scheduled_for_an_automatic_resume() -> Result<()> {
    let harness = supervised([Script::completed(OVERLOADED)])?;
    let before = chrono::Utc::now();
    let (run_id, status) = harness.overloaded_consultation()?;

    assert_that!(
        status.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::Overloaded),
            detail: contains_substring("at capacity"),
        })
    );
    assert_that!(status.settled, eq(true));
    assert_that!(status.resumable, eq(true));
    assert_that!(
        status.auto_resume,
        some(matches_pattern!(AutoResume::Scheduled {
            turn: eq(&2),
            attempt: eq(&1),
            attempts: eq(&12),
            due_at: ge(&(before + chrono::TimeDelta::seconds(30))),
        }))
    );
    assert_that!(
        harness
            .turn_dir(&run_id, 1)
            .join("auto-resume.json")
            .exists(),
        eq(true)
    );
    Ok(())
}

/// Once the supervisor that was to resume has gone without resuming, every reader says the
/// resume did not happen, rather than promising it.
#[gtest]
fn a_resume_whose_supervisor_has_gone_is_abandoned() -> Result<()> {
    let harness = supervised([Script::completed(OVERLOADED)])?;
    let (run_id, _) = harness.overloaded_consultation()?;

    harness
        .launcher
        .mark_process_gone(harness.launcher.launch_handle(0));
    let status = harness.store.status(&run_id).or_fail()?;
    assert_that!(
        status.auto_resume,
        some(matches_pattern!(AutoResume::Abandoned {
            due_at: anything(),
            supervisor_log: anything(),
        }))
    );
    Ok(())
}

/// `cancel` on a consultation whose resume is pending stops the resume and leaves the turn as it
/// failed; a stopped resume is never started.
#[gtest]
#[tokio::test]
async fn cancel_stops_a_pending_resume() -> Result<()> {
    let harness = supervised([Script::completed(OVERLOADED)])?;
    let (run_id, _) = harness.overloaded_consultation()?;

    let status = harness.store.cancel(&run_id).await.or_fail()?;
    assert_that!(
        status.auto_resume,
        some(matches_pattern!(AutoResume::Cancelled {
            attempt: eq(&1),
            attempts: eq(&12),
        }))
    );
    assert_that!(
        status.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::Overloaded),
            detail: anything(),
        })
    );
    // Nothing was signalled: the delegate had already gone, and the supervisor is left to see
    // the marker and leave.
    assert_that!(harness.launcher.terminated(), is_empty());
    Ok(())
}

/// A follow-up the caller sends before the resume is due takes its place, and the resume has
/// nothing more to say about the turn.
#[gtest]
fn a_follow_up_takes_the_place_of_a_pending_resume() -> Result<()> {
    // The second script is the follow-up's own supervisor, which stays.
    let harness = supervised([Script::completed(OVERLOADED), Script::running("")])?;
    let (run_id, _) = harness.overloaded_consultation()?;

    let followed = harness.store.follow_up(&run_id, "go on").or_fail()?;
    assert_that!(followed.turns, eq(2));
    assert_that!(harness.store.auto_resume(&run_id, 1).or_fail()?, none());
    // The follow-up is the caller's, and nothing about it is automatic.
    let invocation: serde_json::Value = serde_json::from_slice(
        &std::fs::read(harness.turn_dir(&run_id, 2).join("invocation.json")).or_fail()?,
    )
    .or_fail()?;
    assert_that!(invocation.get("auto_resume_attempt"), none());
    Ok(())
}

/// A turn that was itself the last automatic resume a run allows, and failed overloaded too, is
/// not resumed again, and says that agentmux has stopped.
#[gtest]
fn the_last_allowed_resume_failing_overloaded_ends_the_run() -> Result<()> {
    let harness = supervised([Script::completed(OVERLOADED)])?;
    let run_id = harness.store.start(&request()?).or_fail()?.run_id;

    // Recorded as the twelfth automatic resume in a row, as its supervisor would have launched it.
    let invocation_path = harness.turn_dir(&run_id, 1).join("invocation.json");
    let mut invocation: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&invocation_path).or_fail()?).or_fail()?;
    invocation
        .as_object_mut()
        .or_fail()?
        .insert("auto_resume_attempt".to_owned(), serde_json::json!(12));
    std::fs::write(&invocation_path, invocation.to_string()).or_fail()?;
    harness.admit(&run_id, 1, 1)?;

    let status = harness.store.status(&run_id).or_fail()?;
    assert_that!(
        status.auto_resume,
        some(matches_pattern!(AutoResume::Exhausted {
            attempts: eq(&12)
        }))
    );
    Ok(())
}

/// A store with no supervisor has nothing to resume a turn with, so an overloaded turn is a
/// failure the caller continues by hand, and nothing is recorded about a resume.
#[gtest]
fn an_unsupervised_store_never_schedules_a_resume() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([Script::completed(OVERLOADED)]));
    let store = RunStore::open(dir.path(), launcher, env()).or_fail()?;
    let status = store.start(&request()?).or_fail()?;

    assert_that!(
        status.outcome,
        matches_pattern!(Outcome::Failed {
            kind: eq(&FailureKind::Overloaded),
            detail: anything(),
        })
    );
    assert_that!(status.resumable, eq(true));
    assert_that!(status.auto_resume, none());
    Ok(())
}
