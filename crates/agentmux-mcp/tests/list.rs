//! The `list` tool as a host sees it, over a real MCP transport.

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
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RunningService, ServiceError};
use rmcp::{RoleClient, ServiceExt as _};

/// A server over a store with one completed consultation, and a client connected to it.
struct Connected {
    client: RunningService<RoleClient, ()>,
    run_id: String,
    created_at: chrono::DateTime<chrono::Utc>,
    _dir: tempfile::TempDir,
}

async fn connected() -> Result<Connected> {
    let dir = tempfile::tempdir().or_fail()?;
    let launcher = Arc::new(ScriptedLauncher::new([Script::completed(
        fixtures::CODEX_HAPPY,
    )]));
    let store = Arc::new(RunStore::open(dir.path(), launcher, BTreeMap::new()).or_fail()?);
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
            label: None,
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
        run_id: started.run_id.to_string(),
        created_at: started.created_at,
        _dir: dir,
    })
}

/// Call `list` with these arguments.
async fn list(
    connected: &Connected,
    arguments: serde_json::Value,
) -> Result<std::result::Result<CallToolResult, ServiceError>> {
    Ok(connected
        .client
        .call_tool(
            CallToolRequestParams::new("list")
                .with_arguments(arguments.as_object().cloned().or_fail()?),
        )
        .await)
}

/// The text a successful call answered with.
fn text_of(result: &CallToolResult) -> Result<String> {
    Ok(result
        .content
        .first()
        .or_fail()?
        .as_text()
        .or_fail()?
        .text
        .clone())
}

/// `since` lists the consultations created at or after it, each with its run total, and none
/// created before it.
#[gtest]
#[tokio::test]
async fn since_keeps_the_consultations_created_from_it_on() -> Result<()> {
    let connected = connected().await?;

    let from_creation = list(
        &connected,
        serde_json::json!({ "since": connected.created_at.to_rfc3339() }),
    )
    .await?
    .or_fail()?;
    let from_creation = text_of(&from_creation)?;
    assert_that!(from_creation, contains_substring(connected.run_id.as_str()));
    // The compact run total, as the fixture's one turn reported it.
    assert_that!(from_creation, contains_substring("  14k in / 31 out\n"));

    let later = connected.created_at + chrono::Duration::hours(1);
    let from_later = list(
        &connected,
        serde_json::json!({ "since": later.to_rfc3339() }),
    )
    .await?
    .or_fail()?;
    assert_that!(
        text_of(&from_later)?,
        all![
            not(contains_substring(connected.run_id.as_str())),
            contains_substring("no consultations match")
        ]
    );
    Ok(())
}

/// A `since` that is not an RFC 3339 instant is refused by naming the format, rather than read
/// as no filter at all.
#[gtest]
#[tokio::test]
async fn a_since_that_is_not_an_instant_is_refused() -> Result<()> {
    let connected = connected().await?;

    let refused = list(&connected, serde_json::json!({ "since": "16:00" })).await?;

    assert_that!(refused, err(displays_as(contains_substring("RFC 3339"))));
    Ok(())
}
