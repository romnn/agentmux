//! The MCP surface: eight tools over one run store, spoken over stdio.
//!
//! Changes when the tool API changes.
//!
//! The core library knows nothing about MCP.
//! This crate is the only thing that does, which is what makes the whole delegation pipeline
//! testable without a client.
//!
//! # Why these tools, in this shape
//!
//! Both hosts cap how long a tool call may block — Codex at sixty seconds by default, Claude Code
//! at ten minutes — and a review runs for far longer than either.
//! So no tool waits for a review: `start` returns an id the moment the child is
//! running, and the child is detached so it survives an agentmux restart.
//! `ask` is the same thing with a bounded wait folded in, for the common case where
//! the answer arrives quickly.
//!
//! Claude Code also caps tool result size and handles `structuredContent` unreliably, so every
//! tool returns plain text and always names a path.
//! The text is a bounded convenience; the file is the deliverable.

mod params;
pub mod render;
mod tools;

use std::sync::Arc;

use agentmux::run::RunStore;
use rmcp::handler::server::router::tool::ToolRouter;

pub use crate::tools::AgentMux;

/// Serving the tools over stdio failed.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The host never completed the MCP initialize handshake.
    ///
    /// Boxed because `rmcp`'s own initialize error is several hundred bytes, which would make
    /// every `Result` in this module pay for a case that happens at most once per process.
    #[error("the MCP host did not complete the initialize handshake: {0}")]
    Initialize(#[from] Box<rmcp::service::ServerInitializeError>),

    /// The service task ended abnormally.
    #[error("the MCP service task ended abnormally: {0}")]
    Service(#[from] tokio::task::JoinError),
}

/// Serve the tools on stdin and stdout until the host disconnects.
///
/// stdout is the JSON-RPC channel: anything else written there corrupts the protocol.
/// Tracing must be configured to write to stderr before this is called.
///
/// # Errors
///
/// Returns [`ServeError`] when the host does not complete the handshake, or when the service task
/// ends abnormally.
pub async fn serve_stdio(store: RunStore) -> Result<(), ServeError> {
    use rmcp::ServiceExt as _;

    let service = AgentMux::new(Arc::new(store))
        .serve(rmcp::transport::stdio())
        .await
        .map_err(Box::new)?;
    service.waiting().await?;
    Ok(())
}

/// The tools this server offers.
///
/// Exposed so a test can assert the generated schemas are the flat, fully described shapes a
/// calling model can fill in without guessing.
#[must_use]
pub fn tool_router() -> ToolRouter<AgentMux> {
    AgentMux::tool_router()
}
