//! The real launcher, against real processes.
//!
//! Everything else in the crate runs through the scripted launcher; this is the one place the
//! claims `launch.rs` makes about detached children — that they can be found again, stopped,
//! and stopped harder — are exercised against an operating system.
//! Unix only: the fixtures are shell scripts, and every one of them bounds its own lifetime so a
//! failing test leaks nothing.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use agentmux::delegate::Invocation;
use agentmux::launch::{LaunchSpec, Launched, Launcher, Liveness, ProcessLauncher};
use googletest::prelude::*;

struct Fixture {
    dir: tempfile::TempDir,
    launcher: ProcessLauncher,
}

impl Fixture {
    fn new() -> Result<Self> {
        Ok(Self {
            dir: tempfile::tempdir().or_fail()?,
            launcher: ProcessLauncher::new(),
        })
    }

    /// Run `script` under `sh -c`, the way agentmux runs a delegate: detached, capture to files.
    fn launch(&self, script: &str) -> Result<Launched> {
        let question = self.dir.path().join("question.md");
        std::fs::write(&question, "q").or_fail()?;
        let env: BTreeMap<String, String> =
            std::env::vars().filter(|(key, _)| key == "PATH").collect();
        let invocation = Invocation {
            program: "sh",
            args: vec!["-c".to_owned(), script.to_owned()],
            env,
        };
        self.launcher
            .launch(&LaunchSpec {
                invocation: &invocation,
                cwd: self.dir.path(),
                question: &question,
                events: &self.dir.path().join("events.jsonl"),
                stderr: &self.dir.path().join("stderr.log"),
            })
            .or_fail()
    }

    /// Whether the child left within `grace`, reaping as agentmux does.
    fn gone_within(&self, launched: Launched, grace: Duration) -> bool {
        let deadline = Instant::now() + grace;
        loop {
            let _ = self.launcher.reap(launched);
            if self.launcher.liveness(launched) == Liveness::Gone {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A child that exits on its own is seen to have gone, with its status.
#[gtest]
fn a_finished_child_is_reaped_with_its_status() -> Result<()> {
    let f = Fixture::new()?;
    let launched = f.launch("exit 3")?;
    assert_that!(launched.started, some(anything()));

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = f.launcher.reap(launched) {
            break status;
        }
        assert_that!(
            Instant::now() < deadline,
            eq(true),
            "the child never exited"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_that!(status.code, some(eq(3)));
    assert_that!(f.launcher.liveness(launched), eq(Liveness::Gone));
    // A second reap reports the same status: two callers folding one turn must agree.
    assert_that!(f.launcher.reap(launched).map(|s| s.code), some(some(eq(3))));
    // The same pid with another identity is not this child, whatever the table says.
    let impostor = Launched {
        started: Some(1),
        ..launched
    };
    assert_that!(f.launcher.reap(impostor), none());
    Ok(())
}

/// A cooperative child stops when asked.
#[gtest]
fn a_child_that_honours_sigterm_is_gone_after_terminate() -> Result<()> {
    let f = Fixture::new()?;
    let launched = f.launch("sleep 30")?;
    assert_that!(f.launcher.liveness(launched), eq(Liveness::Alive));

    f.launcher.terminate(launched);
    assert_that!(
        f.gone_within(launched, Duration::from_secs(5)),
        eq(true),
        "the child ignored SIGTERM"
    );
    Ok(())
}

/// A child that ignores the request is stopped by the kill, and its whole group with it.
#[gtest]
fn a_child_that_ignores_sigterm_is_gone_after_kill() -> Result<()> {
    let f = Fixture::new()?;
    // Bounded: a regression in `kill` leaks a process for at most thirty seconds.
    let launched = f.launch("trap '' TERM; sleep 30 & wait")?;
    // Give the shell time to install its trap before the signal arrives.
    std::thread::sleep(Duration::from_millis(300));

    f.launcher.terminate(launched);
    assert_that!(
        f.gone_within(launched, Duration::from_millis(800)),
        eq(false),
        "SIGTERM was honoured, so this fixture no longer tests the escalation"
    );
    f.launcher.kill(launched);
    assert_that!(
        f.gone_within(launched, Duration::from_secs(5)),
        eq(true),
        "the child survived SIGKILL"
    );
    Ok(())
}

/// A pid that now belongs to another process is not the child.
///
/// After an agentmux restart only the record on disk identifies a delegate, and a pid is
/// recycled; the recorded start time is what tells the two apart.
/// A launcher that never started the child — the one a restarted agentmux has — is what asks,
/// because a launcher still holding the handle knows the answer without looking.
#[gtest]
fn a_recycled_pid_is_not_mistaken_for_the_child() -> Result<()> {
    let f = Fixture::new()?;
    let launched = f.launch("sleep 30")?;
    let restarted = ProcessLauncher::new();
    // The same pid and group, but started at some other time: not our child.
    let impostor = Launched {
        started: Some(1),
        ..launched
    };
    assert_that!(restarted.liveness(impostor), eq(Liveness::Gone));
    assert_that!(restarted.liveness(launched), eq(Liveness::Alive));

    f.launcher.kill(launched);
    assert_that!(f.gone_within(launched, Duration::from_secs(5)), eq(true));
    Ok(())
}

/// A working directory that is not there is reported as such, not as a missing CLI.
#[gtest]
fn a_missing_working_directory_is_named() -> Result<()> {
    let f = Fixture::new()?;
    let question = f.dir.path().join("question.md");
    std::fs::write(&question, "q").or_fail()?;
    let invocation = Invocation {
        program: "sh",
        args: vec![],
        env: BTreeMap::new(),
    };
    let error = f
        .launcher
        .launch(&LaunchSpec {
            invocation: &invocation,
            cwd: &f.dir.path().join("nowhere"),
            question: &question,
            events: &f.dir.path().join("events.jsonl"),
            stderr: &f.dir.path().join("stderr.log"),
        })
        .expect_err("the directory does not exist");
    assert_that!(error.to_string(), contains_substring("not a directory"));
    assert_that!(error.to_string(), not(contains_substring("installed")));
    Ok(())
}

/// A finished child is waited on when the next one is launched, so it never lingers as a zombie.
///
/// A consultation nobody reads again after its terminal event is never reaped by a fold; a
/// long-lived server would otherwise keep one defunct process per consultation.
#[gtest]
fn a_finished_child_is_collected_when_the_next_is_launched() -> Result<()> {
    let f = Fixture::new()?;
    let first = f.launch("exit 0")?;
    std::thread::sleep(Duration::from_millis(300));

    let second = f.launch("sleep 30")?;

    // Waited on: the pid no longer names even a zombie.
    let pid = nix::unistd::Pid::from_raw(i32::try_from(first.pid).or_fail()?);
    assert_that!(
        nix::sys::signal::kill(pid, None),
        err(eq(nix::errno::Errno::ESRCH))
    );
    // And its status was kept for whoever asks.
    assert_that!(
        f.launcher.reap(first).map(|status| status.code),
        some(some(eq(0)))
    );

    f.launcher.kill(second);
    assert_that!(f.gone_within(second, Duration::from_secs(5)), eq(true));
    Ok(())
}
