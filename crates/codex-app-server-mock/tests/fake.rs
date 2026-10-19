//! The `codex` binary on its own, driven by a hand-played client: what it answers, refuses and
//! exits with is the server a client test relies on, so it is checked here rather than only
//! through a client's outcome.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use codex_app_server_mock::fixtures::{THREAD_ID, TURN_ID};
use codex_app_server_mock::scenario::{CONFIG_FILE, Config, Scenario};
use codex_app_server_mock::{Peer, PeerError};
use codex_app_server_protocol::envelope::{Message, RequestId, RpcError};
use codex_app_server_protocol::methods;
use codex_app_server_protocol::types::{
    InitializeParams, ThreadStartParams, TurnInterruptParams, TurnStartParams, TurnSteerParams,
};
use googletest::prelude::*;
use serde_json::{Value, json};

/// The fake, started as `codex app-server` with `scenario`, and the client's end of it.
struct MockServerProcess {
    child: Child,
    client: Peer,
    _home: tempfile::TempDir,
    command_pids_path: PathBuf,
}

impl MockServerProcess {
    /// The pids of the commands the fake started, once `count` of them have recorded themselves,
    /// which they do only once they run as their scenario says.
    fn wait_for_command_pids(&self, count: usize) -> Result<Vec<u32>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pids = read_command_pids(&self.command_pids_path);
            if pids.len() >= count {
                return Ok(pids);
            }
            if Instant::now() >= deadline {
                return fail!("{count} commands did not record themselves").map(|()| pids);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The fake's exit status, waiting at most ten seconds for it.
    fn wait_for_exit(&mut self) -> Result<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().or_fail()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                fail!("the fake did not exit within ten seconds")?;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The pids recorded in `command_pids_path` so far.
fn read_command_pids(command_pids_path: &Path) -> Vec<u32> {
    std::fs::read_to_string(command_pids_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Stops the fake and every command it recorded, however the test ended.
impl Drop for MockServerProcess {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        let pids: Vec<sysinfo::Pid> = read_command_pids(&self.command_pids_path)
            .into_iter()
            .map(sysinfo::Pid::from_u32)
            .collect();
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
        for pid in pids {
            if let Some(process) = system.process(pid) {
                process.kill();
            }
        }
    }
}

fn spawn_mock_server(scenario: impl FnOnce(&Path) -> Scenario) -> Result<MockServerProcess> {
    let home = tempfile::tempdir().or_fail()?;
    let command_pids_path = home.path().join("commands.txt");
    let config = Config {
        scenario: scenario(home.path()),
        client_messages_path: None,
        environment_dump_path: None,
        command_pids_path: command_pids_path.clone(),
    };
    std::fs::write(
        home.path().join(CONFIG_FILE),
        serde_json::to_string(&config).or_fail()?,
    )
    .or_fail()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_codex"))
        .args(["app-server", "--listen", "stdio://"])
        .env("CODEX_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .or_fail()?;
    let stdin = child.stdin.take().or_fail()?;
    let stdout = child.stdout.take().or_fail()?;
    let client = Peer::new(BufReader::new(stdout), stdin).with_timeout(Duration::from_secs(10));
    Ok(MockServerProcess {
        child,
        client,
        _home: home,
        command_pids_path,
    })
}

/// Send request `params` under `id` and take the next message, which must answer it.
fn send_request_and_expect_response<
    M: codex_app_server_protocol::ClientRequest + codex_app_server_protocol::TypedParams,
>(
    client: &mut Peer,
    id: i64,
    params: &M::Params,
) -> std::result::Result<std::result::Result<Value, RpcError>, PeerError> {
    client.send(&Message::request::<M>(RequestId::Integer(id), params)?)?;
    client.expect_raw_response(&RequestId::Integer(id))
}

/// The handshake and the thread, as a client opens them, then the turn.
fn initialize_and_start_turn(client: &mut Peer) -> Result<()> {
    let initialize: InitializeParams = serde_json::from_value(json!({
        "clientInfo": {"name": "test", "version": "0"},
    }))
    .or_fail()?;
    send_request_and_expect_response::<methods::Initialize>(client, 1, &initialize)
        .or_fail()?
        .or_fail()?;
    client
        .send(&Message::notification::<methods::Initialized>(&()).or_fail()?)
        .or_fail()?;
    let thread: ThreadStartParams = serde_json::from_value(json!({})).or_fail()?;
    send_request_and_expect_response::<methods::ThreadStart>(client, 2, &thread)
        .or_fail()?
        .or_fail()?;
    let turn: TurnStartParams = serde_json::from_value(json!({
        "threadId": THREAD_ID,
        "input": [{"type": "text", "text": "q"}],
    }))
    .or_fail()?;
    send_request_and_expect_response::<methods::TurnStart>(client, 3, &turn)
        .or_fail()?
        .or_fail()?;
    Ok(())
}

fn steer_params(turn: &str) -> Result<TurnSteerParams> {
    serde_json::from_value(json!({
        "threadId": THREAD_ID,
        "expectedTurnId": turn,
        "input": [{"type": "text", "text": "only auth"}],
    }))
    .or_fail()
}

/// The next notification's method.
fn expect_notification_method(client: &mut Peer) -> Result<String> {
    match client.receive_message().or_fail()? {
        Message::Notification { method, .. } => Ok(method),
        other => fail!("not a notification: {}", other.to_line()).map(|()| String::new()),
    }
}

/// A steer completes the running turn once; a second one, and one naming another turn, are
/// refused, as codex refuses a steer for a turn that is not running.
#[gtest]
fn a_steerable_turn_takes_one_steer_and_refuses_the_rest() -> Result<()> {
    let mut fake = spawn_mock_server(|_| Scenario::Steerable)?;
    initialize_and_start_turn(&mut fake.client)?;
    assert_that!(
        expect_notification_method(&mut fake.client)?,
        eq("turn/started")
    );
    // The turn's command, which dropping the fake stops.
    assert_that!(fake.wait_for_command_pids(1)?.len(), eq(1));

    let wrong_turn_response = send_request_and_expect_response::<methods::TurnSteer>(
        &mut fake.client,
        4,
        &steer_params("tu-other")?,
    )
    .or_fail()?;
    assert_that!(wrong_turn_response, err(anything()));

    let steer_response = send_request_and_expect_response::<methods::TurnSteer>(
        &mut fake.client,
        5,
        &steer_params(TURN_ID)?,
    )
    .or_fail()?;
    assert_that!(steer_response, ok(anything()));
    assert_that!(
        expect_notification_method(&mut fake.client)?,
        eq("item/completed")
    );
    assert_that!(
        expect_notification_method(&mut fake.client)?,
        eq("turn/completed")
    );

    let late_steer_response = send_request_and_expect_response::<methods::TurnSteer>(
        &mut fake.client,
        6,
        &steer_params(TURN_ID)?,
    )
    .or_fail()?;
    assert_that!(late_steer_response, err(anything()));
    let interrupt = TurnInterruptParams {
        thread_id: THREAD_ID.to_owned(),
        turn_id: TURN_ID.to_owned(),
    };
    let late_interrupt_response =
        send_request_and_expect_response::<methods::TurnInterrupt>(&mut fake.client, 7, &interrupt)
            .or_fail()?;
    assert_that!(late_interrupt_response, err(anything()));
    Ok(())
}

/// A crashing server never answers `turn/start` and exits with a failure once released, having
/// recorded the command it started — in a session of its own, where the platform has sessions.
#[gtest]
fn a_crashing_server_exits_with_a_failure() -> Result<()> {
    let mut fake = spawn_mock_server(|home| {
        let exit_trigger_path = home.join("release");
        std::fs::write(&exit_trigger_path, "").ok();
        Scenario::ServerExit {
            exit_mode: codex_app_server_mock::scenario::ServerExitMode::Crash,
            exit_trigger_path,
        }
    })?;
    let initialize: InitializeParams = serde_json::from_value(json!({
        "clientInfo": {"name": "test", "version": "0"},
    }))
    .or_fail()?;
    send_request_and_expect_response::<methods::Initialize>(&mut fake.client, 1, &initialize)
        .or_fail()?
        .or_fail()?;
    fake.client
        .send(&Message::notification::<methods::Initialized>(&()).or_fail()?)
        .or_fail()?;
    let thread: ThreadStartParams = serde_json::from_value(json!({})).or_fail()?;
    send_request_and_expect_response::<methods::ThreadStart>(&mut fake.client, 2, &thread)
        .or_fail()?
        .or_fail()?;
    let turn: TurnStartParams = serde_json::from_value(json!({
        "threadId": THREAD_ID,
        "input": [{"type": "text", "text": "q"}],
    }))
    .or_fail()?;
    fake.client
        .send(&Message::request::<methods::TurnStart>(RequestId::Integer(3), &turn).or_fail()?)
        .or_fail()?;

    let status = fake.wait_for_exit()?;
    assert_that!(status.code(), some(eq(1)));
    // The command recorded itself only once it was set up; dropping the fake stops it.
    let recorded = fake.wait_for_command_pids(1)?;
    assert_that!(recorded.len(), eq(1));
    // On Unix, set up means leading a session of its own.
    #[cfg(unix)]
    {
        let pid = recorded.first().copied().or_fail()?;
        let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).or_fail()?);
        assert_that!(nix::unistd::getsid(Some(pid)).or_fail()?, eq(pid));
    }
    Ok(())
}
