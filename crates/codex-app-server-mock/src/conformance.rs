//! Messages checked against codex's own JSON Schema rather than against the types generated from
//! it.
//!
//! The mock and a client of it build and read messages through the same generated types, so a
//! mistake in those types, or in what progeny could not represent, would pass both sides unseen.
//! These checks hold a message to the vendored schema bundle the types were generated from, the
//! one `codex app-server generate-json-schema` exported.
//! The `"jsonrpc"` member agentmux adds is allowed: the schema leaves extra members open.

use std::sync::LazyLock;

use serde_json::{Value, json};

/// The vendored bundle, from the generator's inputs.
const BUNDLE: &str =
    include_str!("../../codex-app-server-codegen/schema/codex_app_server_protocol.schemas.json");

/// The bundle's four message unions and its two answer envelopes, compiled once.
struct Messages {
    client_request: jsonschema::Validator,
    client_notification: jsonschema::Validator,
    server_request: jsonschema::Validator,
    server_notification: jsonschema::Validator,
    result: jsonschema::Validator,
    error: jsonschema::Validator,
}

static MESSAGES: LazyLock<Result<Messages, String>> = LazyLock::new(|| {
    Ok(Messages {
        client_request: validator("ClientRequest")?,
        client_notification: validator("ClientNotification")?,
        server_request: validator("ServerRequest")?,
        server_notification: validator("ServerNotification")?,
        result: validator("JSONRPCResponse")?,
        error: validator("JSONRPCError")?,
    })
});

/// A validator for the bundle's definition `name`, a top-level one or a v2 one.
fn validator(name: &str) -> Result<jsonschema::Validator, String> {
    let bundle: Value =
        serde_json::from_str(BUNDLE).map_err(|err| format!("the bundle is not JSON: {err}"))?;
    let definitions = bundle.get("definitions").cloned().unwrap_or(Value::Null);
    let target = if definitions.get(name).is_some() {
        format!("#/definitions/{name}")
    } else {
        format!("#/definitions/v2/{name}")
    };
    let schema = json!({"allOf": [{"$ref": target}], "definitions": definitions});
    jsonschema::draft7::new(&schema).map_err(|err| format!("cannot compile `{name}`: {err}"))
}

/// Who sent a message, which decides the union it must belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The client.
    Client,
    /// The server.
    Server,
}

/// Check one message, as it is on the wire, against what `sender` may send.
///
/// A request must be one of that side's requests and a notification one of its notifications,
/// each with the parameters codex's schema gives it.
/// An answer must carry exactly one of a result and an error, in the envelope the schema gives
/// each; which result answers which request the schema does not say, so that is not checked.
///
/// # Errors
///
/// Returns every way the message breaks the schema, one per line, each with where it is.
pub fn check(sender: Side, message: &Value) -> Result<(), String> {
    let messages = MESSAGES.as_ref().map_err(Clone::clone)?;
    let Some(members) = message.as_object() else {
        return Err(format!("{message} is not a JSON object"));
    };
    if members
        .get("method")
        .is_some_and(|method| !method.is_string())
    {
        return Err("a message's `method` is not a string".to_owned());
    }
    let has = |member| members.get(member).is_some_and(|value| !value.is_null());
    let schema = match (sender, has("id"), has("method")) {
        (Side::Client, true, true) => &messages.client_request,
        (Side::Client, false, true) => &messages.client_notification,
        (Side::Server, true, true) => &messages.server_request,
        (Side::Server, false, true) => &messages.server_notification,
        (_, true, false) => match (
            members.contains_key("result"),
            members.contains_key("error"),
        ) {
            (true, true) => return Err("an answer carries both a result and an error".to_owned()),
            (false, true) => &messages.error,
            _ => &messages.result,
        },
        (_, false, false) => return Err("a message names neither a method nor an id".to_owned()),
    };
    violations(schema, message)
}

/// Check a payload against the bundle's definition `name`, such as `ThreadStartResponse`.
///
/// # Errors
///
/// Returns every way the payload breaks the schema, one per line, or why the definition could
/// not be compiled.
pub fn check_definition(name: &str, payload: &Value) -> Result<(), String> {
    violations(&validator(name)?, payload)
}

/// Every way `instance` breaks `schema`, one per line, each with where it is.
fn violations(schema: &jsonschema::Validator, instance: &Value) -> Result<(), String> {
    let errors: Vec<String> = schema
        .iter_errors(instance)
        .map(|err| format!("{err} at `{}`", err.instance_path()))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}
