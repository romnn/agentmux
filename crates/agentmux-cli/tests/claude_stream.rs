//! Claude turns run end to end: the real `agentmux` executable as the supervisor and the driver,
//! against a fake `claude`.
//!
//! The fake is a shell script on the delegate's `PATH` that reads its question and answers as
//! Claude does, as each test scripts it.
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

/// A machine of its own: a `bin` for the fake `claude`, a home, a project and a store.
struct Machine {
    root: tempfile::TempDir,
    store: RunStore,
}

impl Machine {
    /// A machine whose `claude` runs `body` once it has read its question, and whose store
    /// resumes an overloaded turn as `auto_resume` says.
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
        .with_claude_driver(env!("CARGO_BIN_EXE_agentmux"))
        .with_auto_resume(auto_resume);
        Ok(Self { root, store })
    }

    /// A file under the machine's root.
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// Start a Claude consultation in the project.
    fn start(&self) -> Result<RunId> {
        let started = self.store.start(&StartRequest {
            delegate: Delegate::Claude {
                permissions: ClaudePermissions::default(),
                model: ModelId::parse("haiku").or_fail()?,
                effort: None,
                account: AccountChoice::Default,
                isolation: None,
            },
            question: "Review the repository.".to_owned(),
            cwd: self.path("project"),
            retention: Retention::Ttl,
            env: BTreeMap::new(),
            billing: None,
            force_codex_exec: false,
            label: None,
        })?;
        Ok(started.run_id)
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

/// A cancelled Claude turn's driver stops Claude and waits for it before it leaves, so `cancel`
/// returns with Claude gone, though Claude takes two seconds to honour its `SIGTERM`, and the
/// turn is recorded as cancelled.
/// A tool Claude started and left running in its process group is gone with it.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_claude_turn_is_gone_with_its_driver() -> Result<()> {
    // Apart from the machine, whose script names them.
    let pids = tempfile::tempdir().or_fail()?;
    let claude_pid_file = pids.path().join("claude.pid");
    let tool_pid_file = pids.path().join("tool.pid");
    let machine = Machine::new(
        &indoc::formatdoc! {r#"
            echo $$ > '{claude_pid}'
            sleep 120 &
            echo $! > '{tool_pid}'
            trap 'sleep 2; exit 143' TERM
            read -r question
            echo '{{"type":"system","subtype":"init","session_id":"{SESSION}"}}'
            while :; do sleep 1 & wait $!; done
            "#,
            claude_pid = claude_pid_file.display(),
            tool_pid = tool_pid_file.display(),
        },
        None,
    )?;
    let id = machine.start()?;

    // The turn takes input once Claude has announced its session, by which time Claude has
    // started its tool.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !machine.store.status(&id)?.steerable {
        assert_that!(Instant::now() < deadline, eq(true), "the turn never opened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let claude_pid = read_pid(&claude_pid_file)?;
    let tool_pid = read_pid(&tool_pid_file)?;
    assert_that!(is_running(claude_pid), eq(true));
    assert_that!(is_running(tool_pid), eq(true));

    // Claude and its tool are gone by the time `cancel` returns, and the turn is cancelled.
    let cancelled = machine.store.cancel(&id).await?;
    assert_that!(is_running(claude_pid), eq(false));
    assert_that!(is_running(tool_pid), eq(false));
    assert_that!(cancelled.outcome, eq(&Outcome::Cancelled));
    Ok(())
}

/// A Claude turn that fails overloaded is resumed by its supervisor over stream-json input too,
/// so the resumed turn can be steered like the first.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn an_automatic_resume_of_a_claude_turn_is_driven_too() -> Result<()> {
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
    let id = machine.start()?;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = machine.store.status(&id)?;
        if status.turns == 2 && status.outcome.is_terminal() {
            assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
            break;
        }
        assert_that!(
            Instant::now() < deadline,
            eq(true),
            "the turn was not resumed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let invocation: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            machine
                .path("state/runs")
                .join(id.to_string())
                .join("turns/0002/invocation.json"),
        )
        .or_fail()?,
    )
    .or_fail()?;
    assert_that!(
        invocation.get("mode"),
        some(eq(&serde_json::json!("claude_stream")))
    );
    Ok(())
}
