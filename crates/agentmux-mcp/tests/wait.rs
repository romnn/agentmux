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
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([Script::running(
        fixtures::CODEX_HAPPY,
    )]));
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
    let arguments = serde_json::json!({ "run_id": run_id });
    Ok(ClientRequest::CallToolRequest(Request::new(
        CallToolRequestParams::new("wait")
            .with_arguments(arguments.as_object().cloned().or_fail()?),
    )))
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
