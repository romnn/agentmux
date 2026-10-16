//! A steered consultation end to end: the real `agentmux` executable as the driver, against a
//! fake `codex` that speaks just enough of the app-server protocol.
//!
//! Everything between the caller and the server is real here — the `agentmux steer` command, the
//! detached launch, the driver's handshake, the mailbox, `turn/steer`, the follow-up's
//! `thread/resume`, and the cancellation that interrupts over the protocol — and nothing reaches
//! a vendor.
//! Unix only: the fake is a shell script.
//! What its turns leave running is checked with `ps`, and a command is put in a session of its
//! own with Perl's `setsid`, both of which Linux and macOS have.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentmux::delegate::{CodexSandbox, Delegate, Effort, Isolation, ModelId};
use agentmux::launch::ProcessLauncher;
use agentmux::run::{Retention, RunId, RunStore, StartRequest};
use agentmux::transcript::Outcome;
use googletest::prelude::*;

/// A fake `codex app-server`: answers each request by method, logs what it was sent, and records
/// its own environment.
///
/// A turn runs until it is steered, which completes it, or interrupted, which ends it as the real
/// server does.
/// Each turn also starts a command in a session of its own, as codex's sandbox does, and leaves it
/// running whatever happens to the turn, as codex-cli 0.159.3 was seen to do on an interrupt.
/// `@LOG@`, `@ENV@` and `@CHILDREN@` are replaced with paths in the test's directory.
const FAKE_CODEX: &str = indoc::indoc! {r#"
    #!/bin/sh
    [ "$1" = "app-server" ] || { echo "fake codex: only app-server is faked" >&2; exit 2; }
    env > '@ENV@'
    turn=tu-1
    while IFS= read -r line; do
      printf '%s\n' "$line" >> '@LOG@'
      id=$(printf '%s' "$line" | sed -n 's/.*"id" *: *\([0-9][0-9]*\).*/\1/p')
      method=$(printf '%s' "$line" | sed -n 's/.*"method" *: *"\([^"]*\)".*/\1/p')
      case "$method" in
        initialize)
          printf '{"id":%s,"result":{"codexHome":"/home/dev/.codex","platformFamily":"unix","platformOs":"linux","userAgent":"fake"}}\n' "$id" ;;
        thread/start|thread/resume)
          printf '{"id":%s,"result":{"approvalPolicy":"never","approvalsReviewer":"user","cwd":"/work","model":"gpt-6-astra","modelProvider":"openai","sandbox":{"type":"readOnly"},"thread":{"cliVersion":"0.159.3","createdAt":1,"cwd":"/work","ephemeral":false,"id":"th-fake","modelProvider":"openai","preview":"","projectId":null,"sessionId":"se-fake","source":"appServer","status":{"type":"idle"},"turns":[],"updatedAt":1}}}\n' "$id" ;;
        turn/start)
          perl -MPOSIX -e 'defined POSIX::setsid() or die "setsid: $!"; exec @ARGV' sleep 300 </dev/null >/dev/null 2>&1 &
          echo $! >> '@CHILDREN@'
          printf '{"id":%s,"result":{"turn":{"id":"%s","items":[],"status":"inProgress"}}}\n' "$id" "$turn"
          printf '{"method":"turn/started","params":{"threadId":"th-fake","turn":{"id":"%s","items":[],"status":"inProgress"}}}\n' "$turn" ;;
        turn/steer)
          printf '{"id":%s,"result":{"turnId":"%s"}}\n' "$id" "$turn"
          printf '{"method":"item/completed","params":{"threadId":"th-fake","turnId":"%s","completedAtMs":1,"item":{"id":"i1","type":"agentMessage","text":"STEERED: auth only"}}}\n' "$turn"
          printf '{"method":"turn/completed","params":{"threadId":"th-fake","turn":{"id":"%s","items":[],"status":"completed"}}}\n' "$turn" ;;
        turn/interrupt)
          printf '{"id":%s,"result":{}}\n' "$id"
          printf '{"method":"turn/completed","params":{"threadId":"th-fake","turn":{"id":"%s","items":[],"status":"interrupted"}}}\n' "$turn" ;;
      esac
    done
"#};

struct World {
    _root: tempfile::TempDir,
    store: RunStore,
    state: PathBuf,
    host: BTreeMap<String, String>,
    project: PathBuf,
    log: PathBuf,
    env_dump: PathBuf,
    /// Where the fake records the commands its turns started.
    children: PathBuf,
}

/// A store whose Codex turns are driven by the `agentmux` under test, with the fake on `PATH` and
/// keys for both vendors in the environment agentmux was started with.
fn world() -> Result<World> {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().or_fail()?;
    let bin = root.path().join("bin");
    let home = root.path().join("home");
    let project = root.path().join("project");
    for dir in [&bin, &home, &project] {
        std::fs::create_dir_all(dir).or_fail()?;
    }
    let log = root.path().join("requests.jsonl");
    let env_dump = root.path().join("codex-env.txt");
    let children = root.path().join("children.txt");
    let codex = bin.join("codex");
    let script = FAKE_CODEX
        .replace("@LOG@", &log.to_string_lossy())
        .replace("@ENV@", &env_dump.to_string_lossy())
        .replace("@CHILDREN@", &children.to_string_lossy());
    std::fs::write(&codex, script).or_fail()?;
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).or_fail()?;

    let host: BTreeMap<String, String> = [
        ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ("HOME", home.display().to_string()),
        ("OPENAI_API_KEY", "sk-must-not-arrive".to_owned()),
        ("CODEX_API_KEY", "sk-must-not-arrive-either".to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    let state = root.path().join("state");
    let store = RunStore::open(&state, Arc::new(ProcessLauncher::new()), host.clone())
        .or_fail()?
        .with_codex_app_server(env!("CARGO_BIN_EXE_agentmux"));
    Ok(World {
        _root: root,
        store,
        state,
        host,
        project,
        log,
        env_dump,
        children,
    })
}

impl World {
    /// Run `agentmux steer` on the consultation, as a caller in a shell would.
    async fn steer(&self, id: &str, message: &str) -> Result<std::process::Output> {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_agentmux"));
        command
            .args(["steer", id, message])
            .env_clear()
            .envs(&self.host)
            .env("AGENTMUX_STATE_DIR", &self.state);
        tokio::task::spawn_blocking(move || command.output())
            .await
            .or_fail()?
            .or_fail()
    }
}

/// The pids of the commands the fake's turns started, in the order they started.
fn commands(children: &Path) -> Vec<String> {
    std::fs::read_to_string(children)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// What `ps` says about `pid` in `column`, or `None` when there is no such process.
///
/// `ps` that cannot run, or fails for any other reason than finding no such process, is an
/// error, so a broken inspection can never read as a process that has gone.
fn ps(pid: &str, column: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("ps")
        .args(["-o", &format!("{column}="), "-p", pid])
        .output()
        .or_fail()?;
    let said = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    match (
        output.status.code(),
        said.is_empty(),
        output.stderr.is_empty(),
    ) {
        (Some(0), false, _) => Ok(Some(said)),
        // Both procps and BSD `ps` exit 1, printing nothing at all, when no process matches.
        (Some(1), true, true) => Ok(None),
        _ => fail!(
            "ps -p {pid} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .map(|()| None),
    }
}

/// Whether `pid` is a running process, rather than gone or a zombie nobody reaped yet.
fn is_running(pid: &str) -> Result<bool> {
    Ok(ps(pid, "stat")?.is_some_and(|stat| !stat.starts_with('Z')))
}

/// Whether the fake's turns have started exactly `count` commands and the newest is running,
/// waiting a few seconds for it.
async fn newest_command_is_running(children: &Path, count: usize) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let pids = commands(children);
        if pids.len() == count
            && let Some(pid) = pids.last()
            && is_running(pid)?
        {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(false)
}

/// Whether the fake's turns started exactly `count` commands and every one is gone, waiting a
/// few seconds for them.
async fn every_command_is_gone(children: &Path, count: usize) -> Result<bool> {
    let pids = commands(children);
    if pids.len() != count {
        return Ok(false);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let mut running = false;
        for pid in &pids {
            running |= is_running(pid)?;
        }
        if !running {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(false)
}

/// How many requests of `method` the fake has received.
fn received(log: &Path, method: &str) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|request| request.get("method").and_then(serde_json::Value::as_str) == Some(method))
        .count()
}

/// Wait for the fake to have received `count` requests of `method`.
async fn until_received(log: &Path, method: &str, count: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if received(log, method) >= count {
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

/// Start, steer, finish, follow up, and cancel one consultation, through the real driver.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_consultation_is_steered_followed_up_and_cancelled_over_the_app_server() -> Result<()> {
    let w = world()?;
    let started = w.store.start(&StartRequest {
        delegate: Delegate::Codex {
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            effort: Some(Effort::parse("high").or_fail()?),
            sandbox: CodexSandbox::ReadOnly,
            account: None,
            isolation: Some(Isolation::Inherit),
        },
        question: "Review the repository.".to_owned(),
        cwd: w.project.clone(),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        codex_exec: false,
    })?;
    let id = started.run_id.clone();
    assert_that!(until_steerable(&w.store, &id).await?, eq(true));
    assert_that!(newest_command_is_running(&w.children, 1).await?, eq(true));

    // Steer the running turn from the command line; the fake completes the turn when steered.
    let steered = w
        .steer(id.as_str(), "Only look at the auth module.")
        .await?;
    assert_that!(steered.status.success(), eq(true));
    assert_that!(
        String::from_utf8_lossy(&steered.stdout).into_owned(),
        contains_substring("steered: the delegate accepted the message")
    );
    w.store
        .wait_until_terminal(&id, Duration::from_secs(30))
        .await?;

    // A steer once the turn has ended is refused by the command, and reaches nothing.
    let late = w.steer(id.as_str(), "NEVER-DELIVERED").await?;
    assert_that!(late.status.success(), eq(false));
    assert_that!(
        String::from_utf8_lossy(&late.stderr).into_owned(),
        contains_substring("not running")
    );
    let (status, page) = w.store.view(&id, 0, 1_000_000)?;
    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(status.steerable, eq(false));
    assert_that!(status.resumable, eq(true));
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
    assert_that!(received(&w.log, "turn/steer"), eq(1));
    // The command the finished turn left running went with its server.
    assert_that!(every_command_is_gone(&w.children, 1).await?, eq(true));

    // The server was asked for no key, and was given none.
    let server_env = std::fs::read_to_string(&w.env_dump).or_fail()?;
    assert_that!(server_env, not(contains_substring("sk-must-not-arrive")));

    // A follow-up is a new turn on the same thread.
    let followed = w.store.follow_up(&id, "And the session code?")?;
    assert_that!(followed.turns, eq(2));
    assert_that!(until_received(&w.log, "turn/start", 2).await, eq(true));
    let resumed: Vec<serde_json::Value> = std::fs::read_to_string(&w.log)
        .or_fail()?
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|request| request.get("method") == Some(&serde_json::json!("thread/resume")))
        .filter_map(|request| request.get("params").cloned())
        .collect();
    assert_that!(
        resumed,
        elements_are![eq(&serde_json::json!({
            "threadId": "th-fake",
            "model": "gpt-6-astra",
            "cwd": w.project.to_string_lossy(),
            "sandbox": "read-only",
            "approvalPolicy": "never",
            "excludeTurns": true,
        }))]
    );

    // Cancelling interrupts that turn over the protocol, and the store records the cancellation.
    assert_that!(newest_command_is_running(&w.children, 2).await?, eq(true));
    let cancelled = w.store.cancel(&id).await?;
    assert_that!(cancelled.outcome, eq(&Outcome::Cancelled));
    assert_that!(received(&w.log, "turn/interrupt"), eq(1));
    let (_, page) = w.store.view(&id, 0, 1_000_000)?;
    assert_that!(page.text, contains_substring("_Turn 2 cancelled."));
    // And so did the one the cancelled turn left running, in a session no signal to the driver
    // would have reached.
    assert_that!(every_command_is_gone(&w.children, 2).await?, eq(true));
    Ok(())
}

/// The driver scrubs the server's environment itself, so a key reaches `codex app-server` only
/// on API pricing, whatever environment the driver was started with.
///
/// The driver is run directly here, with a dirty environment no launcher cleaned, and a turn that
/// is already cancelled, so the server is started and stopped without any turn.
#[gtest]
fn the_driver_hands_the_server_a_key_only_on_api_pricing() -> Result<()> {
    use agentmux::appserver::Plan;
    use agentmux::delegate::{AppServerSettings, Billing};

    let w = world()?;
    for (billing, expect_key) in [(Billing::Subscription, false), (Billing::Api, true)] {
        let turn = w.state.join(format!("direct-{billing}"));
        std::fs::create_dir_all(&turn).or_fail()?;
        let plan = Plan {
            settings: AppServerSettings {
                program: "codex".to_owned(),
                args: vec!["app-server".to_owned()],
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                sandbox: CodexSandbox::ReadOnly,
            },
            cwd: w.project.clone(),
            resume: None,
            billing,
            last_message: turn.join("last-message.md"),
            cancel_marker: turn.join("cancelling"),
            mailbox: turn.join("steer"),
        };
        // Cancelled before it starts: the server is spawned, and no turn is begun.
        std::fs::write(&plan.cancel_marker, "").or_fail()?;
        let plan_path = turn.join("app-server.json");
        std::fs::write(&plan_path, serde_json::to_string(&plan).or_fail()?).or_fail()?;

        let status = std::process::Command::new(env!("CARGO_BIN_EXE_agentmux"))
            .args(["codex-app-server", &plan_path.to_string_lossy()])
            .env_clear()
            .envs(&w.host)
            .env("ANTHROPIC_AUTH_TOKEN", "tok-direct")
            .stdin(std::process::Stdio::null())
            .status()
            .or_fail()?;
        assert_that!(status.success(), eq(true), "{billing}");

        let seen = std::fs::read_to_string(&w.env_dump).or_fail()?;
        for key in ["sk-must-not-arrive", "tok-direct"] {
            assert_that!(seen.contains(key), eq(expect_key), "{billing}: {key}");
        }
    }
    Ok(())
}

/// A fake `codex app-server` that answers setup, starts a command when the turn starts, waits
/// for `@RELEASE@` to exist, and then misbehaves as `@END@` says.
///
/// - `crash` never answers `turn/start`, and exits; its command runs in a session of its own.
/// - `linger` answers, and exits, leaving its command holding the server's output open.
/// - `chatter` answers, and exits, leaving a command that keeps writing valid notifications to
///   the server's output.
///
/// `@CHILDREN@` records the command.
const FAKE_DYING_CODEX: &str = indoc::indoc! {r#"
    #!/bin/sh
    while IFS= read -r line; do
      id=$(printf '%s' "$line" | sed -n 's/.*"id" *: *\([0-9][0-9]*\).*/\1/p')
      method=$(printf '%s' "$line" | sed -n 's/.*"method" *: *"\([^"]*\)".*/\1/p')
      case "$method" in
        initialize)
          printf '{"id":%s,"result":{"codexHome":"/home/dev/.codex","platformFamily":"unix","platformOs":"linux","userAgent":"fake"}}\n' "$id" ;;
        thread/start)
          printf '{"id":%s,"result":{"approvalPolicy":"never","approvalsReviewer":"user","cwd":"/work","model":"gpt-6-astra","modelProvider":"openai","sandbox":{"type":"readOnly"},"thread":{"cliVersion":"0.159.3","createdAt":1,"cwd":"/work","ephemeral":false,"id":"th-fake","modelProvider":"openai","preview":"","projectId":null,"sessionId":"se-fake","source":"appServer","status":{"type":"idle"},"turns":[],"updatedAt":1}}}\n' "$id" ;;
        turn/start)
          case '@END@' in
            crash)
              perl -MPOSIX -e 'defined POSIX::setsid() or die "setsid: $!"; exec @ARGV' sleep 300 </dev/null >/dev/null 2>&1 & ;;
            linger)
              printf '{"id":%s,"result":{"turn":{"id":"tu-1","items":[],"status":"inProgress"}}}\n' "$id"
              sleep 300 & ;;
            chatter)
              printf '{"id":%s,"result":{"turn":{"id":"tu-1","items":[],"status":"inProgress"}}}\n' "$id"
              while :; do
                printf '{"method":"warning","params":{"threadId":"th-fake","message":"still here"}}\n' || exit 0
                sleep 0.02
              done & ;;
          esac
          echo $! >> '@CHILDREN@'
          while [ ! -e '@RELEASE@' ]; do sleep 0.05; done
          [ '@END@' = crash ] && exit 1
          exit 0 ;;
      esac
    done
"#};

/// A driver a test started, which leaves nothing running however the test ends.
///
/// On drop the fake's barriers are released, so the turn ends and the driver stops what the fake
/// started, as a real one would; a driver that does not leave in time is killed, and so is any
/// command it recorded that is still running, for a driver whose cleanup is what broke.
struct Driven {
    child: std::process::Child,
    release: PathBuf,
    children: PathBuf,
}

impl Drop for Driven {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, "");
        let _ = std::fs::write(self.release.with_file_name("release-helper"), "");
        let deadline = Instant::now() + Duration::from_secs(30);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Only a process that is still one the fake started: a pid that has since passed to
        // another process, or that cannot be looked at, is left alone.
        let dir = self.children.parent().map(Path::to_string_lossy);
        for pid in commands(&self.children) {
            let ours = ps(&pid, "args").ok().flatten().is_some_and(|args| {
                args == "sleep 300" || dir.as_deref().is_some_and(|dir| args.contains(dir))
            });
            if ours && is_running(&pid).unwrap_or(false) {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", &pid])
                    .status();
            }
        }
    }
}

/// Run the `codex-app-server` driver directly on a plan for a fresh turn in `dir`, with `fake` as
/// the `codex` on its `PATH`, and return the driver with the paths the fake reads and writes.
fn drive_fake(w: &World, dir: &Path, fake: &str) -> Result<(Driven, PathBuf, PathBuf)> {
    use std::os::unix::fs::PermissionsExt as _;

    use agentmux::appserver::Plan;
    use agentmux::delegate::{AppServerSettings, Billing};

    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).or_fail()?;
    let children = dir.join("children.txt");
    let release = dir.join("release");
    let fake = fake
        .replace("@CHILDREN@", &children.to_string_lossy())
        .replace("@RELEASE@", &release.to_string_lossy());
    std::fs::write(bin.join("codex"), fake).or_fail()?;
    std::fs::set_permissions(bin.join("codex"), std::fs::Permissions::from_mode(0o755))
        .or_fail()?;
    let plan = Plan {
        settings: AppServerSettings {
            program: "codex".to_owned(),
            args: vec!["app-server".to_owned()],
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            sandbox: CodexSandbox::ReadOnly,
        },
        cwd: w.project.clone(),
        resume: None,
        billing: Billing::Subscription,
        last_message: dir.join("last-message.md"),
        cancel_marker: dir.join("cancelling"),
        mailbox: dir.join("steer"),
    };
    let plan_path = dir.join("app-server.json");
    std::fs::write(&plan_path, serde_json::to_string(&plan).or_fail()?).or_fail()?;

    let driver = std::process::Command::new(env!("CARGO_BIN_EXE_agentmux"))
        .args(["codex-app-server", &plan_path.to_string_lossy()])
        .env_clear()
        .envs(&w.host)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .or_fail()?;
    let driver = Driven {
        child: driver,
        release: release.clone(),
        children: children.clone(),
    };
    Ok((driver, children, release))
}

/// Wait for `driver` to exit, for at most a minute, killing it rather than hanging the test.
async fn exit_of(driver: &mut std::process::Child) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = driver.try_wait().or_fail()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = driver.kill();
            let _ = driver.wait();
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
    let w = world()?;
    for end in ["crash", "linger", "chatter"] {
        let dir = w.state.join(format!("dying-{end}"));
        let (mut driver, children, release) =
            drive_fake(&w, &dir, &FAKE_DYING_CODEX.replace("@END@", end))?;

        assert_that!(
            newest_command_is_running(&children, 1).await?,
            eq(true),
            "{end}"
        );
        // Three of the driver's once-a-second records, so it has seen the command, which is what
        // reaches it where the driver cannot adopt the server's orphans.
        tokio::time::sleep(Duration::from_secs(3)).await;
        std::fs::write(&release, "").or_fail()?;

        // The turn broke, and said so in its exit status.
        let status = exit_of(&mut driver.child).await?;
        assert_that!(status.code(), some(eq(1)), "{end}");
        assert_that!(
            every_command_is_gone(&children, 1).await?,
            eq(true),
            "{end}"
        );
    }
    Ok(())
}

/// A fake `codex app-server` whose turn forks a helper that outlives its parent, so the driver
/// adopts it; the helper exits once `@RELEASE@-helper` exists, and the turn completes once
/// `@RELEASE@` does.
#[cfg(target_os = "linux")]
const FAKE_FORKING_CODEX: &str = indoc::indoc! {r#"
    #!/bin/sh
    while IFS= read -r line; do
      id=$(printf '%s' "$line" | sed -n 's/.*"id" *: *\([0-9][0-9]*\).*/\1/p')
      method=$(printf '%s' "$line" | sed -n 's/.*"method" *: *"\([^"]*\)".*/\1/p')
      case "$method" in
        initialize)
          printf '{"id":%s,"result":{"codexHome":"/home/dev/.codex","platformFamily":"unix","platformOs":"linux","userAgent":"fake"}}\n' "$id" ;;
        thread/start)
          printf '{"id":%s,"result":{"approvalPolicy":"never","approvalsReviewer":"user","cwd":"/work","model":"gpt-6-astra","modelProvider":"openai","sandbox":{"type":"readOnly"},"thread":{"cliVersion":"0.159.3","createdAt":1,"cwd":"/work","ephemeral":false,"id":"th-fake","modelProvider":"openai","preview":"","projectId":null,"sessionId":"se-fake","source":"appServer","status":{"type":"idle"},"turns":[],"updatedAt":1}}}\n' "$id" ;;
        turn/start)
          printf '{"id":%s,"result":{"turn":{"id":"tu-1","items":[],"status":"inProgress"}}}\n' "$id"
          ( sh -c 'echo $$ >> "$0"; while [ ! -e "$1" ]; do sleep 0.05; done' '@CHILDREN@' '@RELEASE@-helper' </dev/null >/dev/null 2>&1 & )
          while [ ! -e '@RELEASE@' ]; do sleep 0.05; done
          printf '{"method":"turn/completed","params":{"threadId":"th-fake","turn":{"id":"tu-1","items":[],"status":"completed"}}}\n' ;;
      esac
    done
"#};

/// A helper a turn's command forks and abandons is adopted by the driver and waited for as soon as
/// it exits, while the turn still runs, rather than left in the process table until the turn ends.
#[cfg(target_os = "linux")]
#[gtest]
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_reaps_adopted_children_before_the_turn_ends() -> Result<()> {
    let w = world()?;
    let dir = w.state.join("forking");
    let (mut driver, children, release) = drive_fake(&w, &dir, FAKE_FORKING_CODEX)?;

    assert_that!(newest_command_is_running(&children, 1).await?, eq(true));
    let helper = commands(&children).pop().or_fail()?;
    // The helper's parent goes, and the driver takes its place.
    let driver_pid = driver.child.id().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    while ps(&helper, "ppid")?.as_deref() != Some(driver_pid.as_str()) && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_that!(ps(&helper, "ppid")?, some(eq(&driver_pid)));
    // It exits, and is waited for, while the turn still runs.
    std::fs::write(dir.join("release-helper"), "").or_fail()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while ps(&helper, "stat")?.is_some() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_that!(ps(&helper, "stat")?, none());
    assert_that!(driver.child.try_wait().or_fail()?, none());

    std::fs::write(&release, "").or_fail()?;
    let status = exit_of(&mut driver.child).await?;
    assert_that!(status.code(), some(eq(0)));
    Ok(())
}
