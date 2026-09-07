//! The MCP surface: nine tools over one run store, spoken over stdio.
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

use std::collections::BTreeSet;
use std::sync::Arc;

use agentmux::delegate::Vendor;
use agentmux::run::RunStore;
use rmcp::handler::server::router::tool::ToolRouter;
use serde_json::Value;

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

/// The tools this server offers, without the vendors it will not launch.
///
/// Exposed so a test can assert the generated schemas are the flat, fully described shapes a
/// calling model can fill in without guessing.
#[must_use]
pub fn tool_router(denied: &BTreeSet<Vendor>) -> ToolRouter<AgentMux> {
    let mut router = AgentMux::tool_router();
    narrow_delegate_choice(&mut router, denied);
    router
}

/// Leave a denied vendor out of the choices a tool that starts a consultation offers.
///
/// This is advertising, not the gate.
/// [`RunStore`] refuses the launch itself, so a host that ignores schemas — or a caller that sends
/// the denied name anyway — still gets that refusal; what this buys is that a calling model never
/// spends a call discovering it.
/// A schema whose shape has moved therefore leaves the list intact rather than failing: the wide
/// list costs one refused call, while a wrong narrowing would hide a vendor that does work.
///
/// A tool that only reads *about* a vendor keeps both: `quota` leaves `delegate` optional, and an
/// account this server will not launch still has a quota worth reporting.
/// Requiring a delegate is what marks a tool as one that starts a consultation, so a tool added
/// later cannot quietly escape this by being forgotten in a list of names.
fn narrow_delegate_choice(router: &mut ToolRouter<AgentMux>, denied: &BTreeSet<Vendor>) {
    // Spelled by the enum's own serde name, so the schema cannot disagree with what the store
    // will match against.
    let denied: Vec<Value> = denied
        .iter()
        .filter_map(|vendor| serde_json::to_value(vendor).ok())
        .collect();
    if denied.is_empty() {
        return;
    }

    for route in router.map.values_mut() {
        if !requires_delegate(&route.attr.input_schema) {
            continue;
        }
        let Some(choice) = Arc::make_mut(&mut route.attr.input_schema)
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .and_then(|properties| properties.get_mut("delegate"))
            .and_then(Value::as_object_mut)
            .and_then(|delegate| delegate.get_mut("enum"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        choice.retain(|value| !denied.contains(value));
    }
}

/// Whether a tool takes a delegate it must launch, rather than one it may ask about.
fn requires_delegate(schema: &serde_json::Map<String, Value>) -> bool {
    schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| {
            required
                .iter()
                .any(|field| field.as_str() == Some("delegate"))
        })
}
