//! What agentmux says to `codex app-server`: the parameters of the requests it sends, and how it
//! answers the requests codex sends it.
//!
//! The payload types, the method bindings and the JSON-RPC envelope are
//! [`codex_app_server_protocol`]'s; what is decided here is agentmux's alone — which settings a
//! thread runs under, and that a consultation declines everything it is asked.

use codex_app_server_protocol::envelope::{EncodeError, Message, RequestId};
use codex_app_server_protocol::methods::{self as m, AnyServerRequest as Any};
use codex_app_server_protocol::{Method as _, ServerRequest, types as wire};

use crate::delegate::{CodexSandbox, SessionRef};

/// Notifications the driver asks the server not to send.
///
/// Every one of them is a token-by-token delta of something that also arrives whole in
/// `item/completed`, and the driver writes only whole items.
pub(super) const OPTED_OUT: &[&str] = &[
    m::ItemAgentMessageDelta::METHOD,
    m::ItemReasoningSummaryTextDelta::METHOD,
    m::ItemReasoningSummaryPartAdded::METHOD,
    m::ItemReasoningTextDelta::METHOD,
    m::ItemCommandExecutionOutputDelta::METHOD,
    m::ItemFileChangeOutputDelta::METHOD,
    m::ItemPlanDelta::METHOD,
    m::CommandExecOutputDelta::METHOD,
    m::ProcessOutputDelta::METHOD,
];

/// codex's id for the thread a consultation runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ThreadId(String);

/// codex's id for one turn of a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TurnId(String);

macro_rules! wire_id {
    ($($ty:ident),+) => {$(
        impl $ty {
            /// The id as codex sent it.
            pub(super) fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// The id as codex reads it.
            pub(super) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    )+};
}
wire_id!(ThreadId, TurnId);

/// The running turn a steer or an interrupt is sent into.
#[derive(Debug, Clone, Copy)]
pub(super) struct TurnTarget<'a> {
    pub(super) thread: &'a ThreadId,
    pub(super) turn: &'a TurnId,
}

/// `initialize`'s parameters, which the quota probe sends too.
pub(crate) fn initialize_params() -> wire::InitializeParams {
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
    session: &SessionRef,
    model: &str,
    cwd: &str,
    sandbox: CodexSandbox,
) -> wire::ThreadResumeParams {
    wire::ThreadResumeParams {
        thread_id: session.as_str().to_owned(),
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
/// `UserInput` itself is generated as plain JSON — progeny cannot tell its branches apart, as the
/// generated module's header records — so the branch is built as its own generated type and
/// encoded into that slot.
fn text_input(text: &str) -> Result<Vec<wire::UserInput>, String> {
    let input = wire::TextUserInput {
        text: text.to_owned(),
        text_elements: None,
        type_: wire::TextUserInputType::Text,
    };
    let input =
        serde_json::to_value(&input).map_err(|err| format!("cannot encode the input: {err}"))?;
    Ok(vec![input])
}

/// `turn/start`'s parameters.
pub(super) fn turn_start_params(
    thread: &ThreadId,
    question: &str,
) -> Result<wire::TurnStartParams, String> {
    // Everything else is the thread's, which `thread/start` or `thread/resume` already set.
    Ok(wire::TurnStartParams {
        thread_id: thread.as_str().to_owned(),
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
    target: TurnTarget<'_>,
    text: &str,
) -> Result<wire::TurnSteerParams, String> {
    Ok(wire::TurnSteerParams {
        thread_id: target.thread.as_str().to_owned(),
        expected_turn_id: target.turn.as_str().to_owned(),
        input: text_input(text)?,
        client_user_message_id: None,
    })
}

/// `turn/interrupt`'s parameters.
pub(super) fn turn_interrupt_params(target: TurnTarget<'_>) -> wire::TurnInterruptParams {
    wire::TurnInterruptParams {
        thread_id: target.thread.as_str().to_owned(),
        turn_id: target.turn.as_str().to_owned(),
    }
}

/// The reason every decline gives.
const UNATTENDED: &str = "agentmux runs consultations unattended, with nobody to approve this";

/// The answer to request `id`, when it is an approval or an input request, or `None` for a
/// request a consultation does not answer at all.
///
/// Each answer is the one the schema documents as "denied, and the turn continues", so the model
/// learns the action was refused rather than the turn ending.
/// Each is built through the method's own binding, so it is always the type that method expects,
/// and every request codex can send is named, so a regeneration that adds one fails to build
/// until it is decided here.
pub(super) fn decline(id: &RequestId, request: Any<'_>) -> Option<Result<Message, EncodeError>> {
    fn answer<M: ServerRequest>(
        id: &RequestId,
        response: &M::Response,
    ) -> Result<Message, EncodeError> {
        Message::result::<M>(id.clone(), response)
    }
    let denied = || {
        wire::ReviewDecision::DeniedReviewDecision(Box::new(wire::DeniedReviewDecision {
            denied: wire::DeniedReviewDecisionDenied {
                rejection: UNATTENDED.to_owned(),
            },
        }))
    };
    Some(match request {
        Any::ItemCommandExecutionRequestApproval(_) => answer::<
            m::ItemCommandExecutionRequestApproval,
        >(
            id,
            &wire::CommandExecutionRequestApprovalResponse {
                decision: wire::CommandExecutionApprovalDecision::CommandExecutionApprovalDecision4(
                    Box::new(wire::CommandExecutionApprovalDecision4::Decline),
                ),
            },
        ),
        Any::ItemFileChangeRequestApproval(_) => answer::<m::ItemFileChangeRequestApproval>(
            id,
            &wire::FileChangeRequestApprovalResponse {
                decision: wire::FileChangeApprovalDecision::Decline,
            },
        ),
        Any::ExecCommandApproval(_) => answer::<m::ExecCommandApproval>(
            id,
            &wire::ExecCommandApprovalResponse { decision: denied() },
        ),
        Any::ApplyPatchApproval(_) => answer::<m::ApplyPatchApproval>(
            id,
            &wire::ApplyPatchApprovalResponse { decision: denied() },
        ),
        Any::McpServerElicitationRequest(_) => answer::<m::McpServerElicitationRequest>(
            id,
            &wire::McpServerElicitationRequestResponse {
                meta: None,
                action: wire::McpServerElicitationAction::Decline,
                content: None,
            },
        ),
        Any::ItemToolRequestUserInput(_) => answer::<m::ItemToolRequestUserInput>(
            id,
            &wire::ToolRequestUserInputResponse {
                answers: std::collections::BTreeMap::new(),
            },
        ),
        // An empty grant: no file system and no network beyond what the sandbox already allows.
        Any::ItemPermissionsRequestApproval(_) => answer::<m::ItemPermissionsRequestApproval>(
            id,
            &wire::PermissionsRequestApprovalResponse {
                permissions: wire::GrantedPermissionProfile {
                    file_system: None,
                    network: None,
                },
                scope: None,
                strict_auto_review: None,
            },
        ),
        // Asks the client for credentials, an attestation, or the result of a tool the client
        // provides, none of which a consultation has.
        Any::AccountChatgptAuthTokensRefresh(_)
        | Any::AttestationGenerate(_)
        | Any::ItemToolCall(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use codex_app_server_protocol::envelope::{Message, RequestId};
    use codex_app_server_protocol::methods::{self as m, AnyServerRequest};
    use googletest::prelude::*;
    use serde_json::{Value, json};

    use super::{ThreadId, TurnId, TurnTarget, decline, turn_steer_params};

    /// A steer names the turn it is for and carries the instruction as one text input.
    #[gtest]
    fn a_steer_encodes_its_turn_precondition_and_text() -> Result<()> {
        let target = TurnTarget {
            thread: &ThreadId::new("th-1"),
            turn: &TurnId::new("tu-2"),
        };
        let params = turn_steer_params(target, "only auth").or_fail()?;
        let line = Message::request::<m::TurnSteer>(RequestId::Integer(7), &params)
            .or_fail()?
            .to_line();
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
        Ok(())
    }

    /// Every decline is the schema's own "denied, carry on" for its request.
    #[gtest]
    fn approvals_are_declined_in_the_schemas_own_words() -> Result<()> {
        let request = |method: &str| AnyServerRequest::classify(method, &Value::Null).or_fail();
        let result = |method: &str| -> Result<Value> {
            match decline(&RequestId::Integer(1), request(method)?)
                .or_fail()?
                .or_fail()?
            {
                Message::Response {
                    outcome: Ok(result),
                    ..
                } => Ok(result),
                other => fail!("not a result: {other:?}").map(|()| Value::Null),
            }
        };
        assert_that!(
            result("item/commandExecution/requestApproval")?,
            eq(&json!({"decision": "decline"}))
        );
        assert_that!(
            result("item/fileChange/requestApproval")?,
            eq(&json!({"decision": "decline"}))
        );
        // The v1 review decision denies with an object that carries the reason.
        assert_that!(
            result("execCommandApproval")?,
            eq(&json!({"decision": {"denied": {"rejection": super::UNATTENDED}}}))
        );
        assert_that!(
            result("mcpServer/elicitation/request")?,
            eq(&json!({"action": "decline"}))
        );
        // A request no consultation could answer is not declined: it is refused outright.
        assert_that!(
            decline(&RequestId::Integer(1), request("item/tool/call")?),
            none()
        );
        Ok(())
    }
}
