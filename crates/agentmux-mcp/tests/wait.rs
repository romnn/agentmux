//! The `wait` tool as a host sees it: over a real MCP transport, with a client that asks for
//! progress, against a clock that lets a wait of many minutes run in an instant.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentmux::delegate::{
    AccountChoice, CodexPermissions, CodexSandbox, Delegate, Effort, ModelId,
};
use agentmux::run::{Retention, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use agentmux::transcript::Outcome;
use agentmux_mcp::AgentMux;
use googletest::prelude::*;
use rmcp::model::{
    CallToolRequestParams, ClientRequest, ProgressNotificationParam, Request, ServerResult,
};
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService};
use rmcp::{ClientHandler, RoleClient, ServiceExt as _};

/// A client that keeps every progress notification it is sent.
#[derive(Clone, Default)]
struct ProgressRecorder {
    received: Arc<std::sync::Mutex<Vec<ProgressNotificationParam>>>,
}

impl ProgressRecorder {
    fn received(&self) -> Vec<ProgressNotificationParam> {
        self.received
            .lock()
            .map(|received| received.clone())
            .unwrap_or_default()
    }
}

impl ClientHandler for ProgressRecorder {
    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        if let Ok(mut received) = self.received.lock() {
            received.push(params);
        }
        std::future::ready(())
    }
}

/// A server over a store whose one consultation has answered and whose delegate has not left, and
/// a client connected to it.
struct Connected {
    client: RunningService<RoleClient, ProgressRecorder>,
    recorder: ProgressRecorder,
    store: Arc<RunStore>,
    launcher: Arc<ScriptedLauncher>,
    run_id: String,
    _dir: tempfile::TempDir,
}

async fn connected() -> Result<Connected> {
    connected_to(fixtures::CODEX_HAPPY).await
}

/// As [`connected`], with the consultation's delegate having written `events` and still there.
async fn connected_to(events: &str) -> Result<Connected> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([Script::running(events)]));
    let env: BTreeMap<String, String> = [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let store = Arc::new(RunStore::open(dir.path(), launcher.clone(), env).or_fail()?);
    let started = store.start(&StartRequest {
        delegate: Delegate::Codex {
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            effort: Some(Effort::parse("high").or_fail()?),
            permissions: CodexPermissions {
                sandbox: Some(CodexSandbox::ReadOnly),
                network_access: None,
            },
            account: AccountChoice::Default,
            isolation: None,
        },
        question: "Review it.".to_owned(),
        cwd: PathBuf::from("/work/project"),
        retention: Retention::Ttl,
        env: BTreeMap::new(),
        billing: None,
        force_codex_exec: false,
        label: None,
    })?;

    let (server_end, client_end) = tokio::io::duplex(64 * 1024);
    let server = AgentMux::new(Arc::clone(&store));
    tokio::spawn(async move {
        if let Ok(service) = server.serve(server_end).await {
            service.waiting().await.ok();
        }
    });
    let recorder = ProgressRecorder::default();
    let client = recorder.clone().serve(client_end).await.or_fail()?;
    Ok(Connected {
        client,
        recorder,
        store,
        launcher,
        run_id: started.run_id.to_string(),
        _dir: dir,
    })
}

fn wait_request(run_id: &str) -> Result<ClientRequest> {
    tool_request("wait", &serde_json::json!({ "run_id": run_id }))
}

fn tool_request(tool: &'static str, arguments: &serde_json::Value) -> Result<ClientRequest> {
    Ok(ClientRequest::CallToolRequest(Request::new(
        CallToolRequestParams::new(tool).with_arguments(arguments.as_object().cloned().or_fail()?),
    )))
}

/// The text of a tool's answer.
fn answer_text(response: ServerResult) -> Result<String> {
    let ServerResult::CallToolResult(result) = response else {
        return fail!("the tool answered with something other than a tool result")
            .map(|()| String::new());
    };
    assert_that!(result.is_error, not(some(eq(true))));
    result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|text| text.text.clone())
        .or_fail()
}

/// A running turn that asked one question, as the app-server driver writes it.
const ASKED: &str = indoc::indoc! {r#"
    {"type":"thread.started","thread_id":"01a0"}
    {"type":"turn.started"}
    {"type":"agentmux.question","id":"call_1","asked_at":"2026-11-05T03:06:13Z","questions":[{"title":"Assume valid namespace names?","options":["Yes (Recommended)","No"]}]}
"#};

/// `wait` with `or_delegate_question` answers while the turn still runs, once the delegate has asked
/// something: the question, and the calls that reply to it and keep waiting past it.
#[gtest]
#[tokio::test(start_paused = true)]
async fn wait_or_delegate_question_answers_on_a_question_the_running_turn_asks() -> Result<()> {
    let connected = connected_to(ASKED).await?;
    let request = tool_request(
        "wait",
        &serde_json::json!({ "run_id": connected.run_id, "or_delegate_question": true }),
    )?;
    let handle = connected
        .client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await
        .or_fail()?;
    let response = tokio::time::timeout(Duration::from_secs(10), handle.await_response())
        .await
        .or_fail()?
        .or_fail()?;
    let text = answer_text(response)?;

    assert_that!(
        text,
        contains_substring("this wait returned because the\n            delegate asked a question")
    );
    assert_that!(text, contains_substring("turn 1 question 1, asked"));
    assert_that!(text, contains_substring("Assume valid namespace names?"));
    assert_that!(
        text,
        contains_substring("options:\n              - Yes (Recommended)\n              - No")
    );
    assert_that!(text, contains_substring("Reply with `steer`"));
    assert_that!(
        text,
        contains_substring(r#""or_delegate_question": true, "delegate_questions_seen": 1"#)
    );
    // The consultation was only observed.
    assert_that!(
        connected
            .store
            .status(&agentmux::run::RunId::parse(&connected.run_id).or_fail()?)?
            .outcome,
        eq(&Outcome::Running)
    );
    Ok(())
}

/// A settled turn's `wait` says what it asked above its transcript, and that only a follow-up
/// can reply now.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_settled_wait_says_what_the_turn_asked() -> Result<()> {
    let events = format!(
        "{ASKED}{}\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#
    );
    let connected = connected_to(&events).await?;
    connected
        .launcher
        .mark_process_gone(connected.launcher.launch_handle(0));
    let handle = connected
        .client
        .send_cancellable_request(
            wait_request(&connected.run_id)?,
            PeerRequestOptions::no_options(),
        )
        .await
        .or_fail()?;
    let response = tokio::time::timeout(Duration::from_secs(10), handle.await_response())
        .await
        .or_fail()?
        .or_fail()?;
    let text = answer_text(response)?;

    let (header, transcript) = text.split_once("\n---\n").or_fail()?;
    assert_that!(
        header,
        contains_substring("asked:      the delegate asked 1 question(s)")
    );
    assert_that!(header, contains_substring("Assume valid namespace names?"));
    assert_that!(
        header,
        contains_substring("only a `follow_up` can reply now, where `status` reports")
    );
    assert_that!(transcript, contains_substring("### assistant (question)"));
    Ok(())
}

/// `wait` holds the call for as long as the turn takes — well past the 540 seconds every bounded
/// tool stops at — telling the client every thirty seconds that it still is, with its own token
/// and a rising count, and answers only once the turn has settled.
#[gtest]
#[tokio::test(start_paused = true)]
async fn wait_answers_only_once_the_turn_has_settled() -> Result<()> {
    let connected = connected().await?;
    let mut handle = connected
        .client
        .send_cancellable_request(
            wait_request(&connected.run_id)?,
            PeerRequestOptions::no_options(),
        )
        .await
        .or_fail()?;

    // Ten minutes in, the stream long since finished but the delegate still there.
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_that!(handle.rx.try_recv().is_err(), eq(true));
    let progress = connected.recorder.received();
    assert_that!(
        progress.len() >= 19,
        eq(true),
        "{} notifications",
        progress.len()
    );
    assert_that!(
        progress
            .iter()
            .all(|notification| notification.progress_token == handle.progress_token),
        eq(true)
    );
    assert_that!(
        progress
            .windows(2)
            .all(|pair| matches!(pair, [earlier, later] if later.progress > earlier.progress)),
        eq(true)
    );

    connected
        .launcher
        .mark_process_gone(connected.launcher.launch_handle(0));
    let response = tokio::time::timeout(Duration::from_secs(5), handle.await_response())
        .await
        .or_fail()?
        .or_fail()?;
    let ServerResult::CallToolResult(result) = response else {
        return fail!("wait answered with something other than a tool result");
    };
    let text = result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|text| text.text.clone())
        .or_fail()?;
    assert_that!(result.is_error, not(some(eq(true))));
    assert_that!(text, contains_substring("settled:    turn 1 is final"));
    assert_that!(text, contains_substring("_Turn 1 completed"));
    assert_that!(text, not(contains_substring("Still working")));
    Ok(())
}

/// Cancelling a `wait` ends the wait, not the consultation: nothing is signalled, and the turn is
/// left to settle.
#[gtest]
#[tokio::test(start_paused = true)]
async fn cancelling_a_wait_leaves_the_consultation_running() -> Result<()> {
    let connected = connected().await?;
    let handle = connected
        .client
        .send_cancellable_request(
            wait_request(&connected.run_id)?,
            PeerRequestOptions::no_options(),
        )
        .await
        .or_fail()?;
    tokio::time::sleep(Duration::from_secs(90)).await;
    handle
        .cancel(Some("the caller moved on".to_owned()))
        .await
        .or_fail()?;
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert_that!(connected.launcher.terminated(), is_empty());
    assert_that!(connected.launcher.killed(), is_empty());
    let run_id = agentmux::run::RunId::parse(&connected.run_id).or_fail()?;
    let status = connected.store.status(&run_id)?;
    assert_that!(status.outcome, matches_pattern!(Outcome::Completed { .. }));
    assert_that!(status.settled, eq(false));
    Ok(())
}

/// A wait that returns on several questions at once hands over every one, whole, and the rearm it
/// names acknowledges exactly those.
#[gtest]
#[tokio::test(start_paused = true)]
async fn a_wait_hands_over_every_question_it_returns_on() -> Result<()> {
    let long_title = format!("Should the gate run? {}", "context ".repeat(200));
    let questions: Vec<serde_json::Value> = [long_title.as_str(), "B?", "C?", "D?"]
        .iter()
        .map(|title| serde_json::json!({"title": title}))
        .collect();
    let question = serde_json::json!({
        "type": "agentmux.question", "id": "call_1", "asked_at": "2026-11-05T03:06:13Z",
        "questions": questions,
    });
    let events = format!(
        "{}{question}\n",
        indoc::indoc! {r#"
            {"type":"thread.started","thread_id":"01a0"}
            {"type":"turn.started"}
        "#}
    );
    let connected = connected_to(&events).await?;
    let request = tool_request(
        "wait",
        &serde_json::json!({ "run_id": connected.run_id, "or_delegate_question": true }),
    )?;
    let handle = connected
        .client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await
        .or_fail()?;
    let response = tokio::time::timeout(Duration::from_secs(10), handle.await_response())
        .await
        .or_fail()?
        .or_fail()?;
    let text = answer_text(response)?;

    for number in 1..=4 {
        expect_that!(
            text,
            contains_substring(format!("turn 1 question {number}, asked"))
        );
    }
    expect_that!(text, contains_substring(long_title.trim_end()));
    expect_that!(text, contains_substring(r#""delegate_questions_seen": 4"#));
    expect_that!(text, not(contains_substring("not shown")));
    Ok(())
}
