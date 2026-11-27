//! Claude turns whose supervisor holds their input, end to end: the real `agentmux` executable as
//! the supervisor, against a fake `claude`.
//!
//! The fake is a shell script on the delegate's `PATH` that reads its input and answers as Claude
//! does, as each test scripts it.
//! Unix only: the script is a shell script, and the signals it handles are Unix's.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentmux::delegate::{AccountChoice, ClaudePermissions, Delegate, ModelId};
use agentmux::launch::ProcessLauncher;
use agentmux::run::{AutoResumePolicy, Retention, RunId, RunStore, StartRequest};
use agentmux::transcript::Outcome;
use googletest::prelude::*;

/// The session every fake `claude` announces.
const SESSION: &str = "2f1c6e0a-7d4b-4e1a-9c3f-5b8a0d2e6f71";

/// A function of the shell script that echoes the input line held in `$1` as Claude echoes what it
/// takes: a `user` event with `isReplay`, the id the line was sent under, and its text, which
/// holds no quote.
/// Written with `printf`, because `echo` in some shells turns the text's escaped newlines into
/// line breaks.
const ECHO: &str = r#"echo_input() {
  id=$(printf '%s\n' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
  text=$(printf '%s\n' "$1" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p')
  printf '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"%s"}]},"parent_tool_use_id":null,"uuid":"%s","isReplay":true}\n' "$text" "$id"
}"#;

/// A function of the shell script that keeps it running, for a minute at most, so a fake whose
/// test failed before stopping it leaves on its own.
/// It sleeps in the background and waits, so a trapped signal is handled at once.
const LINGER: &str = r#"linger() {
  i=0
  while [ "$i" -lt 60 ]; do sleep 1 & wait $!; i=$((i + 1)); done
}"#;

/// A machine of its own: a `bin` for the fake `claude`, a home, a project and a store.
struct Machine {
    root: tempfile::TempDir,
    store: RunStore,
}

impl Machine {
    /// A machine whose `claude` runs `body`, and whose store resumes an overloaded turn as
    /// `auto_resume` says.
    fn new(body: &str, auto_resume: Option<AutoResumePolicy>) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().or_fail()?;
        let bin = root.path().join("bin");
        for dir in [
            &bin,
            &root.path().join("home"),
            &root.path().join("project"),
        ] {
            std::fs::create_dir_all(dir).or_fail()?;
        }
        let script = indoc::formatdoc! {r#"
            #!/bin/sh
            case "$1" in --version) echo "2.1.332 (Claude Code)"; exit 0;; esac
            {ECHO}
            {LINGER}
            {body}
            "#,
        };
        let claude = bin.join("claude");
        std::fs::write(&claude, script).or_fail()?;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).or_fail()?;

        let host_env: BTreeMap<String, String> = [
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("HOME", root.path().join("home").display().to_string()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();
        let store = RunStore::open(
            root.path().join("state"),
            Arc::new(ProcessLauncher::new()),
            host_env,
        )
        .or_fail()?
        .with_supervisor(env!("CARGO_BIN_EXE_agentmux"))
        .with_auto_resume(auto_resume);
        Ok(Self { root, store })
    }

    /// A file under the machine's root.
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// Start a Claude consultation in the project, asking `question`.
    fn start(&self, question: &str) -> Result<RunId> {
        let started = self.store.start(&StartRequest {
            delegate: Delegate::Claude {
                permissions: ClaudePermissions::default(),
                model: ModelId::parse("haiku").or_fail()?,
                effort: None,
                account: AccountChoice::Default,
                isolation: None,
            },
            question: question.to_owned(),
            cwd: self.path("project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })?;
        Ok(started.run_id)
    }

    /// The directory of turn `number` of consultation `id`.
    fn turn_dir(&self, id: &RunId, number: u32) -> PathBuf {
        self.path("state/runs")
            .join(id.to_string())
            .join(format!("turns/{number:04}"))
    }

    /// Poll the consultation until `ready` holds of its status, failing with `what` after twenty
    /// seconds.
    async fn until(
        &self,
        id: &RunId,
        what: &str,
        ready: impl Fn(&agentmux::run::RunStatus) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready(&self.store.status(id)?) {
            assert_that!(Instant::now() < deadline, eq(true), "{what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
}

/// The pid a fake wrote into `path`.
fn read_pid(path: &Path) -> Result<u32> {
    std::fs::read_to_string(path)
        .or_fail()?
        .trim()
        .parse()
        .or_fail()
}

/// Whether `pid` is a running process, rather than gone or exited and not yet waited for.
fn is_running(pid: u32) -> bool {
    let pid = sysinfo::Pid::from_u32(pid);
    let Ok(mut system) = sysinfo::System::new() else {
        return false;
    };
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some_and(|process| {
        !matches!(
            process.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        )
    })
}

/// A steer travels through the turn's mailbox to its supervisor, which writes it to Claude's
/// input, and is accepted once Claude's echo of it is in the capture.
/// The transcript shows it where Claude took it, between the two answers, and leaves out the
/// question's echo, which the question already shows; the input closes at the `result`, which is
/// what lets this Claude leave.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_claude_turn_is_steered_through_its_supervisor() -> Result<()> {
    let machine = Machine::new(
        &indoc::formatdoc! {r#"
            read -r question
            echo '{{"type":"system","subtype":"init","session_id":"{SESSION}"}}'
            echo_input "$question"
            echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Looking at it."}}]}},"parent_tool_use_id":null}}'
            read -r steer
            echo_input "$steer"
            echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Only auth, then."}}]}},"parent_tool_use_id":null}}'
            echo '{{"type":"result","subtype":"success","is_error":false,"result":"Only auth, then.","session_id":"{SESSION}","usage":{{"input_tokens":1,"output_tokens":1}}}}'
            while read -r more; do :; done
            "#,
        },
        None,
    )?;
    let id = machine.start("Review the repository.")?;
    machine
        .until(&id, "the turn never took input", |status| status.steerable)
        .await?;

    machine.store.steer(&id, "Narrow it to auth.").await?;
    machine
        .store
        .wait_until_terminal(&id, Duration::from_secs(20))
        .await?;

    let (status, page) = machine.store.view(&id, 0, usize::MAX)?;
    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(status.unrecognised.summary(), none());
    // The question once, then the first answer, the steer, and the answer to it.
    assert_that!(page.text.matches("Review the repository.").count(), eq(1));
    let first = page.text.find("Looking at it.").or_fail()?;
    let steer = page
        .text
        .find("### user (steer)\n\nNarrow it to auth.")
        .or_fail()?;
    let second = page.text.find("Only auth, then.").or_fail()?;
    assert_that!(first < steer && steer < second, eq(true));
    Ok(())
}

/// A Claude that never reads its input leaves the supervisor's write of a question larger than
/// any pipe holds unfinished, yet the supervisor still sees a cancellation the caller only
/// marked, as a caller that died before stopping anything leaves it, and carries it out.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_supervisor_writing_to_a_claude_that_does_not_read_still_cancels_it() -> Result<()> {
    let machine = Machine::new(
        &indoc::formatdoc! {r#"
            echo '{{"type":"system","subtype":"init","session_id":"{SESSION}"}}'
            linger
            "#,
        },
        None,
    )?;
    let id = machine.start(&"Review the repository. ".repeat(1 << 16))?;
    machine
        .until(&id, "the turn never took input", |status| status.steerable)
        .await?;

    std::fs::File::create(machine.turn_dir(&id, 1).join("cancelling")).or_fail()?;
    machine
        .until(&id, "the supervisor never cancelled the turn", |status| {
            status.outcome == Outcome::Cancelled && status.settled
        })
        .await?;
    Ok(())
}

/// A cancelled Claude turn is stopped as any delegate is, through its process group: `cancel`
/// returns with Claude gone, though it takes two seconds to honour its `SIGTERM`, and with it a
/// helper Claude started, which has left Claude's tree but not its group.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_claude_turn_is_gone_with_its_helpers() -> Result<()> {
    // Apart from the machine, whose script names them.
    let pids = tempfile::tempdir().or_fail()?;
    let claude_pid_file = pids.path().join("claude.pid");
    let tool_pid_file = pids.path().join("tool.pid");
    let machine = Machine::new(
        &indoc::formatdoc! {r#"
            echo $$ > '{claude_pid}'
            sh -c "sleep 60 & echo \$! > '{tool_pid}'"
            trap 'sleep 2; exit 143' TERM
            read -r question
            echo '{{"type":"system","subtype":"init","session_id":"{SESSION}"}}'
            linger
            "#,
            claude_pid = claude_pid_file.display(),
            tool_pid = tool_pid_file.display(),
        },
        None,
    )?;
    let id = machine.start("Review the repository.")?;
    machine
        .until(&id, "the turn never took input", |status| status.steerable)
        .await?;
    let claude_pid = read_pid(&claude_pid_file)?;
    let tool_pid = read_pid(&tool_pid_file)?;
    assert_that!(is_running(claude_pid), eq(true));
    assert_that!(is_running(tool_pid), eq(true));

    let cancelled = machine.store.cancel(&id).await?;
    assert_that!(is_running(claude_pid), eq(false));
    assert_that!(is_running(tool_pid), eq(false));
    assert_that!(cancelled.outcome, eq(&Outcome::Cancelled));
    Ok(())
}

/// A Claude turn that fails overloaded is resumed by its supervisor as a turn that reads its input
/// as stream-json too, so the resumed turn can be steered like the first.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn an_automatic_resume_of_a_claude_turn_reads_its_input_as_stream_json() -> Result<()> {
    let machine = Machine::new(
        &indoc::formatdoc! {r#"
            read -r question
            echo '{{"type":"system","subtype":"init","session_id":"{SESSION}"}}'
            echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Looking."}}]}},"parent_tool_use_id":null}}'
            case " $* " in
              *" --resume "*)
                echo '{{"type":"result","subtype":"success","is_error":false,"result":"RESUMED","session_id":"{SESSION}","usage":{{"input_tokens":1,"output_tokens":1}}}}'
                exit 0;;
            esac
            echo '{{"type":"result","subtype":"success","is_error":true,"api_error_status":529,"result":"Overloaded","session_id":"{SESSION}","usage":{{"input_tokens":0,"output_tokens":0}}}}'
            exit 1
            "#,
        },
        Some(AutoResumePolicy {
            first_delay: Duration::from_millis(200),
            longest_delay: Duration::from_millis(200),
            attempts: 1,
        }),
    )?;
    let id = machine.start("Review the repository.")?;

    machine
        .until(&id, "the turn was not resumed", |status| {
            status.turns == 2 && status.outcome.is_terminal()
        })
        .await?;
    assert_that!(
        machine.store.status(&id)?.outcome,
        matches_pattern!(Outcome::Completed { .. })
    );
    let invocation: serde_json::Value = serde_json::from_slice(
        &std::fs::read(machine.turn_dir(&id, 2).join("invocation.json")).or_fail()?,
    )
    .or_fail()?;
    assert_that!(
        invocation.get("mode"),
        some(eq(&serde_json::json!("claude_stream_input")))
    );
    Ok(())
}
