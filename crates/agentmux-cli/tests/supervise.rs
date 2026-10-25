//! Eager settlement and `agentmux wait`, end to end: the real `agentmux` executable as the
//! command, the supervisor and the waiter, against a fake delegate CLI.
//!
//! The fake is this package's `fake-delegate` example, built for the test run on first use and
//! copied into each test's own directory as `claude`, beside the configuration that says what it
//! prints, when it may leave and with which status.
//! Every command is a separate process, as it is for a caller: `agentmux start` exits at once, so
//! nothing but the supervisor is left to settle the turn, and these tests check the files
//! themselves rather than asking agentmux.
//! Nothing here is specific to one platform.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use agentmux::testing::fixtures;
use googletest::prelude::*;

/// An example of this package, built once per test process into `built` with the profile and
/// target the tests run with.
fn example(
    built: &std::sync::OnceLock<std::result::Result<PathBuf, String>>,
    name: &str,
) -> Result<PathBuf> {
    built
        .get_or_init(|| {
            escargot::CargoBuild::new()
                .package("agentmux-cli")
                .example(name)
                .current_release()
                .current_target()
                .run()
                .map(|built| built.path().to_path_buf())
                .map_err(|err| format!("cannot build the {name} example: {err}"))
        })
        .clone()
        .or_fail()
}

/// The fake delegate.
fn fake_delegate() -> Result<PathBuf> {
    static BUILT: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
        std::sync::OnceLock::new();
    example(&BUILT, "fake-delegate")
}

/// A caller that captures the command it runs, standing between a test and `agentmux`.
fn capture_wrapper() -> Result<PathBuf> {
    static BUILT: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
        std::sync::OnceLock::new();
    example(&BUILT, "capture-wrapper")
}

/// What the fake delegate does, as its configuration file spells it.
struct FakeRun<'a> {
    stdout: &'a str,
    stderr: &'a str,
    exit_code: u8,
}

/// One machine: a home, a project, a state directory, and a `PATH` holding only the fake.
struct Machine {
    root: tempfile::TempDir,
    bin: PathBuf,
    /// The file whose creation lets the fake leave.
    release: PathBuf,
}

/// The environment variables a Windows process needs before it can start at all, passed through
/// where the test process has them; elsewhere none is set.
const PASSED_THROUGH: &[&str] = &["SystemRoot", "SystemDrive", "windir", "TEMP", "TMP"];

/// A machine whose `claude` is the fake, playing `run`.
fn machine(run: &FakeRun<'_>) -> Result<Machine> {
    let root = tempfile::tempdir().or_fail()?;
    let bin = root.path().join("bin");
    for dir in [
        bin.clone(),
        root.path().join("home"),
        root.path().join("project"),
    ] {
        std::fs::create_dir_all(dir).or_fail()?;
    }
    let release = root.path().join("release");
    let fake = bin.join(format!("claude{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(fake_delegate()?, &fake).or_fail()?;
    let config = serde_json::json!({
        "stdout": run.stdout,
        "stderr": run.stderr,
        "release_path": release,
        "exit_code": run.exit_code,
    });
    std::fs::write(bin.join("fake-delegate.json"), config.to_string()).or_fail()?;
    Ok(Machine { root, bin, release })
}

impl Machine {
    /// `agentmux` with `args`, in the project, with this machine's environment and nothing else.
    fn agentmux(&self, args: &[&str]) -> Command {
        self.command(Path::new(env!("CARGO_BIN_EXE_agentmux")), args)
    }

    /// `program` with `args`, in the project, with this machine's environment and nothing else.
    fn command(&self, program: &Path, args: &[&str]) -> Command {
        let home = self.root.path().join("home");
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(self.root.path().join("project"))
            .env_clear()
            .env("PATH", &self.bin)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("AGENTMUX_STATE_DIR", self.root.path().join("state"))
            .stdin(Stdio::null());
        for name in PASSED_THROUGH {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    /// Start a consultation, as a caller's `agentmux start` does, and return its id once that
    /// command has exited and its output has ended.
    ///
    /// Its output is a pipe, as a host capturing it has: the supervisor and the delegate it
    /// starts outlive the command, and must hold none of that pipe.
    fn start(&self) -> Result<String> {
        started_run(&run_in_background(self.agentmux(START))?.finish(COMMAND_LIMIT)?)
    }

    /// Start `agentmux wait` on the consultation, in the background.
    fn spawn_wait(&self, run_id: &str, extra: &[&str]) -> Result<Running> {
        let mut args = vec!["wait", run_id];
        args.extend_from_slice(extra);
        run_in_background(self.agentmux(&args))
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.path().join("state").join("runs").join(run_id)
    }

    fn marker(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id)
            .join("turns")
            .join("0001")
            .join("settled.json")
    }

    fn release(&self) -> Result<()> {
        std::fs::write(&self.release, "").or_fail()
    }
}

impl Drop for Machine {
    /// Lets a fake still waiting leave, so no test leaves one behind; one that misses the file
    /// leaves once the directory holding it is removed.
    fn drop(&mut self) {
        std::fs::write(&self.release, "").ok();
    }
}

/// Wait up to `limit` for `path` to exist.
fn until_exists(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    path.exists()
}

/// The arguments of the `agentmux start` every test begins with.
const START: &[&str] = &[
    "start",
    "--json",
    "--delegate",
    "claude",
    "--model",
    "opus",
    "Review it.",
];

/// The id of the consultation `agentmux start` reported, once it succeeded.
fn started_run(output: &Finished) -> Result<String> {
    if !output.status.success() {
        fail!("start failed: {}", output.stderr)?;
    }
    let status: serde_json::Value = serde_json::from_str(&output.stdout).or_fail()?;
    status
        .get("run_id")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .or_fail()
}

/// The longest any one `agentmux` command these tests run may take, so a test that would hang
/// fails instead.
const COMMAND_LIMIT: Duration = Duration::from_secs(60);

/// An `agentmux` command running in the background, its output read as it is written, so a full
/// pipe never stalls it.
struct Running {
    child: Child,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
}

/// How a command ended, and everything it printed.
struct Finished {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// Start `command` with both outputs piped and read in the background.
///
/// This process's own standard handles are kept from it first, as agentmux keeps its own from
/// the processes it starts, so the runner capturing this test is not held open by a supervisor
/// that outlives it.
fn run_in_background(mut command: Command) -> Result<Running> {
    agentmux::testing::keep_std_handles_from_children();
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .or_fail()?;
    let stdout = read_in_background(child.stdout.take().or_fail()?);
    let stderr = read_in_background(child.stderr.take().or_fail()?);
    Ok(Running {
        child,
        stdout,
        stderr,
    })
}

/// Read `pipe` to its end on a thread of its own, and hand over what it held.
fn read_in_background(mut pipe: impl Read + Send + 'static) -> Receiver<String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        if pipe.read_to_string(&mut text).is_ok() {
            sender.send(text).ok();
        }
    });
    receiver
}

impl Running {
    /// Whether it is still running.
    fn is_running(&mut self) -> Result<bool> {
        Ok(self.child.try_wait().or_fail()?.is_none())
    }

    /// Wait up to `limit` for it to exit and for its output to end, failing rather than hanging.
    ///
    /// Output that stays open after the command has exited is held by a process it started,
    /// which is the leak of a caller's pipes into a detached supervisor this suite guards
    /// against.
    fn finish(mut self, limit: Duration) -> Result<Finished> {
        let deadline = Instant::now() + limit;
        let status = exit_within(&mut self.child, limit)?;
        // A moment at least, for the reader threads to see the end once the command has gone.
        let remaining = || {
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(2))
        };
        // Output that stays open after the command has exited is held by a process it started.
        let stdout = self.stdout.recv_timeout(remaining()).ok();
        let stderr = self.stderr.recv_timeout(remaining()).ok();
        let (stdout, stderr) = stdout.zip(stderr).or_fail()?;
        Ok(Finished {
            status,
            stdout,
            stderr,
        })
    }
}

/// Wait up to `limit` for `child` to exit, killing it rather than hanging the test.
fn exit_within(child: &mut Child, limit: Duration) -> Result<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().or_fail()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            fail!("the process did not exit within {limit:?}")?;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A delegate that leaves with nothing asking about its consultation leaves its turn settled on
/// disk: the marker, and the final transcript it vouches for, without a single agentmux call.
#[gtest]
fn a_turn_settles_with_nothing_asking() -> Result<()> {
    let machine = machine(&FakeRun {
        stdout: fixtures::CLAUDE_HAPPY,
        stderr: "",
        exit_code: 0,
    })?;
    let run_id = machine.start()?;
    let marker = machine.marker(&run_id);

    // Its stream has ended, but it has not left, so nothing is settled yet.
    std::thread::sleep(Duration::from_millis(500));
    assert_that!(marker.exists(), eq(false));

    machine.release()?;
    assert_that!(until_exists(&marker, Duration::from_secs(10)), eq(true));

    // The marker commits the transcript as it stands on disk.
    let settled: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker).or_fail()?).or_fail()?;
    let transcript =
        std::fs::read_to_string(machine.run_dir(&run_id).join("transcript.md")).or_fail()?;
    assert_that!(settled.get("version"), some(eq(&serde_json::json!(2))));
    // Numbered as its directory, `turns/0001`, is.
    assert_that!(settled.get("turn"), some(eq(&serde_json::json!(1))));
    assert_that!(
        settled.pointer("/outcome/state"),
        some(eq(&serde_json::json!("completed")))
    );
    assert_that!(
        settled.get("transcript_end"),
        some(eq(&serde_json::json!(transcript.len())))
    );
    assert_that!(transcript, contains_substring("FIXTURE_OK"));
    assert_that!(transcript, contains_substring("_Turn 1 completed"));
    Ok(())
}

/// A caller capturing a wrapper that itself captures `agentmux start` sees the end of the
/// wrapper's output once the wrapper exits, while the turn it started still runs.
///
/// On Windows the wrapper's captured output is inheritable in it, so a wrapper that did not keep
/// its standard handles from `agentmux` would pass them on beside the pipes it gives it, and on
/// into the supervisor and the delegate, past where agentmux can tell them apart.
/// Elsewhere a child's standard descriptors replace its parent's, and nothing can leak this way.
#[gtest]
fn a_caller_of_a_wrapper_that_captures_agentmux_sees_its_output_end() -> Result<()> {
    let machine = machine(&FakeRun {
        stdout: fixtures::CLAUDE_HAPPY,
        stderr: "",
        exit_code: 0,
    })?;
    let mut args = vec![env!("CARGO_BIN_EXE_agentmux")];
    args.extend_from_slice(START);
    let wrapper = run_in_background(machine.command(&capture_wrapper()?, &args))?;

    // The wrapper's output ended with the wrapper, before the delegate it led to was let go.
    let run_id = started_run(&wrapper.finish(COMMAND_LIMIT)?)?;
    let marker = machine.marker(&run_id);
    assert_that!(marker.exists(), eq(false));

    // The turn still settles as usual once the delegate leaves.
    machine.release()?;
    assert_that!(until_exists(&marker, Duration::from_secs(10)), eq(true));
    Ok(())
}

/// `agentmux wait` blocks while the turn runs, returns within a moment of its settling, and
/// exits with a status that says how it ended: 0 completed, 10 failed, 11 cancelled.
#[gtest]
fn agentmux_wait_returns_on_settlement_with_its_outcome() -> Result<()> {
    let cases = [
        // Completed
        (
            FakeRun {
                stdout: fixtures::CLAUDE_HAPPY,
                stderr: "",
                exit_code: 0,
            },
            false,
            0,
        ),
        // Failed: it left with no terminal event, explained on stderr
        (
            FakeRun {
                stdout: "",
                stderr: "boom: the model is unavailable",
                exit_code: 3,
            },
            false,
            10,
        ),
        // Cancelled while it ran
        (
            FakeRun {
                stdout: "",
                stderr: "",
                exit_code: 0,
            },
            true,
            11,
        ),
    ];
    for (run, cancel, expected) in cases {
        let machine = machine(&run)?;
        let run_id = machine.start()?;
        let mut waiter = machine.spawn_wait(&run_id, &[])?;

        // Still running, so still waiting.
        std::thread::sleep(Duration::from_millis(500));
        assert_that!(waiter.is_running()?, eq(true), "{expected}");

        let ended = Instant::now();
        if cancel {
            let cancelled =
                run_in_background(machine.agentmux(&["cancel", &run_id]))?.finish(COMMAND_LIMIT)?;
            assert_that!(cancelled.status.success(), eq(true));
        } else {
            machine.release()?;
        }
        let waited = waiter.finish(Duration::from_secs(20))?;
        assert_that!(waited.status.code(), some(eq(expected)));
        // Settled by the supervisor within a tenth of a second, seen within half of one; the
        // rest is slack for a loaded machine.
        if !cancel {
            assert_that!(
                ended.elapsed() < Duration::from_secs(3),
                eq(true),
                "returned {:?} after the delegate was let go",
                ended.elapsed()
            );
        }
        assert_that!(
            waited.stdout,
            contains_substring("turn 1 settled"),
            "{expected}"
        );
        assert_that!(machine.marker(&run_id).exists(), eq(true));
    }
    Ok(())
}

/// A wait that runs out of time exits 124 and says, under `--json`, that it timed out and what
/// it was still waiting for; the consultation carries on.
#[gtest]
fn a_wait_that_times_out_exits_124() -> Result<()> {
    let machine = machine(&FakeRun {
        stdout: fixtures::CLAUDE_HAPPY,
        stderr: "",
        exit_code: 0,
    })?;
    let run_id = machine.start()?;
    let waited = machine
        .spawn_wait(&run_id, &["--timeout", "1s", "--json"])?
        .finish(Duration::from_secs(20))?;
    assert_that!(waited.status.code(), some(eq(124)));
    let report: serde_json::Value = serde_json::from_str(&waited.stdout).or_fail()?;
    assert_that!(
        report.get("result"),
        some(eq(&serde_json::json!("timed_out")))
    );
    assert_that!(
        report.pointer("/pending/0/run_id"),
        some(eq(&serde_json::json!(run_id)))
    );
    assert_that!(machine.marker(&run_id).exists(), eq(false));

    // It carries on, and settles once its delegate goes.
    machine.release()?;
    assert_that!(
        until_exists(&machine.marker(&run_id), Duration::from_secs(10)),
        eq(true)
    );
    Ok(())
}

/// A delegate the supervisor cannot start is a failed turn kept with the reason, which a wait
/// returns as a failure.
#[gtest]
fn a_delegate_that_cannot_start_is_a_failed_turn() -> Result<()> {
    let machine = machine(&FakeRun {
        stdout: "",
        stderr: "",
        exit_code: 0,
    })?;
    std::fs::remove_file(
        machine
            .bin
            .join(format!("claude{}", std::env::consts::EXE_SUFFIX)),
    )
    .or_fail()?;
    let run_id = machine.start()?;
    let waited = machine
        .spawn_wait(&run_id, &[])?
        .finish(Duration::from_secs(20))?;
    assert_that!(waited.status.code(), some(eq(10)));
    let transcript =
        std::fs::read_to_string(machine.run_dir(&run_id).join("transcript.md")).or_fail()?;
    assert_that!(transcript, contains_substring("never started"));
    Ok(())
}

/// The supervisor's pid, from the record its caller wrote.
fn supervisor_pid(machine: &Machine, run_id: &str) -> Result<u32> {
    let path = machine.run_dir(run_id).join("turns/0001/supervisor.json");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).or_fail()?).or_fail()?;
    record
        .pointer("/launched/pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .or_fail()
}

/// Whether `pid` is a running process, rather than gone or exited and not yet waited for.
fn is_running(pid: u32) -> bool {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some_and(|process| {
        !matches!(
            process.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        )
    })
}

/// A supervisor that dies does not take its delegate with it, and does not mean the turn is
/// done: the delegate runs on, and once it leaves, the next observer — here a waiter — settles
/// the turn in the supervisor's place.
#[gtest]
fn a_dead_supervisor_leaves_its_turn_to_the_next_observer() -> Result<()> {
    let machine = machine(&FakeRun {
        stdout: fixtures::CLAUDE_HAPPY,
        stderr: "",
        exit_code: 0,
    })?;
    let run_id = machine.start()?;
    // Recorded by the supervisor once it has started the delegate.
    let launch = machine.run_dir(&run_id).join("turns/0001/launch.json");
    assert_that!(until_exists(&launch, Duration::from_secs(10)), eq(true));

    let supervisor = supervisor_pid(&machine, &run_id)?;
    let mut system = sysinfo::System::new();
    let pid = sysinfo::Pid::from_u32(supervisor);
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    assert_that!(
        system.process(pid).is_some_and(sysinfo::Process::kill),
        eq(true)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while is_running(supervisor) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_that!(is_running(supervisor), eq(false));

    // Nothing settles a turn whose delegate is still there.
    std::thread::sleep(Duration::from_millis(300));
    assert_that!(machine.marker(&run_id).exists(), eq(false));

    machine.release()?;
    let waited = machine
        .spawn_wait(&run_id, &[])?
        .finish(Duration::from_secs(20))?;
    assert_that!(waited.status.code(), some(eq(0)));
    assert_that!(machine.marker(&run_id).exists(), eq(true));
    Ok(())
}

/// The supervisor starts the delegate with exactly the argument vector the launch recorded, so a
/// Codex turn's permissions — its sandbox, network, unattended approval and the permission
/// profile named for that one launch — reach the CLI it runs, on the first turn and on a
/// follow-up, which names a profile of its own.
#[gtest]
fn the_supervisor_runs_the_permissions_the_launch_recorded() -> Result<()> {
    let machine = machine(&FakeRun {
        stdout: fixtures::CODEX_HAPPY,
        stderr: "",
        exit_code: 0,
    })?;
    // The fake answers `--version` with a release past the one permission profiles need.
    std::fs::copy(
        fake_delegate()?,
        machine
            .bin
            .join(format!("codex{}", std::env::consts::EXE_SUFFIX)),
    )
    .or_fail()?;
    let started = run_in_background(machine.agentmux(&[
        "--codex-launch",
        "exec",
        "start",
        "--json",
        "--delegate",
        "codex",
        "--model",
        "sol",
        "--network-access",
        "Review it.",
    ]))?
    .finish(COMMAND_LIMIT)?;
    let run_id = started_run(&started)?;
    machine.release()?;
    assert_that!(
        until_exists(&machine.marker(&run_id), Duration::from_secs(10)),
        eq(true)
    );

    // The follow-up resumes the session, its fake released already.
    let followed = run_in_background(machine.agentmux(&[
        "--codex-launch",
        "exec",
        "follow-up",
        &run_id,
        "--json",
        "--wait",
        "30",
        "And again.",
    ]))?
    .finish(COMMAND_LIMIT)?;
    if !followed.status.success() {
        fail!("follow-up failed: {}", followed.stderr)?;
    }

    // What each run of the fake was started with, in order.
    let received: Vec<Vec<String>> =
        std::fs::read_to_string(machine.bin.join("fake-delegate-argv.jsonl"))
            .or_fail()?
            .lines()
            .map(|line| serde_json::from_str(line).or_fail())
            .collect::<Result<_>>()?;
    let mut profiles = Vec::new();
    for (index, turn) in ["0001", "0002"].into_iter().enumerate() {
        let recorded: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                machine
                    .run_dir(&run_id)
                    .join("turns")
                    .join(turn)
                    .join("invocation.json"),
            )
            .or_fail()?,
        )
        .or_fail()?;
        let recorded: Vec<String> =
            serde_json::from_value(recorded.get("args").or_fail()?.clone()).or_fail()?;
        // The delegate received exactly what the launch recorded.
        assert_that!(received.get(index), some(eq(&recorded)), "turn {turn}");
        assert_that!(
            recorded,
            contains(eq("approval_policy=\"never\"")),
            "turn {turn}"
        );
        assert_that!(
            recorded,
            contains(eq("approvals_reviewer=\"user\"")),
            "turn {turn}"
        );
        assert_that!(recorded, not(contains(eq("--sandbox"))), "turn {turn}");
        let profile = recorded
            .iter()
            .find(|arg| arg.starts_with("default_permissions=\"agentmux_ro_"))
            .cloned()
            .or_fail()?;
        profiles.push(profile);
    }
    assert_that!(received.get(1), some(contains(eq("resume"))));
    // A resume names a profile of its own rather than reusing the first turn's.
    assert_that!(profiles.first(), not(eq(profiles.get(1))));
    Ok(())
}

/// A Codex turn that fails with the vendor's "at capacity" error, after saying something in a
/// session it announced.
const CODEX_OVERLOADED: &str = indoc::indoc! {r#"
    {"type":"thread.started","thread_id":"01a0784a-915b-7d92-a381-b53d765296c4"}
    {"type":"turn.started"}
    {"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Halfway through the review."}}
    {"type":"turn.failed","error":{"message":"Selected model is at capacity. Please try a different model."}}
"#};

/// A machine whose `codex` is the fake: its first run fails overloaded, every later one answers
/// in the same thread, and none waits for a release.
fn overloaded_codex_machine() -> Result<Machine> {
    let machine = machine(&FakeRun {
        stdout: "",
        stderr: "",
        exit_code: 0,
    })?;
    std::fs::copy(
        fake_delegate()?,
        machine
            .bin
            .join(format!("codex{}", std::env::consts::EXE_SUFFIX)),
    )
    .or_fail()?;
    let config = serde_json::json!({
        "stdout": CODEX_OVERLOADED,
        "exit_code": 1,
        "later": [{ "stdout": fixtures::CODEX_HAPPY, "exit_code": 0 }],
    });
    std::fs::write(machine.bin.join("fake-delegate.json"), config.to_string()).or_fail()?;
    Ok(machine)
}

/// Start a Codex consultation over `codex exec`, whose supervisor waits `delay` before resuming
/// it automatically.
fn start_codex(machine: &Machine, delay: &str) -> Result<String> {
    let started = run_in_background(machine.agentmux(&[
        "--codex-launch",
        "exec",
        "--auto-resume-delay",
        delay,
        "start",
        "--json",
        "--delegate",
        "codex",
        "--model",
        "sol",
        "Review it.",
    ]))?
    .finish(COMMAND_LIMIT)?;
    started_run(&started)
}

/// A JSON record agentmux wrote into the turn directory `turn` of a consultation.
fn turn_record(
    machine: &Machine,
    run_id: &str,
    turn: &str,
    name: &str,
) -> Result<serde_json::Value> {
    let path = machine.run_dir(run_id).join("turns").join(turn).join(name);
    serde_json::from_slice(&std::fs::read(path).or_fail()?).or_fail()
}

/// A turn that fails because the model is at capacity is resumed by its own supervisor, with
/// nothing asking: it waits, sends "continue" in the same session as the next turn, and that turn
/// completes.
/// A wait on the failed turn says so and names the turn to wait for next.
#[gtest]
fn an_overloaded_turn_is_resumed_automatically() -> Result<()> {
    let machine = overloaded_codex_machine()?;
    let run_id = start_codex(&machine, "200ms")?;

    // The failed turn is a failure to `wait`, which names the resume and the turn it starts.
    let first = machine
        .spawn_wait(&run_id, &["--turn", "1"])?
        .finish(Duration::from_secs(20))?;
    assert_that!(first.status.code(), some(eq(10)));
    assert_that!(first.stdout, contains_substring("model overloaded"));
    assert_that!(
        first.stdout,
        contains_substring("resumes this automatically")
            .or(contains_substring("resumed this automatically"))
    );
    assert_that!(first.stdout, contains_substring("--turn 2"));
    let decided = turn_record(&machine, &run_id, "0001", "auto-resume.json")?;
    assert_that!(
        decided.get("state"),
        some(eq(&serde_json::json!("scheduled")))
    );
    assert_that!(decided.get("attempt"), some(eq(&serde_json::json!(1))));

    // The supervisor started turn 2 itself, and it settles as completed.
    assert_that!(
        until_exists(
            &machine.run_dir(&run_id).join("turns/0002/settled.json"),
            Duration::from_secs(20)
        ),
        eq(true)
    );
    let second = machine
        .spawn_wait(&run_id, &["--turn", "2"])?
        .finish(Duration::from_secs(20))?;
    assert_that!(second.status.code(), some(eq(0)));
    assert_that!(second.stdout, contains_substring("FIXTURE_OK"));

    // Turn 2 is recorded as agentmux's own, asked "continue", resuming the same thread, and
    // the transcript says so under its question.
    let invocation = turn_record(&machine, &run_id, "0002", "invocation.json")?;
    assert_that!(
        invocation.get("auto_resume"),
        some(eq(&serde_json::json!({ "attempt": 1, "attempts": 12 })))
    );
    let transcript =
        std::fs::read_to_string(machine.run_dir(&run_id).join("transcript.md")).or_fail()?;
    assert_that!(
        transcript,
        contains_substring("agentmux sent this question itself")
    );
    assert_that!(transcript, contains_substring("automatic resume 1 of 12"));
    assert_that!(
        invocation.get("resumed_from"),
        some(eq(&serde_json::json!(
            "01a0784a-915b-7d92-a381-b53d765296c4"
        )))
    );
    let question = std::fs::read_to_string(machine.run_dir(&run_id).join("turns/0002/question.md"))
        .or_fail()?;
    assert_that!(question, eq("continue"));

    // The consultation reads as completed, with the overload as an earlier failure.
    let status = run_in_background(machine.agentmux(&["status", &run_id, "--json"]))?
        .finish(COMMAND_LIMIT)?;
    let status: serde_json::Value = serde_json::from_str(&status.stdout).or_fail()?;
    assert_that!(
        status.pointer("/outcome/state"),
        some(eq(&serde_json::json!("completed")))
    );
    assert_that!(
        status.pointer("/earlier_failures/0/kind"),
        some(eq(&serde_json::json!("overloaded")))
    );
    assert_that!(status.get("auto_resume"), none());

    // Damage to the later turn's records leaves a wait on the failed turn whole: it still
    // returns that turn, and says only that what followed cannot be read.
    std::fs::write(
        machine.run_dir(&run_id).join("turns/0002/invocation.json"),
        "{ not json",
    )
    .or_fail()?;
    let again = machine
        .spawn_wait(&run_id, &["--turn", "1", "--json"])?
        .finish(Duration::from_secs(20))?;
    assert_that!(again.status.code(), some(eq(10)));
    let again: serde_json::Value = serde_json::from_str(&again.stdout).or_fail()?;
    assert_that!(
        again.pointer("/settled/0/auto_resume/state"),
        some(eq(&serde_json::json!("unknown")))
    );
    assert_that!(
        again.pointer("/settled/0/auto_resume/turn"),
        some(eq(&serde_json::json!(2)))
    );
    Ok(())
}

/// `cancel` on a consultation whose resume is still pending stops the resume: the supervisor
/// leaves without starting another turn, and the failed turn says the resume was cancelled.
#[gtest]
fn cancel_stops_a_pending_automatic_resume() -> Result<()> {
    let machine = overloaded_codex_machine()?;
    let run_id = start_codex(&machine, "1h")?;
    let first = machine
        .spawn_wait(&run_id, &["--turn", "1"])?
        .finish(Duration::from_secs(20))?;
    assert_that!(first.status.code(), some(eq(10)));
    assert_that!(
        first.stdout,
        contains_substring("resumes this automatically")
    );

    // Every other reading of the consultation says so too: the status, `result` and `tail`
    // beside the transcript, and `tail --json`.
    for args in [["status", &run_id], ["result", &run_id], ["tail", &run_id]] {
        let read = run_in_background(machine.agentmux(&args))?.finish(COMMAND_LIMIT)?;
        assert_that!(
            format!("{}{}", read.stdout, read.stderr),
            contains_substring("resumes this automatically"),
            "{args:?}"
        );
    }
    let tailed =
        run_in_background(machine.agentmux(&["tail", &run_id, "--json"]))?.finish(COMMAND_LIMIT)?;
    let tailed: serde_json::Value = serde_json::from_str(&tailed.stdout).or_fail()?;
    assert_that!(
        tailed.pointer("/auto_resume/state"),
        some(eq(&serde_json::json!("scheduled")))
    );

    let cancelled = run_in_background(machine.agentmux(&["cancel", &run_id, "--json"]))?
        .finish(COMMAND_LIMIT)?;
    assert_that!(cancelled.status.success(), eq(true));
    let status: serde_json::Value = serde_json::from_str(&cancelled.stdout).or_fail()?;
    assert_that!(
        status.pointer("/auto_resume/state"),
        some(eq(&serde_json::json!("cancelled")))
    );
    assert_that!(
        status.pointer("/outcome/state"),
        some(eq(&serde_json::json!("failed")))
    );

    // The supervisor sees the cancellation within a second or so and leaves, having started
    // nothing.
    let supervisor = supervisor_pid(&machine, &run_id)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while is_running(supervisor) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_that!(is_running(supervisor), eq(false));
    assert_that!(
        machine.run_dir(&run_id).join("turns/0002").exists(),
        eq(false)
    );
    Ok(())
}
