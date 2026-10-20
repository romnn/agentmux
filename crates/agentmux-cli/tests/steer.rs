//! A steered consultation end to end: the real `agentmux` executable as the supervisor and the
//! driver, against a fake `codex` that speaks the app-server protocol.
//!
//! Everything between the caller and the server is real here — the `agentmux steer` command, the
//! detached supervisor and the driver it starts, the driver's handshake, the mailbox,
//! `turn/steer`, the follow-up's `thread/resume`, and the cancellation that interrupts over the
//! protocol — and nothing reaches a vendor.
//! The fake is `codex-app-server-mock`'s `codex` binary, built for the test run on first use; it
//! reads its scenario ([`codex_app_server_mock::scenario`]) from codex's home, where the driver
//! lets codex's own variables through.
//! Unix only: what a turn leaves running is checked as processes in sessions of their own, which
//! Windows does not have.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentmux::delegate::{AccountChoice, CodexSandbox, Delegate, Effort, Isolation, ModelId};
use agentmux::launch::ProcessLauncher;
use agentmux::run::{Retention, RunId, RunStore, StartRequest};
use agentmux::transcript::Outcome;
use codex_app_server_mock::scenario::{CONFIG_FILE, Config, HELPER, Scenario, ServerExitMode};
use googletest::prelude::*;

/// The directory holding the fake `codex`, `codex-app-server-mock`'s binary, built once per test
/// process with the profile and target the tests run with.
///
/// Built here because nothing else builds it: `cargo test` builds the binaries of the package under
/// test only.
fn fake_codex_dir() -> Result<PathBuf> {
    static BUILT: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
        std::sync::OnceLock::new();
    let built = BUILT.get_or_init(|| {
        let run = escargot::CargoBuild::new()
            .package("codex-app-server-mock")
            .bin("codex")
            .current_release()
            .current_target()
            .run()
            .map_err(|err| format!("cannot build the fake codex: {err}"))?;
        run.path()
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "the fake codex was built nowhere".to_owned())
    });
    built.clone().or_fail()
}

/// A `PATH` on which `codex` is the fake.
fn path_with_fake() -> Result<String> {
    Ok(format!("{}:/usr/bin:/bin", fake_codex_dir()?.display()))
}

/// Write the fake's configuration into `codex_home`.
fn configure(codex_home: &Path, config: &Config) -> Result<()> {
    std::fs::create_dir_all(codex_home).or_fail()?;
    let json = serde_json::to_string(config).or_fail()?;
    std::fs::write(codex_home.join(CONFIG_FILE), json).or_fail()
}

struct CliTestHarness {
    _root: tempfile::TempDir,
    store: RunStore,
    state_dir: PathBuf,
    host_env: BTreeMap<String, String>,
    project_dir: PathBuf,
    /// Every message the fake received, one per line.
    client_messages_path: PathBuf,
    /// The fake's environment, as it started.
    server_env_path: PathBuf,
    /// The pids of the commands the fake's turns started.
    command_pids_path: PathBuf,
}

/// A store whose Codex turns are supervised and driven by the `agentmux` under test, with the fake
/// on `PATH` and keys for both vendors in the environment agentmux was started with.
///
/// Each turn runs until steered, which completes it, or interrupted, and starts a command in a
/// session of its own that it leaves running however it ends, as codex's sandbox does.
fn cli_harness() -> Result<CliTestHarness> {
    let root = tempfile::tempdir().or_fail()?;
    let home = root.path().join("home");
    let project_dir = root.path().join("project");
    for dir in [&home, &project_dir] {
        std::fs::create_dir_all(dir).or_fail()?;
    }
    let client_messages_path = root.path().join("requests.jsonl");
    let server_env_path = root.path().join("codex-env.txt");
    let command_pids_path = root.path().join("commands.txt");
    configure(
        &home.join(".codex"),
        &Config {
            scenario: Scenario::Steerable,
            client_messages_path: Some(client_messages_path.clone()),
            environment_dump_path: Some(server_env_path.clone()),
            command_pids_path: command_pids_path.clone(),
        },
    )?;

    let host_env: BTreeMap<String, String> = [
        ("PATH", path_with_fake()?),
        ("HOME", home.display().to_string()),
        ("OPENAI_API_KEY", "sk-must-not-arrive".to_owned()),
        ("CODEX_API_KEY", "sk-must-not-arrive-either".to_owned()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect();
    let state_dir = root.path().join("state");
    let store = RunStore::open(
        &state_dir,
        Arc::new(ProcessLauncher::new()),
        host_env.clone(),
    )
    .or_fail()?
    .with_codex_app_server(env!("CARGO_BIN_EXE_agentmux"))
    .with_supervisor(env!("CARGO_BIN_EXE_agentmux"));
    Ok(CliTestHarness {
        _root: root,
        store,
        state_dir,
        host_env,
        project_dir,
        client_messages_path,
        server_env_path,
        command_pids_path,
    })
}

impl CliTestHarness {
    /// Run `agentmux steer` on the consultation, as a caller in a shell would.
    async fn run_steer_command(&self, id: &str, message: &str) -> Result<std::process::Output> {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_agentmux"));
        command
            .args(["steer", id, message])
            .env_clear()
            .envs(&self.host_env)
            .env("AGENTMUX_STATE_DIR", &self.state_dir);
        tokio::task::spawn_blocking(move || command.output())
            .await
            .or_fail()?
            .or_fail()
    }
}

/// The pids of the commands the fake's turns started, in the order they started.
fn read_command_pids(file: &Path) -> Vec<u32> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// What the process table says about one process.
struct ProcessSnapshot {
    /// Whether it runs, rather than having exited without being waited for.
    running: bool,
    command_line: String,
}

/// The process `pid`, or `None` when there is none.
fn process_snapshot(pid: u32) -> Option<ProcessSnapshot> {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
    );
    system.process(pid).map(|process| ProcessSnapshot {
        running: !matches!(
            process.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        ),
        command_line: process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
    })
}

/// The parent of process `pid`, where the platform says.
#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system
        .process(pid)
        .and_then(sysinfo::Process::parent)
        .map(sysinfo::Pid::as_u32)
}

/// Whether `pid` is a running process, rather than gone or exited and not yet waited for.
fn is_running(pid: u32) -> bool {
    process_snapshot(pid).is_some_and(|process| process.running)
}

/// Whether the fake's turns have started exactly `count` commands and the newest is running,
/// waiting a few seconds for it.
async fn wait_for_newest_command(file: &Path, count: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let pids = read_command_pids(file);
        if pids.len() == count && pids.last().copied().is_some_and(is_running) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Whether the fake's turns started exactly `count` commands and every one is gone, waiting a
/// few seconds for them.
async fn wait_for_commands_to_exit(file: &Path, count: usize) -> bool {
    let pids = read_command_pids(file);
    if pids.len() != count {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !pids.iter().copied().any(is_running) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// The messages of `method` the fake has received, in order.
fn client_messages_for_method(log: &Path, method: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|request| request.get("method").and_then(serde_json::Value::as_str) == Some(method))
        .collect()
}

/// Wait for the fake to have received `count` requests of `method`.
async fn until_received(log: &Path, method: &str, count: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if client_messages_for_method(log, method).len() >= count {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Wait a few seconds for `path` to exist.
async fn until_exists(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Wait for the driver to announce the consultation's turn, which takes no steer before that.
async fn until_steerable(store: &RunStore, id: &RunId) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if store.status(id)?.steerable {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(false)
}

/// Wait for the consultation to take a follow-up, which it does once its turn is over and its
/// delegate has left, having saved the session the follow-up resumes.
async fn until_resumable(store: &RunStore, id: &RunId) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if store.status(id)?.resumable {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(false)
}

/// Start, steer, finish, follow up, and cancel one consultation, through the real driver.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_consultation_is_steered_followed_up_and_cancelled_over_the_app_server() -> Result<()> {
    let harness = cli_harness()?;
    let started = harness.store.start(&StartRequest {
        delegate: Delegate::Codex {
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            effort: Some(Effort::parse("high").or_fail()?),
            sandbox: CodexSandbox::ReadOnly,
            account: AccountChoice::Default,
            isolation: Some(Isolation::Inherit),
        },
        question: "Review the repository.".to_owned(),
        cwd: harness.project_dir.clone(),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
    })?;
    let id = started.run_id.clone();
    assert_that!(until_steerable(&harness.store, &id).await?, eq(true));
    assert_that!(
        wait_for_newest_command(&harness.command_pids_path, 1).await,
        eq(true)
    );

    // Steer the running turn from the command line; the fake completes the turn when steered.
    let steered = harness
        .run_steer_command(id.as_str(), "Only look at the auth module.")
        .await?;
    assert_that!(steered.status.success(), eq(true));
    assert_that!(
        String::from_utf8_lossy(&steered.stdout).into_owned(),
        contains_substring("steered: the delegate accepted the message")
    );
    // The supervisor settles the turn once the driver has cleaned up and gone, with nothing
    // asking: its marker appears on its own.
    let turn_dir = |index: u32| {
        harness
            .state_dir
            .join("runs")
            .join(id.as_str())
            .join("turns")
            .join(format!("{index:04}"))
    };
    assert_that!(
        until_exists(&turn_dir(0).join("settled.json")).await,
        eq(true)
    );
    harness
        .store
        .wait_until_terminal(&id, Duration::from_secs(30))
        .await?;

    // A steer once the turn has ended is refused by the command, and reaches nothing.
    let late = harness
        .run_steer_command(id.as_str(), "NEVER-DELIVERED")
        .await?;
    assert_that!(late.status.success(), eq(false));
    assert_that!(
        String::from_utf8_lossy(&late.stderr).into_owned(),
        contains_substring("not running")
    );
    let (status, page) = harness.store.view(&id, 0, 1_000_000)?;
    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(status.steerable, eq(false));
    assert_that!(until_resumable(&harness.store, &id).await?, eq(true));
    // Every message the fake sent decoded into the generated protocol types.
    assert_that!(status.unrecognised.is_empty(), eq(true));

    // The instruction is in the transcript as a user fragment, above the reply it produced.
    let steer_at = page
        .text
        .find("### user (steer)\n\nOnly look at the auth module.")
        .or_fail()?;
    let reply_at = page.text.find("STEERED: auth only").or_fail()?;
    assert_that!(steer_at < reply_at, eq(true));
    assert_that!(page.text, not(contains_substring("NEVER-DELIVERED")));
    assert_that!(
        client_messages_for_method(&harness.client_messages_path, "turn/steer").len(),
        eq(1)
    );
    // The command the finished turn left running went with its server.
    assert_that!(
        wait_for_commands_to_exit(&harness.command_pids_path, 1).await,
        eq(true)
    );

    // The server was asked for no key, and was given none.
    let server_env = std::fs::read_to_string(&harness.server_env_path).or_fail()?;
    assert_that!(server_env, not(contains_substring("sk-must-not-arrive")));

    // A follow-up is a new turn on the same thread.
    let followed = harness.store.follow_up(&id, "And the session code?")?;
    assert_that!(followed.turns, eq(2));
    assert_that!(
        until_received(&harness.client_messages_path, "turn/start", 2).await,
        eq(true)
    );
    let resumed: Vec<serde_json::Value> =
        client_messages_for_method(&harness.client_messages_path, "thread/resume")
            .into_iter()
            .filter_map(|request| request.get("params").cloned())
            .collect();
    assert_that!(
        resumed,
        elements_are![eq(&serde_json::json!({
            "threadId": "th-fake",
            "model": "gpt-6-astra",
            "cwd": harness.project_dir.to_string_lossy(),
            "sandbox": "read-only",
            "approvalPolicy": "never",
            "excludeTurns": true,
        }))]
    );

    // Cancelling interrupts that turn over the protocol, and the store records the cancellation.
    assert_that!(
        wait_for_newest_command(&harness.command_pids_path, 2).await,
        eq(true)
    );
    let cancelled = harness.store.cancel(&id).await?;
    assert_that!(cancelled.outcome, eq(&Outcome::Cancelled));
    assert_that!(
        client_messages_for_method(&harness.client_messages_path, "turn/interrupt").len(),
        eq(1)
    );
    let (_, page) = harness.store.view(&id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("_Turn 2 cancelled."));
    let cancelled_turn = harness
        .store
        .wait_for_settlement(&harness.store.pin_turn(&id, Some(2))?)
        .await?;
    assert_that!(cancelled_turn.outcome(), eq(&Outcome::Cancelled));
    // And so did the one the cancelled turn left running, in a session no signal to the driver
    // would have reached.
    assert_that!(
        wait_for_commands_to_exit(&harness.command_pids_path, 2).await,
        eq(true)
    );
    Ok(())
}

/// Record `driver` as the child of the turn in `turn`, whole, as the store does once the spawn
/// returns: a driver whose turn's launch record does not name it starts nothing.
fn record_launch(turn: &Path, driver: u32) -> Result<()> {
    let record = serde_json::json!({
        "launched": agentmux::launch::Launched {
            pid: driver,
            process_group: Some(driver),
            started: None,
        },
        "started_at": "2026-10-19T12:00:00Z",
    });
    let written = turn.join("launch.json.tmp");
    std::fs::write(&written, record.to_string()).or_fail()?;
    std::fs::rename(&written, turn.join("launch.json")).or_fail()
}

/// A driver whose turn's launch record names another process leaves without starting codex, and
/// says why.
///
/// That is the turn a follow-up took over after the caller that spawned this driver died before
/// recording it, and whose mailbox and closing message are the new turn's.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_whose_launch_record_names_another_process_starts_nothing() -> Result<()> {
    use agentmux::appserver::DriverPlan;
    use agentmux::delegate::{AppServerSettings, Billing};

    let harness = cli_harness()?;
    let turn = harness.state_dir.join("taken-over");
    std::fs::create_dir_all(&turn).or_fail()?;
    let plan = DriverPlan {
        settings: AppServerSettings {
            program: "codex".to_owned(),
            args: vec!["app-server".to_owned()],
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            sandbox: CodexSandbox::ReadOnly,
        },
        cwd: harness.project_dir.clone(),
        resume_thread_id: None,
        billing: Billing::Subscription,
        turn_dir: turn.clone(),
    };
    let plan_path = turn.join("app-server.json");
    std::fs::write(&plan_path, serde_json::to_string(&plan).or_fail()?).or_fail()?;
    // The test process stands in for the driver the follow-up launched.
    record_launch(&turn, std::process::id())?;

    let mut driver = std::process::Command::new(env!("CARGO_BIN_EXE_agentmux"))
        .args(["codex-app-server", &plan_path.to_string_lossy()])
        .env_clear()
        .envs(&harness.host_env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .or_fail()?;
    let status = wait_for_driver_exit(&mut driver).await?;
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut driver.stderr.take().or_fail()?, &mut stderr).or_fail()?;

    assert_that!(status.code(), some(eq(2)));
    assert_that!(stderr, contains_substring("not as this driver"));
    // The fake writes its environment as soon as it starts, so no file means no server.
    assert_that!(harness.server_env_path.exists(), eq(false));
    Ok(())
}

/// The driver scrubs the server's environment itself, so a key reaches `codex app-server` only
/// on API pricing, whatever environment the driver was started with.
///
/// The driver is run directly here, with a dirty environment no launcher cleaned, and a turn that
/// is already cancelled, so the server is started and stopped without any turn.
#[gtest]
fn the_driver_hands_the_server_a_key_only_on_api_pricing() -> Result<()> {
    use agentmux::appserver::DriverPlan;
    use agentmux::delegate::{AppServerSettings, Billing};

    let harness = cli_harness()?;
    for (billing, expect_key) in [(Billing::Subscription, false), (Billing::Api, true)] {
        let turn = harness.state_dir.join(format!("direct-{billing}"));
        std::fs::create_dir_all(&turn).or_fail()?;
        let plan = DriverPlan {
            settings: AppServerSettings {
                program: "codex".to_owned(),
                args: vec!["app-server".to_owned()],
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                sandbox: CodexSandbox::ReadOnly,
            },
            cwd: harness.project_dir.clone(),
            resume_thread_id: None,
            billing,
            turn_dir: turn.clone(),
        };
        // Cancelled before it starts: the server is spawned, and no turn is begun.
        std::fs::write(plan.cancel_marker_path(), "").or_fail()?;
        let plan_path = turn.join("app-server.json");
        std::fs::write(&plan_path, serde_json::to_string(&plan).or_fail()?).or_fail()?;

        let mut driver = std::process::Command::new(env!("CARGO_BIN_EXE_agentmux"))
            .args(["codex-app-server", &plan_path.to_string_lossy()])
            .env_clear()
            .envs(&harness.host_env)
            .env("ANTHROPIC_AUTH_TOKEN", "tok-direct")
            .stdin(std::process::Stdio::null())
            .spawn()
            .or_fail()?;
        record_launch(&turn, driver.id())?;
        let status = driver.wait().or_fail()?;
        assert_that!(status.success(), eq(true), "{billing}");

        let seen = std::fs::read_to_string(&harness.server_env_path).or_fail()?;
        for key in ["sk-must-not-arrive", "tok-direct"] {
            assert_that!(seen.contains(key), eq(expect_key), "{billing}: {key}");
        }
    }
    Ok(())
}

/// A driver a test started, which leaves nothing running however the test ends.
///
/// On drop the fake's barriers are released, so the turn ends and the driver stops what the fake
/// started, as a real one would; a driver that does not leave in time is killed, and so is any
/// command it recorded that is still running, for a driver whose cleanup is what broke.
struct DriverProcess {
    child: std::process::Child,
    release_marker_paths: Vec<PathBuf>,
    command_pids_path: PathBuf,
}

impl Drop for DriverProcess {
    fn drop(&mut self) {
        for release in &self.release_marker_paths {
            std::fs::write(release, "").ok();
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        self.child.kill().ok();
        self.child.wait().ok();
        // Only a process that is still one the fake started: a pid that has since passed to
        // another process is left alone.
        for pid in read_command_pids(&self.command_pids_path) {
            if process_snapshot(pid)
                .is_some_and(|process| process.running && process.command_line.contains(HELPER))
            {
                let mut system = sysinfo::System::new();
                let pid = sysinfo::Pid::from_u32(pid);
                system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
                if let Some(process) = system.process(pid) {
                    process.kill();
                }
            }
        }
    }
}

/// Run the `codex-app-server` driver directly on a plan for a fresh turn in `dir`, against the
/// fake playing `scenario`, which records its commands' pids in the returned file.
///
/// `releases` are the files the scenario waits on, which dropping the driver creates.
fn drive_fake(
    harness: &CliTestHarness,
    dir: &Path,
    scenario: Scenario,
    release_marker_paths: Vec<PathBuf>,
) -> Result<DriverProcess> {
    use agentmux::appserver::DriverPlan;
    use agentmux::delegate::{AppServerSettings, Billing};

    let codex_home = dir.join("codex-home");
    let command_pids_path = dir.join("commands.txt");
    configure(
        &codex_home,
        &Config {
            scenario,
            client_messages_path: None,
            environment_dump_path: None,
            command_pids_path: command_pids_path.clone(),
        },
    )?;
    let plan = DriverPlan {
        settings: AppServerSettings {
            program: "codex".to_owned(),
            args: vec!["app-server".to_owned()],
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            sandbox: CodexSandbox::ReadOnly,
        },
        cwd: harness.project_dir.clone(),
        resume_thread_id: None,
        billing: Billing::Subscription,
        turn_dir: dir.to_path_buf(),
    };
    let plan_path = dir.join("app-server.json");
    std::fs::write(&plan_path, serde_json::to_string(&plan).or_fail()?).or_fail()?;

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_agentmux"))
        .args(["codex-app-server", &plan_path.to_string_lossy()])
        .env_clear()
        .envs(&harness.host_env)
        .env("CODEX_HOME", &codex_home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .or_fail()?;
    record_launch(dir, child.id())?;
    Ok(DriverProcess {
        child,
        release_marker_paths,
        command_pids_path,
    })
}

/// Wait for `driver` to exit, for at most a minute, killing it rather than hanging the test.
async fn wait_for_driver_exit(
    driver: &mut std::process::Child,
) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = driver.try_wait().or_fail()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            driver.kill().ok();
            driver.wait().ok();
            fail!("the driver did not exit within a minute")?;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A server that crashes, or exits while a command it started holds its output open — silently or
/// writing to it without pause — ends the turn, and the command it left is stopped even though the
/// server is no longer there to parent it.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_dies_mid_turn_leaves_nothing_running() -> Result<()> {
    let harness = cli_harness()?;
    for exit_mode in [
        ServerExitMode::Crash,
        ServerExitMode::LeaveOutputOpen,
        ServerExitMode::LeaveWarningWriter,
    ] {
        let dir = harness.state_dir.join(format!("dying-{exit_mode:?}"));
        std::fs::create_dir_all(&dir).or_fail()?;
        let release = dir.join("release");
        let mut driver = drive_fake(
            &harness,
            &dir,
            Scenario::ServerExit {
                exit_mode,
                exit_trigger_path: release.clone(),
            },
            vec![release.clone()],
        )?;

        assert_that!(
            wait_for_newest_command(&driver.command_pids_path, 1).await,
            eq(true),
            "{exit_mode:?}"
        );
        // Three of the driver's once-a-second records, so it has seen the command, which is what
        // reaches it where the driver cannot adopt the server's orphans.
        tokio::time::sleep(Duration::from_secs(3)).await;
        std::fs::write(&release, "").or_fail()?;

        // The turn broke, and said so in its exit status.
        let status = wait_for_driver_exit(&mut driver.child).await?;
        assert_that!(status.code(), some(eq(1)), "{exit_mode:?}");
        assert_that!(
            wait_for_commands_to_exit(&driver.command_pids_path, 1).await,
            eq(true),
            "{exit_mode:?}"
        );
    }
    Ok(())
}

/// A helper a turn's command forks and abandons is adopted by the driver and waited for as soon as
/// it exits, while the turn still runs, rather than left in the process table until the turn ends.
#[cfg(target_os = "linux")]
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_reaps_adopted_children_before_the_turn_ends() -> Result<()> {
    let harness = cli_harness()?;
    let dir = harness.state_dir.join("forking");
    std::fs::create_dir_all(&dir).or_fail()?;
    let release = dir.join("release");
    let helper_release = dir.join("release-helper");
    let mut driver = drive_fake(
        &harness,
        &dir,
        Scenario::OrphanedHelper {
            completion_trigger_path: release.clone(),
            helper_exit_trigger_path: helper_release.clone(),
        },
        vec![helper_release.clone(), release.clone()],
    )?;

    assert_that!(
        wait_for_newest_command(&driver.command_pids_path, 1).await,
        eq(true)
    );
    let helper = read_command_pids(&driver.command_pids_path)
        .pop()
        .or_fail()?;
    // The helper's parent goes, and the driver takes its place.
    let driver_pid = driver.child.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    while parent_pid(helper) != Some(driver_pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_that!(parent_pid(helper), some(eq(driver_pid)));
    // It exits, and is waited for, while the turn still runs.
    std::fs::write(&helper_release, "").or_fail()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_snapshot(helper).is_some() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_that!(process_snapshot(helper).is_some(), eq(false));
    assert_that!(driver.child.try_wait().or_fail()?, none());

    std::fs::write(&release, "").or_fail()?;
    let status = wait_for_driver_exit(&mut driver.child).await?;
    assert_that!(status.code(), some(eq(0)));
    Ok(())
}
