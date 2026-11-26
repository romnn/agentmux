//! Steered consultations against the real `codex app-server` and the real `claude`.
//!
//! The only tests of this crate that spend a turn and need a logged-in CLI, so they are
//! `#[ignore]`d and CI passes without them.
//! Run them by hand after upgrading either CLI, outside any sandbox, because the delegate saves
//! its session where the sandbox would not let it:
//!
//! ```text
//! AGENTMUX_LIVE_CODEX_ACCOUNT=personal AGENTMUX_LIVE_CLAUDE_ACCOUNT=personal \
//!   cargo nextest run -p agentmux-cli --run-ignored all -E 'test(live)' --no-capture
//! ```
//!
//! `AGENTMUX_LIVE_CODEX_ACCOUNT` and `AGENTMUX_LIVE_CLAUDE_ACCOUNT` name each vendor's account from
//! the machine's `agentmux.toml`; without one the file's default, or the CLI's own login, is
//! used.
//! The driver is the `agentmux` built with these tests, never an installed one.
//! The store is kept, and its path printed, so a failure's capture can be read afterwards.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentmux::delegate::{
    AccountAlias, ClaudePermissionMode, ClaudePermissions, ClaudeTool, CodexPermissions,
    CodexSandbox, Delegate, Effort, Isolation, ModelId,
};
use agentmux::launch::ProcessLauncher;
use agentmux::run::{Retention, RunId, RunStatus, RunStore, StartRequest};
use agentmux::transcript::Outcome;
use googletest::prelude::*;
use serde_json::Value;

/// The cheapest model in codex-cli 0.159.3's own list that still runs a command and follows an
/// instruction.
const MODEL: &str = "gpt-6-luna";

/// The cheapest Claude model, which still runs a command and follows an instruction.
const CLAUDE_MODEL: &str = "haiku";

/// Start, steer, finish, follow up, and cancel one consultation over the real app-server.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spends a Codex turn and needs a logged-in codex CLI"]
async fn a_live_codex_turn_is_steered_followed_up_and_cancelled() -> Result<()> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let root = tempfile::tempdir().or_fail()?.keep();
    let project = root.join("project");
    std::fs::create_dir_all(&project).or_fail()?;
    let store = RunStore::open(root.join("state"), Arc::new(ProcessLauncher::new()), env)
        .or_fail()?
        .with_codex_app_server(env!("CARGO_BIN_EXE_agentmux"));
    let account = live_account("AGENTMUX_LIVE_CODEX_ACCOUNT")?;

    let started = store.start(&StartRequest {
        delegate: Delegate::Codex {
            model: ModelId::parse(MODEL).or_fail()?,
            effort: Some(Effort::parse("low").or_fail()?),
            permissions: CodexPermissions {
                sandbox: Some(CodexSandbox::ReadOnly),
                network_access: None,
            },
            account: account.into(),
            isolation: Some(Isolation::Inherit),
        },
        question: "Run the shell command `sleep 20` and wait for it to finish. Then reply with \
                   one line: LIVE_DONE, followed by anything else you have been asked to add."
            .to_owned(),
        cwd: project,
        retention: Retention::UntilReleased,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
        label: None,
    })?;
    let id = started.run_id.clone();
    eprintln!(
        "live run {id}, transcript {}",
        started.transcript_path.display()
    );
    // The turn takes a steer once the driver has announced it.
    wait_for_status(&store, &id, "the turn never started", |status| {
        status.steerable
    })
    .await?;

    // Steered while the command runs; it reaches the model before its next call.
    store
        .steer(&id, "Also put the word STEERED at the end of that line.")
        .await?;
    store
        .wait_until_terminal(&id, Duration::from_secs(300))
        .await?;
    let (status, page) = store.view(&id, 0, usize::MAX)?;
    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(page.text, contains_substring("### user (steer)"));
    // The closing reply comes after the steer, and so after everything that asked for it.
    let (_, reply) = page.text.rsplit_once("### assistant").or_fail()?;
    assert_that!(reply, not(contains_substring("### user")));
    assert_that!(reply, contains_substring("LIVE_DONE"));
    assert_that!(reply, contains_substring("STEERED"));

    // A follow-up resumes the same thread; its replies are read from below its question.
    // The driver saves the session on its way out, after the turn's last event, so a follow-up
    // waits for it to leave.
    wait_for_status(&store, &id, "the driver never left", |status| {
        status.resumable
    })
    .await?;
    store.follow_up(&id, "Reply with exactly LIVE_AGAIN and nothing else.")?;
    store
        .wait_until_terminal(&id, Duration::from_secs(300))
        .await?;
    let (followed, page) = store.view(&id, status.transcript_bytes, usize::MAX)?;
    assert_that!(
        followed.outcome,
        matches_pattern!(Outcome::Completed { .. })
    );
    assert_that!(followed.broke_continuity, eq(false));
    let (_, replies) = page.text.split_once("### assistant").or_fail()?;
    assert_that!(replies, contains_substring("LIVE_AGAIN"));

    // A long turn is cancelled over the protocol once it is running, and the store records it.
    wait_for_status(&store, &id, "the driver never left", |status| {
        status.resumable
    })
    .await?;
    store.follow_up(
        &id,
        "Run the shell command `sleep 120`, then reply with exactly LIVE_LATE.",
    )?;
    wait_for_status(&store, &id, "the turn never started", |status| {
        status.steerable
    })
    .await?;
    // Time for the model to start the command, which the transcript does not show running.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let cancel_started_at = Instant::now();
    let cancelled = store.cancel(&id).await?;
    eprintln!("cancel took {:?}", cancel_started_at.elapsed());
    assert_that!(cancelled.outcome, eq(&Outcome::Cancelled));
    let page = store.page(&id, followed.transcript_bytes, usize::MAX)?;
    assert_that!(page.text, contains_substring("_Turn 3 cancelled."));
    // The interrupted turn never got to its answer.
    let replies = page
        .text
        .split_once("### assistant")
        .map_or("", |(_, replies)| replies);
    assert_that!(replies, not(contains_substring("LIVE_LATE")));
    // Nothing codex said in any of the three turns went unread.
    assert_that!(
        cancelled.unrecognised.summary(),
        none(),
        "the app-server protocol has drifted"
    );
    Ok(())
}

/// Steer a real Claude turn while its command runs, over stream-json input, and read the steer and
/// the answer it produced back from the transcript.
/// The steer is sent once Claude has called Bash, and lands between that call and the answer.
///
/// Isolated, so no hook or setting of the machine's own changes what Claude does, and allowed to
/// run `sleep` alone.
#[gtest]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spends a Claude turn and needs a logged-in claude CLI"]
async fn a_live_claude_turn_is_steered() -> Result<()> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let root = tempfile::tempdir().or_fail()?.keep();
    let project = root.join("project");
    std::fs::create_dir_all(&project).or_fail()?;
    let store = RunStore::open(root.join("state"), Arc::new(ProcessLauncher::new()), env)
        .or_fail()?
        .with_claude_driver(env!("CARGO_BIN_EXE_agentmux"));

    let started = store.start(&StartRequest {
        delegate: Delegate::Claude {
            model: ModelId::parse(CLAUDE_MODEL).or_fail()?,
            effort: Some(Effort::parse("low").or_fail()?),
            permissions: ClaudePermissions {
                permission_mode: Some(ClaudePermissionMode::Manual),
                tools: Some(ClaudeTool::parse_all(&["Bash"]).or_fail()?),
                allowed_tools: Some(ClaudeTool::parse_all(&["Bash(sleep *)"]).or_fail()?),
                disallowed_tools: None,
            },
            account: live_account("AGENTMUX_LIVE_CLAUDE_ACCOUNT")?.into(),
            isolation: Some(Isolation::Isolated),
        },
        question: "Run the shell command `sleep 20` with Bash and wait for it to finish. Then \
                   reply with one line: LIVE_DONE, followed by anything else you have been asked \
                   to add."
            .to_owned(),
        cwd: project,
        retention: Retention::UntilReleased,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
        label: None,
    })?;
    let id = started.run_id.clone();
    eprintln!(
        "live run {id}, transcript {}",
        started.transcript_path.display()
    );
    let run_dir = started.transcript_path.parent().or_fail()?.to_path_buf();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !capture_lines(&run_dir)?
        .iter()
        .any(|line| bash_call(line).is_some())
    {
        assert_that!(Instant::now() < deadline, eq(true), "Claude never ran Bash");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    store
        .steer(&id, "Also put the word STEERED at the end of that line.")
        .await?;
    store
        .wait_until_terminal(&id, Duration::from_secs(300))
        .await?;

    let (status, page) = store.view(&id, 0, usize::MAX)?;
    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(page.text, contains_substring("### user (steer)"));
    // Claude took the steer once Bash had finished, and before it reported its result.
    let lines = capture_lines(&run_dir)?;
    let bash = lines.iter().find_map(bash_call).or_fail()?;
    let finished = lines
        .iter()
        .position(|line| returns(line, bash))
        .or_fail()?;
    let steered = lines
        .iter()
        .position(|line| line_type(line) == Some("agentmux.steer"))
        .or_fail()?;
    let reported = lines
        .iter()
        .position(|line| line_type(line) == Some("result"))
        .or_fail()?;
    assert_that!(finished < steered && steered < reported, eq(true));
    // The closing reply comes after the steer, and so after everything that asked for it.
    let (_, reply) = page.text.rsplit_once("### assistant").or_fail()?;
    assert_that!(reply, not(contains_substring("### user")));
    assert_that!(reply, contains_substring("LIVE_DONE"));
    assert_that!(reply, contains_substring("STEERED"));
    assert_that!(
        status.unrecognised.summary(),
        none(),
        "the Claude event stream has drifted"
    );
    Ok(())
}

/// The lines of the first turn's capture in the consultation at `run_dir`, as far as they are
/// written.
fn capture_lines(run_dir: &Path) -> Result<Vec<Value>> {
    let capture = std::fs::read_to_string(run_dir.join("turns/0001/events.jsonl")).or_fail()?;
    Ok(capture
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// A capture line's `type`.
fn line_type(line: &Value) -> Option<&str> {
    line.get("type").and_then(Value::as_str)
}

/// The content blocks of a capture line of the type `kind`; none for a line of another type.
fn content_blocks<'line>(line: &'line Value, kind: &str) -> &'line [Value] {
    if line_type(line) != Some(kind) {
        return &[];
    }
    line.pointer("/message/content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The id of the Bash call a capture line makes, if it makes one.
fn bash_call(line: &Value) -> Option<&str> {
    content_blocks(line, "assistant")
        .iter()
        .find(|block| {
            line_type(block) == Some("tool_use") && block.get("name") == Some(&"Bash".into())
        })?
        .get("id")?
        .as_str()
}

/// Whether a capture line carries the result of the tool call `id`.
fn returns(line: &Value, id: &str) -> bool {
    content_blocks(line, "user").iter().any(|block| {
        line_type(block) == Some("tool_result")
            && block.get("tool_use_id").and_then(Value::as_str) == Some(id)
    })
}

/// The account the variable `name` names, or `None` for the machine's default.
fn live_account(name: &str) -> Result<Option<AccountAlias>> {
    std::env::var(name)
        .ok()
        .map(|alias| AccountAlias::parse(&alias))
        .transpose()
        .or_fail()
}

/// Poll the consultation until `ready` holds of its status, failing with `what` after a minute.
async fn wait_for_status(
    store: &RunStore,
    id: &RunId,
    what: &str,
    ready: impl Fn(&RunStatus) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready(&store.status(id)?) {
        assert_that!(Instant::now() < deadline, eq(true), "{what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}
