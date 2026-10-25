//! The `tail` tool as a host sees it, over a real MCP transport.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentmux::delegate::{
    AccountChoice, CodexPermissions, CodexSandbox, Delegate, Effort, ModelId,
};
use agentmux::run::{Retention, RunStore, StartRequest};
use agentmux::testing::{Script, ScriptedLauncher, fixtures};
use agentmux_mcp::AgentMux;
use googletest::prelude::*;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt as _};

/// A server over a store with one consultation still running, and a client connected to it.
struct Connected {
    client: RunningService<RoleClient, ()>,
    store: Arc<RunStore>,
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
    let store = Arc::new(RunStore::open(dir.path(), launcher, env).or_fail()?);
    let started = store
        .start(&StartRequest {
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
        })
        .or_fail()?;

    let (server_end, client_end) = tokio::io::duplex(64 * 1024);
    let server = AgentMux::new(Arc::clone(&store));
    tokio::spawn(async move {
        if let Ok(service) = server.serve(server_end).await {
            service.waiting().await.ok();
        }
    });
    let client = ().serve(client_end).await.or_fail()?;
    Ok(Connected {
        client,
        store,
        run_id: started.run_id.to_string(),
        _dir: dir,
    })
}

/// Call `tail` with these arguments and return the text it answered with.
async fn tail(connected: &Connected, arguments: serde_json::Value) -> Result<String> {
    let result = connected
        .client
        .call_tool(
            CallToolRequestParams::new("tail")
                .with_arguments(arguments.as_object().cloned().or_fail()?),
        )
        .await
        .or_fail()?;
    let content = result.content.first().or_fail()?;
    Ok(content.as_text().or_fail()?.text.clone())
}

/// `from_end` answers with the newest bytes of the transcript and a cursor at its end.
///
/// This is the call a caller makes to see what a delegate hours into a turn is doing now, without
/// paging forward from the question it was asked.
#[gtest]
#[tokio::test]
async fn from_end_reads_the_newest_output_and_follows_on_from_the_end() -> Result<()> {
    let connected = connected().await?;
    let id = agentmux::run::RunId::parse(&connected.run_id).or_fail()?;
    let (status, whole) = connected.store.view(&id, 0, usize::MAX).or_fail()?;
    // Long enough that its first 100 bytes and its last are different text.
    assert_that!(whole.text.len(), gt(200));

    let answer = tail(
        &connected,
        serde_json::json!({ "run_id": connected.run_id, "from_end": true, "max_bytes": 100 }),
    )
    .await?;

    // The page is the transcript's last 100 bytes, not its first.
    let start = whole.text.ceil_char_boundary(whole.text.len() - 100);
    let newest = &whole.text[start..];
    let (_, body) = answer.split_once("\n---\n\n").or_fail()?;
    assert_that!(body, starts_with(newest));
    let expected_range = format!(
        "bytes:      {start}-{end} of {end}",
        end = status.transcript_bytes
    );
    assert_that!(answer, contains_substring(expected_range.as_str()));
    // Its cursor is the end of the transcript, so the next call reads only what arrives next.
    let expected_cursor = format!("next_cursor: {}", status.transcript_bytes);
    assert_that!(answer, contains_substring(expected_cursor.as_str()));
    Ok(())
}

/// A cursor and `from_end` name two different starting points, so asking for both is refused
/// rather than one silently winning.
#[gtest]
#[tokio::test]
async fn a_cursor_and_from_end_together_are_refused() -> Result<()> {
    let connected = connected().await?;
    let refused = connected
        .client
        .call_tool(
            CallToolRequestParams::new("tail").with_arguments(
                serde_json::json!({ "run_id": connected.run_id, "cursor": 0, "from_end": true })
                    .as_object()
                    .cloned()
                    .or_fail()?,
            ),
        )
        .await;
    assert_that!(refused, err(displays_as(contains_substring("not both"))));
    Ok(())
}
