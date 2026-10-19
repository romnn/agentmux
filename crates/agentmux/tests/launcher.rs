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

use agentmux::delegate::{Billing, Invocation};
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
        let env: BTreeMap<String, String> =
            std::env::vars().filter(|(key, _)| key == "PATH").collect();
        self.launch_with(script, env, Billing::Subscription)
    }

    /// Run `script` with exactly `env` and on `billing`, capturing to files.
    fn launch_with(
        &self,
        script: &str,
        env: BTreeMap<String, String>,
        billing: Billing,
    ) -> Result<Launched> {
        let question = self.dir.path().join("question.md");
        std::fs::write(&question, "q").or_fail()?;
        let invocation = Invocation::for_launcher_test(
            "sh",
            vec!["-c".to_owned(), script.to_owned()],
            env,
            billing,
        );
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
    let fixture = Fixture::new()?;
    let launched = fixture.launch("exit 3")?;
    assert_that!(launched.started, some(anything()));

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = fixture.launcher.reap(launched) {
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
    assert_that!(fixture.launcher.liveness(launched), eq(Liveness::Gone));
    // A second reap reports the same status: two callers folding one turn must agree.
    assert_that!(
        fixture.launcher.reap(launched).map(|s| s.code),
        some(some(eq(3)))
    );
    // The same pid with another identity is not this child, whatever the table says.
    let impostor = Launched {
        started: Some(1),
        ..launched
    };
    assert_that!(fixture.launcher.reap(impostor), none());
    Ok(())
}

/// A cooperative child stops when asked.
#[gtest]
fn a_child_that_honours_sigterm_is_gone_after_terminate() -> Result<()> {
    let fixture = Fixture::new()?;
    let launched = fixture.launch("sleep 30")?;
    assert_that!(fixture.launcher.liveness(launched), eq(Liveness::Alive));

    fixture.launcher.terminate(launched);
    assert_that!(
        fixture.gone_within(launched, Duration::from_secs(5)),
        eq(true),
        "the child ignored SIGTERM"
    );
    Ok(())
}

/// A child that ignores the request is stopped by the kill, and its whole group with it.
#[gtest]
fn a_child_that_ignores_sigterm_is_gone_after_kill() -> Result<()> {
    let fixture = Fixture::new()?;
    // Bounded: a regression in `kill` leaks a process for at most thirty seconds.
    let launched = fixture.launch("trap '' TERM; sleep 30 & wait")?;
    // Give the shell time to install its trap before the signal arrives.
    std::thread::sleep(Duration::from_millis(300));

    fixture.launcher.terminate(launched);
    assert_that!(
        fixture.gone_within(launched, Duration::from_millis(800)),
        eq(false),
        "SIGTERM was honoured, so this fixture no longer tests the escalation"
    );
    fixture.launcher.kill(launched);
    assert_that!(
        fixture.gone_within(launched, Duration::from_secs(5)),
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
    let fixture = Fixture::new()?;
    let launched = fixture.launch("sleep 30")?;
    let restarted = ProcessLauncher::new();
    // The same pid and group, but started at some other time: not our child.
    let impostor = Launched {
        started: Some(1),
        ..launched
    };
    assert_that!(restarted.liveness(impostor), eq(Liveness::Gone));
    assert_that!(restarted.liveness(launched), eq(Liveness::Alive));
    // With no start time on record, a process holding the pid cannot be told from the child:
    // neither alive, which would let it be signalled, nor gone, which would let its run go.
    let unidentified = Launched {
        started: None,
        ..launched
    };
    assert_that!(restarted.liveness(unidentified), eq(Liveness::Unknown));

    fixture.launcher.kill(launched);
    assert_that!(
        fixture.gone_within(launched, Duration::from_secs(5)),
        eq(true)
    );
    Ok(())
}

/// A working directory that is not there is reported as such, not as a missing CLI.
#[gtest]
fn a_missing_working_directory_is_named() -> Result<()> {
    let fixture = Fixture::new()?;
    let question = fixture.dir.path().join("question.md");
    std::fs::write(&question, "q").or_fail()?;
    let invocation =
        Invocation::for_launcher_test("sh", vec![], BTreeMap::new(), Billing::Subscription);
    let error = fixture
        .launcher
        .launch(&LaunchSpec {
            invocation: &invocation,
            cwd: &fixture.dir.path().join("nowhere"),
            question: &question,
            events: &fixture.dir.path().join("events.jsonl"),
            stderr: &fixture.dir.path().join("stderr.log"),
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
    let fixture = Fixture::new()?;
    let first = fixture.launch("exit 0")?;
    std::thread::sleep(Duration::from_millis(300));

    let second = fixture.launch("sleep 30")?;

    // Waited on: the pid no longer names even a zombie.
    let pid = nix::unistd::Pid::from_raw(i32::try_from(first.pid).or_fail()?);
    assert_that!(
        nix::sys::signal::kill(pid, None),
        err(eq(nix::errno::Errno::ESRCH))
    );
    // And its status was kept for whoever asks.
    assert_that!(
        fixture.launcher.reap(first).map(|status| status.code),
        some(some(eq(0)))
    );

    fixture.launcher.kill(second);
    assert_that!(
        fixture.gone_within(second, Duration::from_secs(5)),
        eq(true)
    );
    Ok(())
}

/// The spawn itself withholds every key from a subscription child, whatever reached the
/// invocation.
///
/// The invocation here is built by hand with the keys in it, which no path through
/// `Delegate::invocation` produces: this is the last line of the guarantee, so it is tested as if
/// every layer before it had failed.
/// The child is a fake delegate that prints its own environment into the capture.
#[gtest]
fn a_subscription_child_never_receives_a_key() -> Result<()> {
    let fixture = Fixture::new()?;
    let keys = [
        ("ANTHROPIC_API_KEY", "sk-ant-leak"),
        ("ANTHROPIC_AUTH_TOKEN", "tok-leak"),
        ("ANTHROPIC_BASE_URL", "https://gateway.example"),
        ("CLAUDE_CODE_USE_BEDROCK", "bedrock-switch-leak"),
        ("OPENAI_API_KEY", "sk-openai-leak"),
        ("OPENAI_BASE_URL", "https://proxy.example/v1"),
        ("CODEX_API_KEY", "sk-codex-leak"),
        ("AZURE_OPENAI_API_KEY", "az-leak"),
    ];
    let mut env: BTreeMap<String, String> =
        std::env::vars().filter(|(key, _)| key == "PATH").collect();
    env.extend(
        keys.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    env.insert("CODEX_HOME".to_owned(), "/home/dev/.codex".to_owned());

    let launched = fixture.launch_with("env", env, Billing::Subscription)?;
    assert_that!(
        fixture.gone_within(launched, Duration::from_secs(10)),
        eq(true)
    );
    let printed = std::fs::read_to_string(fixture.dir.path().join("events.jsonl")).or_fail()?;

    // Not one key, token, endpoint or provider switch reached the child.
    for (name, value) in keys {
        assert_that!(
            printed,
            not(contains_substring(name)),
            "{name} reached the child"
        );
        assert_that!(
            printed,
            not(contains_substring(value)),
            "{name}'s value reached the child"
        );
    }
    // What selects the login is not a key, and still arrives.
    assert_that!(printed, contains_substring("CODEX_HOME=/home/dev/.codex"));
    Ok(())
}

/// A child an account opted into API pricing keeps the keys its invocation carries.
#[gtest]
fn an_api_child_keeps_its_key() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut env: BTreeMap<String, String> =
        std::env::vars().filter(|(key, _)| key == "PATH").collect();
    env.insert("OPENAI_API_KEY".to_owned(), "sk-opted-in".to_owned());

    let launched = fixture.launch_with("env", env, Billing::Api)?;
    assert_that!(
        fixture.gone_within(launched, Duration::from_secs(10)),
        eq(true)
    );
    let printed = std::fs::read_to_string(fixture.dir.path().join("events.jsonl")).or_fail()?;

    assert_that!(printed, contains_substring("OPENAI_API_KEY=sk-opted-in"));
    Ok(())
}

/// A CLI's version is what it prints for `--version`, and a CLI that cannot say has none.
#[gtest]
fn a_cli_names_its_version_or_has_none() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = Fixture::new()?;
    let write_executable_script = |name: &str, script: &str| -> Result<String> {
        let path = fixture.dir.path().join(name);
        std::fs::write(&path, script).or_fail()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).or_fail()?;
        Ok(path.display().to_string())
    };
    let version = |program: String| {
        let env: BTreeMap<String, String> =
            std::env::vars().filter(|(key, _)| key == "PATH").collect();
        let invocation =
            Invocation::for_launcher_test(&program, Vec::new(), env, Billing::Subscription);
        fixture.launcher.version(&invocation, fixture.dir.path())
    };

    let version_cli_path = write_executable_script(
        "named",
        indoc::indoc! {r#"
            #!/bin/sh
            [ "$1" = --version ] && echo '2.1.270 (Claude Code)'
        "#},
    )?;
    let failing_cli_path = write_executable_script(
        "refusing",
        indoc::indoc! {"
            #!/bin/sh
            echo 'unknown option' >&2
            exit 2
        "},
    )?;

    // A wrapper that leaves a daemon behind holding its output, as a version manager's shim may.
    let daemon_wrapper_path = write_executable_script(
        "leaving",
        indoc::indoc! {"
            #!/bin/sh
            sleep 30 &
            echo '0.162.1'
        "},
    )?;

    // The printed line is kept verbatim, trimmed; a failing or missing CLI says nothing.
    assert_that!(version(version_cli_path), some(eq("2.1.270 (Claude Code)")));
    assert_that!(version(failing_cli_path), none());
    assert_that!(version("/no/such/cli".to_owned()), none());
    // What the CLI printed is read once it exits, whatever it left holding its output.
    let probe_started_at = std::time::Instant::now();
    assert_that!(version(daemon_wrapper_path), some(eq("0.162.1")));
    assert_that!(probe_started_at.elapsed(), lt(Duration::from_secs(5)));
    Ok(())
}
