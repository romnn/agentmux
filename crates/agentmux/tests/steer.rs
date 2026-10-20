//! Steering a consultation through the run store, with the scripted launcher.
//!
//! The scripted launcher runs nothing, so where a test needs the driver's half of the mailbox it
//! plays that half itself, through the same public type the driver uses.
//! The driver's protocol with a server is covered by `appserver.rs`, and the two together by the
//! CLI's own end-to-end test, which runs the real executable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentmux::appserver::mailbox::{Mailbox, SteerReply};
use agentmux::appserver::{DriverPlan, STOP_BUDGET};
use agentmux::delegate::{
    AccountChoice, Billing, ClaudePermissions, CodexPermissions, CodexSandbox, Delegate, Effort,
    Isolation, ModelId,
};
use agentmux::launch::Launcher as _;
use agentmux::run::{
    NotRunningReason, Retention, RunError, RunId, RunStore, StartRequest, SteerError,
};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use agentmux::transcript::TurnState;
use googletest::prelude::*;

/// Where a test pretends the agentmux executable lives.
const DRIVER_PATH: &str = "/opt/agentmux/bin/agentmux";

/// A running Codex turn, as a driven turn's capture begins.
const RUNNING_CAPTURE: &str = indoc::indoc! {r#"
    {"type":"thread.started","thread_id":"th-fake"}
    {"type":"turn.started"}
"#};

fn env() -> BTreeMap<String, String> {
    [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn codex(isolation: Isolation) -> Result<Delegate> {
    Ok(Delegate::Codex {
        model: ModelId::parse("gpt-6-astra").or_fail()?,
        effort: Some(Effort::parse("high").or_fail()?),
        permissions: CodexPermissions {
            sandbox: Some(CodexSandbox::WorkspaceWrite),
            network_access: None,
        },
        account: AccountChoice::Default,
        isolation: Some(isolation),
    })
}

fn claude() -> Result<Delegate> {
    Ok(Delegate::Claude {
        permissions: ClaudePermissions::default(),
        model: ModelId::parse("claude-opus-5").or_fail()?,
        effort: Some(Effort::parse("xhigh").or_fail()?),
        account: AccountChoice::Default,
        isolation: None,
    })
}

struct Harness {
    store: Arc<RunStore>,
    launcher: Arc<ScriptedLauncher>,
    dir: tempfile::TempDir,
}

/// A store that drives inheriting Codex turns through [`DRIVER_PATH`].
fn harness(scripts: impl IntoIterator<Item = Script>) -> Result<Harness> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new(scripts));
    let store = RunStore::open(dir.path(), launcher.clone(), env())
        .or_fail()?
        .with_codex_app_server(DRIVER_PATH);
    Ok(Harness {
        store: Arc::new(store),
        launcher,
        dir,
    })
}

impl Harness {
    /// A consultation of `delegate` about a project in the harness's own directory.
    fn request(&self, delegate: Delegate) -> StartRequest {
        StartRequest {
            delegate,
            question: "Review the auth module.".to_owned(),
            cwd: self.dir.path().join("project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
        }
    }

    /// The plan the `index`th launch's driver was pointed at, read from the launch itself.
    fn plan(&self, index: usize) -> Result<DriverPlan> {
        let launch = self.launcher.launches().into_iter().nth(index).or_fail()?;
        let path = launch.args.get(1).or_fail()?;
        DriverPlan::read(Path::new(path)).or_fail()
    }

    /// The transcript as every reader sees it, from the top.
    fn transcript(&self, run_id: &RunId) -> Result<String> {
        Ok(self.store.page(run_id, 0, usize::MAX)?.text)
    }
}

/// Wait until a steered message has been written into `mailbox`, so a test can act on the turn
/// while the message is known to be waiting.
fn until_submitted(mailbox: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let waiting = std::fs::read_dir(mailbox).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".request.json")
            })
        });
        if waiting {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Play the driver's half of a turn's mailbox: answer the first message with `reply`.
fn spawn_steer_responder(
    mailbox: PathBuf,
    reply: SteerReply,
) -> std::thread::JoinHandle<Option<String>> {
    let mailbox = Mailbox::new(mailbox);
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(claim) = mailbox.claim_all().into_iter().next() {
                let _ = mailbox.write_reply(&claim, &reply);
                return Some(claim.text);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    })
}

/// A Codex consultation that inherits its account's settings is launched through the driver,
/// with a plan that carries what `codex exec` would have been given.
#[gtest]
fn an_inheriting_codex_turn_is_launched_through_the_driver() -> Result<()> {
    let first = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"th-fake"}
        {"type":"turn.started"}
        {"type":"item.completed","item":{"id":"i","type":"agent_message","text":"OK"}}
        {"type":"turn.completed"}
    "#};
    let harness = harness([Script::completed(first), Script::running(RUNNING_CAPTURE)])?;

    let request = harness.request(codex(Isolation::Inherit)?);
    let started = harness.store.start(&request)?;
    let launch = harness.launcher.launches().into_iter().next().or_fail()?;

    // The delegate is the driver, pointed at a plan.
    assert_that!(launch.program, eq(DRIVER_PATH));
    assert_that!(
        launch.args,
        elements_are![eq("codex-app-server"), anything()]
    );
    // The plan keeps the delegate's model, effort and sandbox, and the turn's own files.
    let plan = harness.plan(0)?;
    assert_that!(plan.settings.model.as_str(), eq("gpt-6-astra"));
    assert_that!(
        plan.settings.args,
        contains(eq("model_reasoning_effort=\"high\""))
    );
    assert_that!(plan.settings.sandbox, eq(CodexSandbox::WorkspaceWrite));
    assert_that!(plan.resume_thread_id, none());
    assert_that!(plan.billing, eq(Billing::Subscription));
    assert_that!(plan.cwd, eq(&request.cwd));
    // The status reports the pricing the turn runs on.
    assert_that!(started.billing, some(eq(Billing::Subscription)));

    // A follow-up is driven too, and resumes the thread the first turn announced.
    let followed = harness
        .store
        .follow_up(&started.run_id, "And the session code?")?;
    assert_that!(
        harness
            .plan(1)?
            .resume_thread_id
            .map(|session| session.to_string()),
        some(eq("th-fake"))
    );
    // The running follow-up is the one place `steer` would be offered.
    assert_that!(followed.steerable, eq(true));
    Ok(())
}

/// A consultation started by a store that drives nothing, as `--codex-launch exec` or
/// `AGENTMUX_CODEX_LAUNCH=exec` opens one, stays on `codex exec` when a store that would drive it
/// follows it up.
#[gtest]
#[tokio::test]
async fn a_consultation_started_without_a_driver_stays_on_exec() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let first = Arc::new(ScriptedLauncher::new([Script::completed(
        fixtures::CODEX_HAPPY,
    )]));
    let exec_only = RunStore::open(dir.path(), first.clone(), env()).or_fail()?;
    let started = exec_only.start(&StartRequest {
        delegate: codex(Isolation::Inherit)?,
        question: "q".to_owned(),
        cwd: dir.path().join("project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
    })?;
    exec_only
        .wait_until_terminal(&started.run_id, Duration::from_secs(5))
        .await?;

    let later = Arc::new(ScriptedLauncher::new([Script::running(RUNNING_CAPTURE)]));
    let driving = RunStore::open(dir.path(), later.clone(), env())
        .or_fail()?
        .with_codex_app_server(DRIVER_PATH);
    driving.follow_up(&started.run_id, "And the session code?")?;

    assert_that!(
        later
            .launches()
            .first()
            .map(|launch| launch.program.clone()),
        some(eq("codex"))
    );
    Ok(())
}

/// A consultation that asks for `codex exec` runs every turn with it, its follow-ups included,
/// even from a store that would drive it over the app-server, and is not offered `steer`.
#[gtest]
#[tokio::test]
async fn a_consultation_that_asks_for_exec_runs_over_exec_throughout() -> Result<()> {
    let harness = harness([
        Script::completed(fixtures::CODEX_HAPPY),
        Script::running(RUNNING_CAPTURE),
    ])?;
    let mut request = harness.request(codex(Isolation::Inherit)?);
    request.force_codex_exec = true;

    let started = harness.store.start(&request)?;
    harness
        .store
        .wait_until_terminal(&started.run_id, Duration::from_secs(5))
        .await?;
    let followed = harness
        .store
        .follow_up(&started.run_id, "And the session code?")?;

    let programs: Vec<String> = harness
        .launcher
        .launches()
        .into_iter()
        .map(|launch| launch.program)
        .collect();
    assert_that!(programs, elements_are![eq("codex"), eq("codex")]);
    assert_that!(followed.steerable, eq(false));
    Ok(())
}

/// A steer travels to the driver through the mailbox and returns once the delegate accepted it.
#[gtest]
#[tokio::test]
async fn a_steer_is_delivered_through_the_turns_mailbox() -> Result<()> {
    let harness = harness([Script::running(RUNNING_CAPTURE)])?;
    let started = harness
        .store
        .start(&harness.request(codex(Isolation::Inherit)?))?;
    assert_that!(started.steerable, eq(true));

    let driver = spawn_steer_responder(harness.plan(0)?.mailbox_dir(), SteerReply::Accepted);
    let steered = harness
        .store
        .steer(&started.run_id, "Only look at the auth module.")
        .await;

    assert_that!(steered.map(|status| status.run_id), ok(eq(&started.run_id)));
    assert_that!(
        driver.join().ok().flatten(),
        some(eq("Only look at the auth module."))
    );
    Ok(())
}

/// A refusal from the delegate CLI comes back in its own words.
#[gtest]
#[tokio::test]
async fn a_refused_steer_says_why() -> Result<()> {
    let harness = harness([Script::running(RUNNING_CAPTURE)])?;
    let started = harness
        .store
        .start(&harness.request(codex(Isolation::Inherit)?))?;
    let driver = spawn_steer_responder(
        harness.plan(0)?.mailbox_dir(),
        SteerReply::Rejected {
            reason: "no active turn".to_owned(),
        },
    );

    let refused = harness
        .store
        .steer(&started.run_id, "REFUSED-SENTINEL")
        .await;
    let _ = driver.join();

    assert_that!(
        refused,
        err(matches_pattern!(RunError::Steer(matches_pattern!(
            SteerError::Rejected {
                detail: eq("no active turn"),
                ..
            }
        ))))
    );
    // A refused message is in no reader's view of the consultation.
    assert_that!(
        harness.transcript(&started.run_id)?,
        not(contains_substring("REFUSED-SENTINEL"))
    );
    Ok(())
}

/// A message the driver sent but never heard back about is reported as exactly that: neither
/// delivered nor refused, with the driver's account of why.
#[gtest]
#[tokio::test]
async fn a_steer_the_driver_could_not_confirm_says_so() -> Result<()> {
    let harness = harness([Script::running(RUNNING_CAPTURE)])?;
    let started = harness
        .store
        .start(&harness.request(codex(Isolation::Inherit)?))?;
    let driver = spawn_steer_responder(
        harness.plan(0)?.mailbox_dir(),
        SteerReply::Unconfirmed {
            reason: "the turn ended before codex answered it".to_owned(),
        },
    );

    let unknown = harness.store.steer(&started.run_id, "did it land?").await;
    let _ = driver.join();

    assert_that!(
        unknown,
        err(matches_pattern!(RunError::Steer(matches_pattern!(
            SteerError::Unconfirmed {
                why: contains_substring("before codex answered"),
                ..
            }
        ))))
    );
    Ok(())
}

/// A message the driver claimed is never withdrawn: when no answer comes, because the driver left
/// or the delegate stayed silent, the steer is reported unconfirmed, and nothing is recorded.
///
/// A task plays the driver on the paused clock, so the silent case waits out the store's whole
/// confirmation deadline at once.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_claimed_steer_that_is_never_answered_is_unconfirmed() -> Result<()> {
    for (driver_leaves, why) in [
        (true, "left without answering"),
        (false, "did not confirm it within"),
    ] {
        let harness = harness([Script::running(RUNNING_CAPTURE)])?;
        let started = harness
            .store
            .start(&harness.request(codex(Isolation::Inherit)?))?;
        let mailbox = Mailbox::new(harness.plan(0)?.mailbox_dir());
        let launcher = Arc::clone(&harness.launcher);
        let driver = tokio::spawn(async move {
            for _ in 0..1000 {
                if !mailbox.claim_all().is_empty() {
                    if driver_leaves {
                        launcher.kill(launcher.launch_handle(0));
                    }
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        });
        let steered = harness
            .store
            .steer(&started.run_id, "UNANSWERED-SENTINEL")
            .await;

        assert_that!(driver.await.or_fail()?, eq(true));
        assert_that!(
            steered,
            err(matches_pattern!(RunError::Steer(matches_pattern!(
                SteerError::Unconfirmed {
                    why: contains_substring(why),
                    ..
                }
            ))))
        );
        assert_that!(
            harness.transcript(&started.run_id)?,
            not(contains_substring("UNANSWERED-SENTINEL"))
        );
    }
    Ok(())
}

/// A turn being cancelled takes no steer, and its status stops offering one.
#[gtest]
#[tokio::test]
async fn a_turn_being_cancelled_takes_no_steer() -> Result<()> {
    let harness = harness([Script::running(RUNNING_CAPTURE)])?;
    let started = harness
        .store
        .start(&harness.request(codex(Isolation::Inherit)?))?;
    std::fs::write(harness.plan(0)?.cancel_marker_path(), "").or_fail()?;

    assert_that!(harness.store.status(&started.run_id)?.steerable, eq(false));
    assert_that!(
        harness.store.steer(&started.run_id, "x").await,
        err(matches_pattern!(RunError::Steer(matches_pattern!(
            SteerError::NotRunning {
                state: eq(&NotRunningReason::Cancelling),
                ..
            }
        ))))
    );
    assert_that!(
        std::fs::read_dir(harness.plan(0)?.mailbox_dir())
            .map(Iterator::count)
            .unwrap_or(0),
        eq(0)
    );
    Ok(())
}

/// A driven turn takes no steer until its capture says the turn began, and the refusal says to
/// retry: not while the thread is opening, and not once it is open but the turn is still being
/// started, when the driver is not yet serving its mailbox.
#[gtest]
#[tokio::test]
async fn a_turn_not_yet_running_takes_no_steer() -> Result<()> {
    for (case, capture) in [
        ("opening its thread", ""),
        (
            "starting its turn",
            indoc::indoc! {r#"
                {"type":"thread.started","thread_id":"th-fake"}
            "#},
        ),
    ] {
        let harness = harness([Script::running(capture)])?;
        let started = harness
            .store
            .start(&harness.request(codex(Isolation::Inherit)?))?;

        assert_that!(started.steerable, eq(false), "{case}");
        assert_that!(
            harness.store.steer(&started.run_id, "x").await,
            err(all![
                matches_pattern!(RunError::Steer(matches_pattern!(
                    SteerError::Starting { .. }
                ))),
                displays_as(contains_substring("again in a few seconds"))
            ]),
            "{case}"
        );
        assert_that!(
            std::fs::read_dir(harness.plan(0)?.mailbox_dir())
                .map(Iterator::count)
                .unwrap_or(0),
            eq(0),
            "{case}"
        );
    }
    Ok(())
}

/// A message the driver never picked up is withdrawn once the turn has gone, so it cannot be
/// delivered later into a turn the caller was told it missed.
#[gtest]
#[tokio::test]
async fn a_steer_the_turn_outlives_is_withdrawn_unsent() -> Result<()> {
    let harness = harness([Script::running(RUNNING_CAPTURE)])?;
    let started = harness
        .store
        .start(&harness.request(codex(Isolation::Inherit)?))?;
    let mailbox = harness.plan(0)?.mailbox_dir();

    // The turn's child goes away while the message waits, unclaimed, in the mailbox.
    let launcher = Arc::clone(&harness.launcher);
    let stopper = std::thread::spawn({
        let mailbox = mailbox.clone();
        move || {
            if until_submitted(&mailbox) {
                launcher.terminate(launcher.launch_handle(0));
            }
        }
    });
    let withdrawn = harness
        .store
        .steer(&started.run_id, "WITHDRAWN-SENTINEL")
        .await;
    let _ = stopper.join();

    assert_that!(
        withdrawn,
        err(matches_pattern!(RunError::Steer(matches_pattern!(
            SteerError::Withdrawn {
                why: contains_substring("ended"),
                ..
            }
        ))))
    );
    // Withdrawn means the driver can never claim it afterwards, and nobody reads it.
    assert_that!(Mailbox::new(mailbox).claim_all(), is_empty());
    assert_that!(
        harness.transcript(&started.run_id)?,
        not(contains_substring("WITHDRAWN-SENTINEL"))
    );
    Ok(())
}

/// Every turn that cannot take a steer is refused with a reason the caller can act on.
#[gtest]
#[tokio::test]
async fn a_turn_that_cannot_be_steered_says_why() -> Result<()> {
    let harness = harness([
        Script::running(RUNNING_CAPTURE),
        Script::running(String::new()),
        Script::completed(fixtures::CODEX_HAPPY),
    ])?;

    // Isolated Codex runs over `exec`, because the app-server cannot skip the account's
    // configuration.
    let isolated = harness
        .store
        .start(&harness.request(codex(Isolation::Isolated)?))?;
    assert_that!(
        harness
            .launcher
            .launches()
            .first()
            .map(|launch| launch.args.clone()),
        some(contains(eq("exec")))
    );
    assert_that!(isolated.steerable, eq(false));
    assert_that!(
        harness.store.steer(&isolated.run_id, "x").await,
        err(displays_as(contains_substring("ignore_user_config: false")))
    );

    // Claude reads its question once.
    let claude = harness.store.start(&harness.request(claude()?))?;
    assert_that!(
        harness.store.steer(&claude.run_id, "x").await,
        err(displays_as(contains_substring("claude --print")))
    );

    // A finished consultation points at the follow-up instead.
    let finished = harness
        .store
        .start(&harness.request(codex(Isolation::Isolated)?))?;
    assert_that!(
        harness.store.steer(&finished.run_id, "x").await,
        err(matches_pattern!(RunError::Steer(matches_pattern!(
            SteerError::NotRunning {
                state: eq(&NotRunningReason::Ended(TurnState::Completed)),
                ..
            }
        ))))
    );

    // And an empty message says nothing to steer with.
    assert_that!(
        harness
            .store
            .steer(&isolated.run_id, indoc::indoc! {"  \n"})
            .await,
        err(matches_pattern!(RunError::Steer(eq(&SteerError::Empty))))
    );
    Ok(())
}

/// Without a driver, a Codex turn that would inherit runs over `exec`, and says it could have
/// been driven had the store been told how.
#[gtest]
#[tokio::test]
async fn a_store_without_a_driver_launches_codex_over_exec() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([Script::running(RUNNING_CAPTURE)]));
    let store = RunStore::open(dir.path(), launcher.clone(), env()).or_fail()?;

    let started = store.start(&StartRequest {
        delegate: codex(Isolation::Inherit)?,
        question: "q".to_owned(),
        cwd: dir.path().join("project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
    })?;
    assert_that!(
        launcher
            .launches()
            .first()
            .map(|launch| launch.program.clone()),
        some(eq("codex"))
    );
    assert_that!(
        store.steer(&started.run_id, "x").await,
        err(displays_as(contains_substring("AGENTMUX_CODEX_LAUNCH")))
    );
    Ok(())
}

/// A cancelled driven turn is given the chance to interrupt itself before any signal is sent,
/// for as long as the driver may take to leave.
///
/// A task plays the driver on the paused clock: it sees the cancelling marker, spends its whole
/// stop budget cleaning up, and leaves; the store records the cancellation without having
/// signalled it.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_cancelled_driver_is_given_its_whole_stop_budget_before_any_signal() -> Result<()> {
    let harness = harness([Script::running(RUNNING_CAPTURE)])?;
    let started = harness
        .store
        .start(&harness.request(codex(Isolation::Inherit)?))?;
    let marker = harness.plan(0)?.cancel_marker_path();

    let launcher = Arc::clone(&harness.launcher);
    let driver = tokio::spawn(async move {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(STOP_BUDGET).await;
        // Leaving is all the store can observe of a driver that interrupted its turn.
        launcher.kill(launcher.launch_handle(0));
    });
    let cancelled = harness.store.cancel(&started.run_id).await?;
    driver.await.or_fail()?;

    assert_that!(cancelled.outcome.state(), eq(TurnState::Cancelled));
    // The store sends a termination before anything harsher, and never had to.
    assert_that!(harness.launcher.terminated(), is_empty());
    Ok(())
}

/// A cancelled driven turn whose driver never leaves is stopped by signal once its stop budget
/// has passed, and the cancellation is still recorded.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_cancelled_driver_that_never_leaves_is_signalled() -> Result<()> {
    let harness = harness([Script::running(RUNNING_CAPTURE)])?;
    let started = harness
        .store
        .start(&harness.request(codex(Isolation::Inherit)?))?;

    let cancel_started_at = tokio::time::Instant::now();
    let cancelled = harness.store.cancel(&started.run_id).await?;

    assert_that!(cancel_started_at.elapsed() > STOP_BUDGET, eq(true));
    assert_that!(harness.plan(0)?.cancel_marker_path().exists(), eq(true));
    assert_that!(
        harness.launcher.terminated(),
        elements_are![eq(&harness.launcher.launch_handle(0).pid)]
    );
    assert_that!(cancelled.outcome.state(), eq(TurnState::Cancelled));
    Ok(())
}

/// A driver whose turn has already ended may still be stopping what its server left running, so
/// `cancel` waits for it to leave, for as long as its stop budget, before any signal.
///
/// A task plays the driver on the paused clock: it spends its whole stop budget cleaning up and
/// then leaves; the store neither signals it nor turns the completed turn into a cancelled one.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_cancel_after_a_completed_turn_waits_for_driver_cleanup() -> Result<()> {
    let ended = indoc::indoc! {r#"
        {"type":"thread.started","thread_id":"th-fake"}
        {"type":"turn.completed"}
    "#};
    let harness = harness([Script::running(ended)])?;
    let started = harness
        .store
        .start(&harness.request(codex(Isolation::Inherit)?))?;
    assert_that!(started.outcome.is_terminal(), eq(true));

    let launcher = Arc::clone(&harness.launcher);
    let driver = tokio::spawn(async move {
        tokio::time::sleep(STOP_BUDGET).await;
        launcher.kill(launcher.launch_handle(0));
    });
    let cancel_started_at = tokio::time::Instant::now();
    let cancelled = harness.store.cancel(&started.run_id).await?;
    driver.await.or_fail()?;

    assert_that!(cancel_started_at.elapsed() >= STOP_BUDGET, eq(true));
    assert_that!(harness.launcher.terminated(), is_empty());
    assert_that!(cancelled.outcome.state(), eq(TurnState::Completed));
    Ok(())
}
