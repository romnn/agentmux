//! The JSON-RPC messages agentmux exchanges with `codex app-server`, one per line.
//!
//! The payloads are the generated types in [`super::schema`], rendered by progeny from the
//! schema the codex CLI itself publishes (`codex app-server generate-json-schema`); only the
//! JSON-RPC envelope around them is written here.
//! The server writes JSON-RPC without the `"jsonrpc"` member, so a message is classified by which
//! of `id` and `method` it carries rather than by that.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(super) use super::schema::types as wire;
use crate::delegate::CodexSandbox;

/// Notifications the driver asks the server not to send.
///
/// Every one of them is a token-by-token delta of something that also arrives whole in
/// `item/completed`, and the driver writes only whole items.
pub(super) const OPTED_OUT: &[&str] = &[
    "item/agentMessage/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/plan/delta",
    "command/exec/outputDelta",
    "process/outputDelta",
];

/// JSON-RPC's code for a method the receiver does not offer.
pub(super) const METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC's code for a failure inside the receiver.
pub(super) const INTERNAL_ERROR: i64 = -32603;

/// Encode `payload` as JSON, saying what it was when it cannot be.
fn to_value(what: &str, payload: &impl Serialize) -> Result<Value, String> {
    serde_json::to_value(payload).map_err(|error| format!("cannot encode {what}: {error}"))
}

/// A request the driver sends: a generated parameter type, and the method that carries it.
///
/// Implemented only for those types, so a request cannot be sent with the wrong method, nor with
/// anything that merely happens to serialize.
pub(super) trait Request: Serialize {
    /// The JSON-RPC method.
    const METHOD: &'static str;
}

impl Request for wire::InitializeParams {
    const METHOD: &'static str = "initialize";
}
impl Request for wire::ThreadStartParams {
    const METHOD: &'static str = "thread/start";
}
impl Request for wire::ThreadResumeParams {
    const METHOD: &'static str = "thread/resume";
}
impl Request for wire::TurnStartParams {
    const METHOD: &'static str = "turn/start";
}
impl Request for wire::TurnSteerParams {
    const METHOD: &'static str = "turn/steer";
}
impl Request for wire::TurnInterruptParams {
    const METHOD: &'static str = "turn/interrupt";
}

/// One request, encoded as the line the server reads.
pub(super) fn request<P: Request>(id: u64, params: &P) -> Result<String, String> {
    let params = to_value(P::METHOD, params)?;
    Ok(json!({"jsonrpc": "2.0", "id": id, "method": P::METHOD, "params": params}).to_string())
}

/// One notification without parameters, encoded as the line the server reads.
pub(super) fn notification(method: &str) -> String {
    json!({"jsonrpc": "2.0", "method": method}).to_string()
}

/// A reply to a request the server sent, as a result.
pub(super) fn result(id: &Value, result: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// A reply to a request the server sent, as an error.
pub(super) fn error(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

/// Decode a payload into the type codex's schema gives it.
///
/// The error names the type, because a payload that does not decode means the server speaks a
/// protocol other than the one these types were generated from.
pub(super) fn decode<T: DeserializeOwned>(payload: &Value) -> Result<T, String> {
    T::deserialize(payload).map_err(|error| {
        let name = std::any::type_name::<T>();
        let short = name.rsplit("::").next().unwrap_or(name);
        format!(
            "codex sent a {short} that does not match the schema agentmux was generated from: \
             {error}"
        )
    })
}

/// `initialize`'s parameters.
pub(super) fn initialize_params() -> wire::InitializeParams {
    wire::InitializeParams {
        client_info: wire::ClientInfo {
            name: "agentmux".to_owned(),
            title: None,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        capabilities: Some(wire::InitializeCapabilities {
            opt_out_notification_methods: Some(
                OPTED_OUT.iter().map(|&method| method.to_owned()).collect(),
            ),
            ..wire::InitializeCapabilities::default()
        }),
    }
}

/// The schema's name for a sandbox mode.
fn sandbox_mode(sandbox: CodexSandbox) -> wire::SandboxMode {
    match sandbox {
        CodexSandbox::ReadOnly => wire::SandboxMode::ReadOnly,
        CodexSandbox::WorkspaceWrite => wire::SandboxMode::WorkspaceWrite,
    }
}

/// `never`, which is what `codex exec` runs with: nobody is there to ask.
fn never_ask() -> wire::AskForApproval {
    wire::AskForApproval::AskForApproval0(Box::new(wire::AskForApproval0::Never))
}

/// `thread/start`'s parameters, for a turn that opens a consultation.
///
/// The thread is persisted (`ephemeral: false`) so a follow-up can resume it: the counterpart of
/// `exec` without `--ephemeral`.
pub(super) fn thread_start_params(
    model: &str,
    cwd: &str,
    sandbox: CodexSandbox,
) -> wire::ThreadStartParams {
    wire::ThreadStartParams {
        model: Some(model.to_owned()),
        cwd: Some(cwd.to_owned()),
        sandbox: Some(sandbox_mode(sandbox)),
        approval_policy: Some(never_ask()),
        ephemeral: Some(false),
        approvals_reviewer: None,
        base_instructions: None,
        config: None,
        developer_instructions: None,
        model_provider: None,
        personality: None,
        service_name: None,
        service_tier: None,
        session_start_source: None,
        thread_source: None,
    }
}

/// `thread/resume`'s parameters, for a follow-up.
///
/// A resume does not inherit the model the thread started with, so it is pinned on every turn, as
/// `codex exec resume` pins it, and so are the sandbox and the approval policy.
/// The thread's earlier turns are left out of the answer: the delegate has them, and the driver
/// reads nothing but the thread's id.
pub(super) fn thread_resume_params(
    thread_id: &str,
    model: &str,
    cwd: &str,
    sandbox: CodexSandbox,
) -> wire::ThreadResumeParams {
    wire::ThreadResumeParams {
        thread_id: thread_id.to_owned(),
        model: Some(model.to_owned()),
        cwd: Some(cwd.to_owned()),
        sandbox: Some(sandbox_mode(sandbox)),
        approval_policy: Some(never_ask()),
        exclude_turns: Some(true),
        approvals_reviewer: None,
        base_instructions: None,
        config: None,
        developer_instructions: None,
        model_provider: None,
        personality: None,
        service_tier: None,
    }
}

/// The single text input both `turn/start` and `turn/steer` carry.
///
/// `UserInput` itself is generated as plain JSON — progeny cannot tell its branches apart, as
/// `codegen/app-server/generate.sh` records — so the branch is built as its own generated type
/// and encoded into that slot.
fn text_input(text: &str) -> Result<Vec<wire::UserInput>, String> {
    let input = wire::TextUserInput {
        text: text.to_owned(),
        text_elements: None,
        type_: wire::TextUserInputType::Text,
    };
    Ok(vec![to_value("the input", &input)?])
}

/// `turn/start`'s parameters.
pub(super) fn turn_start_params(
    thread_id: &str,
    question: &str,
) -> Result<wire::TurnStartParams, String> {
    // Everything else is the thread's, which `thread/start` or `thread/resume` already set.
    Ok(wire::TurnStartParams {
        thread_id: thread_id.to_owned(),
        input: text_input(question)?,
        approval_policy: None,
        approvals_reviewer: None,
        client_user_message_id: None,
        cwd: None,
        disabled_plugin_ids: None,
        effort: None,
        model: None,
        output_schema: None,
        parent_turn_id: None,
        personality: None,
        root_turn_id: None,
        sandbox_policy: None,
        service_tier: None,
        service_tier_for_turn: None,
        summary: None,
        tool_output: None,
        turn_trigger: None,
    })
}

/// `turn/steer`'s parameters.
///
/// `expectedTurnId` is required, and is what makes a steer that arrives after its turn ended a
/// refusal rather than the opening of a new turn.
pub(super) fn turn_steer_params(
    thread_id: &str,
    turn_id: &str,
    text: &str,
) -> Result<wire::TurnSteerParams, String> {
    Ok(wire::TurnSteerParams {
        thread_id: thread_id.to_owned(),
        expected_turn_id: turn_id.to_owned(),
        input: text_input(text)?,
        client_user_message_id: None,
    })
}

/// `turn/interrupt`'s parameters.
pub(super) fn turn_interrupt_params(thread_id: &str, turn_id: &str) -> wire::TurnInterruptParams {
    wire::TurnInterruptParams {
        thread_id: thread_id.to_owned(),
        turn_id: turn_id.to_owned(),
    }
}

/// A message the server sent.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Incoming {
    /// The answer to one of the driver's requests.
    Response {
        /// The driver's request id.
        id: u64,
        /// The result, or the server's error message.
        outcome: Result<Value, String>,
    },
    /// Something the server reports unprompted.
    Notification {
        /// The notification's method.
        method: String,
        /// Its parameters, `null` when absent.
        params: Value,
    },
    /// Something the server asks the driver, which must be answered.
    Request {
        /// The server's own id, echoed back verbatim.
        id: Value,
        /// The request's method.
        method: String,
    },
}

/// Classify one line from the server, or `None` for one that is not a JSON-RPC message.
pub(super) fn parse(line: &str) -> Option<Incoming> {
    let message: Value = serde_json::from_str(line).ok()?;
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").filter(|id| !id.is_null());
    match (id, method) {
        (Some(id), Some(method)) => Some(Incoming::Request {
            id: id.clone(),
            method: method.to_owned(),
        }),
        (Some(id), None) => {
            let id = id.as_u64()?;
            let outcome = match message.get("error") {
                Some(error) => Err(error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the server gave no reason")
                    .to_owned()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            Some(Incoming::Response { id, outcome })
        }
        (None, Some(method)) => Some(Incoming::Notification {
            method: method.to_owned(),
            params: message.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, None) => None,
    }
}

/// The reason every decline gives.
const UNATTENDED: &str = "agentmux runs consultations unattended, with nobody to approve this";

/// A decline for an approval or input request, when the method is one the driver knows.
///
/// Each answer is the one the schema documents as "denied, and the turn continues", so the model
/// learns the action was refused rather than the turn ending.
pub(super) fn decline(method: &str) -> Option<Result<Value, String>> {
    let denied = || {
        wire::ReviewDecision::DeniedReviewDecision(Box::new(wire::DeniedReviewDecision {
            denied: wire::DeniedReviewDecisionDenied {
                rejection: UNATTENDED.to_owned(),
            },
        }))
    };
    Some(match method {
        "item/commandExecution/requestApproval" => to_value(
            method,
            &wire::CommandExecutionRequestApprovalResponse {
                decision: wire::CommandExecutionApprovalDecision::CommandExecutionApprovalDecision4(
                    Box::new(wire::CommandExecutionApprovalDecision4::Decline),
                ),
            },
        ),
        "item/fileChange/requestApproval" => to_value(
            method,
            &wire::FileChangeRequestApprovalResponse {
                decision: wire::FileChangeApprovalDecision::Decline,
            },
        ),
        "execCommandApproval" => to_value(
            method,
            &wire::ExecCommandApprovalResponse { decision: denied() },
        ),
        "applyPatchApproval" => to_value(
            method,
            &wire::ApplyPatchApprovalResponse { decision: denied() },
        ),
        "mcpServer/elicitation/request" => to_value(
            method,
            &wire::McpServerElicitationRequestResponse {
                meta: None,
                action: wire::McpServerElicitationAction::Decline,
                content: None,
            },
        ),
        "item/tool/requestUserInput" => to_value(
            method,
            &wire::ToolRequestUserInputResponse {
                answers: std::collections::BTreeMap::new(),
            },
        ),
        // An empty grant: no file system and no network beyond what the sandbox already allows.
        "item/permissions/requestApproval" => to_value(
            method,
            &wire::PermissionsRequestApprovalResponse {
                permissions: wire::GrantedPermissionProfile {
                    file_system: None,
                    network: None,
                },
                scope: None,
                strict_auto_review: None,
            },
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use serde_json::{Value, json};

    use super::{Incoming, decline, parse, request, turn_interrupt_params, turn_steer_params};

    /// A steer names the turn it is for and carries the instruction as one text input.
    #[gtest]
    fn a_steer_encodes_its_turn_precondition_and_text() -> Result<()> {
        let params = turn_steer_params("th-1", "tu-2", "only auth").or_fail()?;
        let line = request(7, &params).or_fail()?;
        let message: Value = serde_json::from_str(&line).or_fail()?;

        assert_that!(
            message,
            eq(&json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "turn/steer",
                "params": {
                    "threadId": "th-1",
                    "expectedTurnId": "tu-2",
                    "input": [{"type": "text", "text": "only auth"}],
                },
            }))
        );
        // One message per line: the server reads newline-delimited JSON.
        assert_that!(line.contains('\n'), eq(false));
        Ok(())
    }

    /// An interrupt names both the thread and the turn.
    #[gtest]
    fn an_interrupt_names_the_thread_and_the_turn() -> Result<()> {
        let line = request(3, &turn_interrupt_params("th-1", "tu-2")).or_fail()?;
        let message: Value = serde_json::from_str(&line).or_fail()?;
        assert_that!(
            message.get("params"),
            some(eq(&json!({"threadId": "th-1", "turnId": "tu-2"})))
        );
        Ok(())
    }

    /// Every decline is the schema's own "denied, carry on" for its request.
    #[gtest]
    fn approvals_are_declined_in_the_schemas_own_words() -> Result<()> {
        assert_that!(
            decline("item/commandExecution/requestApproval")
                .or_fail()?
                .or_fail()?,
            eq(&json!({"decision": "decline"}))
        );
        assert_that!(
            decline("item/fileChange/requestApproval")
                .or_fail()?
                .or_fail()?,
            eq(&json!({"decision": "decline"}))
        );
        // The v1 review decision denies with an object that carries the reason.
        assert_that!(
            decline("execCommandApproval").or_fail()?.or_fail()?,
            eq(&json!({"decision": {"denied": {"rejection": super::UNATTENDED}}}))
        );
        assert_that!(
            decline("mcpServer/elicitation/request")
                .or_fail()?
                .or_fail()?,
            eq(&json!({"action": "decline"}))
        );
        // A request kind agentmux does not know is not declined: it is refused outright.
        assert_that!(decline("item/tool/call"), none());
        Ok(())
    }

    /// Responses, notifications and server requests are told apart by `id` and `method`, with or
    /// without the `jsonrpc` member the server leaves out.
    #[gtest]
    fn incoming_messages_are_classified_by_their_members() {
        assert_that!(
            parse(r#"{"id":3,"result":{"turnId":"tu-2"}}"#),
            some(eq(&Incoming::Response {
                id: 3,
                outcome: Ok(json!({"turnId": "tu-2"})),
            }))
        );
        assert_that!(
            parse(r#"{"id":4,"error":{"code":-32600,"message":"no active turn"}}"#),
            some(eq(&Incoming::Response {
                id: 4,
                outcome: Err("no active turn".to_owned()),
            }))
        );
        assert_that!(
            parse(r#"{"method":"turn/completed","params":{"threadId":"t"}}"#),
            some(eq(&Incoming::Notification {
                method: "turn/completed".to_owned(),
                params: json!({"threadId": "t"}),
            }))
        );
        assert_that!(
            parse(
                r#"{"jsonrpc":"2.0","id":"srv-1","method":"item/fileChange/requestApproval","params":{}}"#
            ),
            some(eq(&Incoming::Request {
                id: json!("srv-1"),
                method: "item/fileChange/requestApproval".to_owned(),
            }))
        );
        // A line that is not JSON-RPC at all is not a message.
        assert_that!(parse("warning: something"), none());
        assert_that!(parse(r#"{"neither":true}"#), none());
    }
}
