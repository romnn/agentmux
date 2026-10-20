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

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use agentmux::testing::fixtures;
use googletest::prelude::*;

/// The fake delegate, built once per test process with the profile and target the tests run with.
fn fake_delegate() -> Result<PathBuf> {
    static BUILT: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
        std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            escargot::CargoBuild::new()
                .package("agentmux-cli")
                .example("fake-delegate")
                .current_release()
                .current_target()
                .run()
                .map(|built| built.path().to_path_buf())
                .map_err(|err| format!("cannot build the fake delegate: {err}"))
        })
        .clone()
        .or_fail()
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
        let home = self.root.path().join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentmux"));
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
    /// command has exited.
    fn start(&self) -> Result<String> {
        let output = self
            .agentmux(&[
                "start",
                "--json",
                "--delegate",
                "claude",
                "--model",
                "opus",
                "Review it.",
            ])
            .output()
            .or_fail()?;
        if !output.status.success() {
            fail!("start failed: {}", String::from_utf8_lossy(&output.stderr))?;
        }
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).or_fail()?;
        status
            .get("run_id")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .or_fail()
    }

    /// Start `agentmux wait` on the consultation, in the background.
    fn spawn_wait(&self, run_id: &str, extra: &[&str]) -> Result<Child> {
        let mut args = vec!["wait", run_id];
        args.extend_from_slice(extra);
        self.agentmux(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .or_fail()
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.path().join("state").join("runs").join(run_id)
    }

    fn marker(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id)
            .join("turns")
            .join("0000")
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

/// What a finished child printed on stdout.
fn stdout_of(child: &mut Child) -> Result<String> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().or_fail()?, &mut text).or_fail()?;
    Ok(text)
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
    assert_that!(settled.get("version"), some(eq(&serde_json::json!(1))));
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
        assert_that!(waiter.try_wait().or_fail()?, none(), "{expected}");

        let ended = Instant::now();
        if cancel {
            let cancelled = machine.agentmux(&["cancel", &run_id]).output().or_fail()?;
            assert_that!(cancelled.status.success(), eq(true));
        } else {
            machine.release()?;
        }
        let status = exit_within(&mut waiter, Duration::from_secs(20))?;
        assert_that!(status.code(), some(eq(expected)));
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
        let printed = stdout_of(&mut waiter)?;
        assert_that!(printed, contains_substring("turn 1 settled"), "{expected}");
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
    let mut waiter = machine.spawn_wait(&run_id, &["--timeout", "1s", "--json"])?;
    let status = exit_within(&mut waiter, Duration::from_secs(20))?;
    assert_that!(status.code(), some(eq(124)));
    let report: serde_json::Value = serde_json::from_str(&stdout_of(&mut waiter)?).or_fail()?;
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
    let mut waiter = machine.spawn_wait(&run_id, &[])?;
    let status = exit_within(&mut waiter, Duration::from_secs(20))?;
    assert_that!(status.code(), some(eq(10)));
    let transcript =
        std::fs::read_to_string(machine.run_dir(&run_id).join("transcript.md")).or_fail()?;
    assert_that!(transcript, contains_substring("never started"));
    Ok(())
}

/// The supervisor's pid, from the record its caller wrote.
fn supervisor_pid(machine: &Machine, run_id: &str) -> Result<u32> {
    let path = machine.run_dir(run_id).join("turns/0000/supervisor.json");
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
    let launch = machine.run_dir(&run_id).join("turns/0000/launch.json");
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
    let mut waiter = machine.spawn_wait(&run_id, &[])?;
    let status = exit_within(&mut waiter, Duration::from_secs(20))?;
    assert_that!(status.code(), some(eq(0)));
    assert_that!(machine.marker(&run_id).exists(), eq(true));
    Ok(())
}
