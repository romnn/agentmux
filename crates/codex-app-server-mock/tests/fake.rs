//! The `codex` binary on its own, driven by a hand-played client: what it answers, refuses and
//! exits with is the server a client test relies on, so it is checked here rather than only
//! through a client's outcome.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use codex_app_server_mock::fixtures::{THREAD, TURN};
use codex_app_server_mock::scenario::{CONFIG_FILE, Config, Scenario};
use codex_app_server_mock::{Peer, PeerError};
use codex_app_server_protocol::envelope::{Message, RequestId, RpcError};
use codex_app_server_protocol::methods as m;
use codex_app_server_protocol::types::{
    InitializeParams, ThreadStartParams, TurnInterruptParams, TurnStartParams, TurnSteerParams,
};
use googletest::prelude::*;
use serde_json::{Value, json};

/// The fake, started as `codex app-server` with `scenario`, and the client's end of it.
struct Fake {
    child: Child,
    client: Peer,
    _home: tempfile::TempDir,
    commands: PathBuf,
}

impl Fake {
    /// The pids of the commands the fake started, once `count` of them have recorded themselves,
    /// which they do only once they run as their scenario says.
    fn recorded(&self, count: usize) -> Result<Vec<u32>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pids = recorded(&self.commands);
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
    fn exit_status(&mut self) -> Result<std::process::ExitStatus> {
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

/// The pids recorded in `commands` so far.
fn recorded(commands: &Path) -> Vec<u32> {
    std::fs::read_to_string(commands)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Stops the fake and every command it recorded, however the test ended.
impl Drop for Fake {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        let pids: Vec<sysinfo::Pid> = recorded(&self.commands)
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

fn fake(scenario: impl FnOnce(&Path) -> Scenario) -> Result<Fake> {
    let home = tempfile::tempdir().or_fail()?;
    let commands = home.path().join("commands.txt");
    let config = Config {
        scenario: scenario(home.path()),
        log: None,
        environment: None,
        commands: commands.clone(),
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
    Ok(Fake {
        child,
        client,
        _home: home,
        commands,
    })
}

/// Send request `params` under `id` and take the next message, which must answer it.
fn ask<M: codex_app_server_protocol::ClientRequest>(
    client: &mut Peer,
    id: i64,
    params: &M::Params,
) -> std::result::Result<std::result::Result<Value, RpcError>, PeerError> {
    client.send(&Message::request::<M>(RequestId::Integer(id), params)?)?;
    client.expect_raw_answer(&RequestId::Integer(id))
}

/// The handshake and the thread, as a client opens them, then the turn.
fn open_and_start(client: &mut Peer) -> Result<()> {
    let initialize: InitializeParams = serde_json::from_value(json!({
        "clientInfo": {"name": "test", "version": "0"},
    }))
    .or_fail()?;
    ask::<m::Initialize>(client, 1, &initialize)
        .or_fail()?
        .or_fail()?;
    client
        .send(&Message::notification::<m::Initialized>(&()).or_fail()?)
        .or_fail()?;
    let thread: ThreadStartParams = serde_json::from_value(json!({})).or_fail()?;
    ask::<m::ThreadStart>(client, 2, &thread)
        .or_fail()?
        .or_fail()?;
    let turn: TurnStartParams = serde_json::from_value(json!({
        "threadId": THREAD,
        "input": [{"type": "text", "text": "q"}],
    }))
    .or_fail()?;
    ask::<m::TurnStart>(client, 3, &turn).or_fail()?.or_fail()?;
    Ok(())
}

fn steer(turn: &str) -> Result<TurnSteerParams> {
    serde_json::from_value(json!({
        "threadId": THREAD,
        "expectedTurnId": turn,
        "input": [{"type": "text", "text": "only auth"}],
    }))
    .or_fail()
}

/// The next notification's method.
fn notified(client: &mut Peer) -> Result<String> {
    match client.recv().or_fail()? {
        Message::Notification { method, .. } => Ok(method),
        other => fail!("not a notification: {}", other.to_line()).map(|()| String::new()),
    }
}

/// A steer completes the running turn once; a second one, and one naming another turn, are
/// refused, as codex refuses a steer for a turn that is not running.
#[gtest]
fn a_steerable_turn_takes_one_steer_and_refuses_the_rest() -> Result<()> {
    let mut fake = fake(|_| Scenario::Steerable)?;
    open_and_start(&mut fake.client)?;
    assert_that!(notified(&mut fake.client)?, eq("turn/started"));
    // The turn's command, which dropping the fake stops.
    assert_that!(fake.recorded(1)?.len(), eq(1));

    let wrong = ask::<m::TurnSteer>(&mut fake.client, 4, &steer("tu-other")?).or_fail()?;
    assert_that!(wrong, err(anything()));

    let steered = ask::<m::TurnSteer>(&mut fake.client, 5, &steer(TURN)?).or_fail()?;
    assert_that!(steered, ok(anything()));
    assert_that!(notified(&mut fake.client)?, eq("item/completed"));
    assert_that!(notified(&mut fake.client)?, eq("turn/completed"));

    let late = ask::<m::TurnSteer>(&mut fake.client, 6, &steer(TURN)?).or_fail()?;
    assert_that!(late, err(anything()));
    let interrupt = TurnInterruptParams {
        thread_id: THREAD.to_owned(),
        turn_id: TURN.to_owned(),
    };
    let late = ask::<m::TurnInterrupt>(&mut fake.client, 7, &interrupt).or_fail()?;
    assert_that!(late, err(anything()));
    Ok(())
}

/// A crashing server never answers `turn/start` and exits with a failure once released, having
/// recorded the command it started in a session of its own.
#[cfg(unix)]
#[gtest]
fn a_crashing_server_exits_with_a_failure() -> Result<()> {
    let mut fake = fake(|home| {
        let release = home.join("release");
        std::fs::write(&release, "").ok();
        Scenario::Dying {
            death: codex_app_server_mock::scenario::Death::Crash,
            release,
        }
    })?;
    let initialize: InitializeParams = serde_json::from_value(json!({
        "clientInfo": {"name": "test", "version": "0"},
    }))
    .or_fail()?;
    ask::<m::Initialize>(&mut fake.client, 1, &initialize)
        .or_fail()?
        .or_fail()?;
    fake.client
        .send(&Message::notification::<m::Initialized>(&()).or_fail()?)
        .or_fail()?;
    let thread: ThreadStartParams = serde_json::from_value(json!({})).or_fail()?;
    ask::<m::ThreadStart>(&mut fake.client, 2, &thread)
        .or_fail()?
        .or_fail()?;
    let turn: TurnStartParams = serde_json::from_value(json!({
        "threadId": THREAD,
        "input": [{"type": "text", "text": "q"}],
    }))
    .or_fail()?;
    fake.client
        .send(&Message::request::<m::TurnStart>(RequestId::Integer(3), &turn).or_fail()?)
        .or_fail()?;

    let status = fake.exit_status()?;
    assert_that!(status.code(), some(eq(1)));
    // The command recorded itself only once it led a session of its own; dropping the fake
    // stops it.
    let pid = fake.recorded(1)?.first().copied().or_fail()?;
    let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).or_fail()?);
    assert_that!(nix::unistd::getsid(Some(pid)).or_fail()?, eq(pid));
    Ok(())
}
