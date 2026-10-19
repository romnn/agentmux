//! Every fixture is a payload codex's own schema allows, which the generated types alone cannot
//! promise: progeny's header in the protocol crate lists the constraints they do not express.

use codex_app_server_mock::conformance::{Side, check, check_definition};
use codex_app_server_mock::fixtures::{self, THREAD_ID, TURN_ID, TokenCounts};
use codex_app_server_protocol::types::TurnStatus;
use googletest::prelude::*;
use serde::Serialize;

fn conforms(definition: &str, payload: &impl Serialize) -> Result<()> {
    let payload = serde_json::to_value(payload).or_fail()?;
    check_definition(definition, &payload).or_fail()
}

/// Each builder against the definition of what it builds.
#[gtest]
fn every_fixture_conforms_to_codexs_schema() -> Result<()> {
    let tokens = TokenCounts {
        input: 3,
        cached_input: 1,
        output: 2,
        reasoning_output: 1,
    };
    let items = [
        fixtures::agent_message("i", "text"),
        fixtures::plan("p", "1. a"),
        fixtures::reasoning("r", &["thinking"]),
        fixtures::command("c", "ls", "src", 0),
    ];
    conforms("InitializeResponse", &fixtures::initialize_response())?;
    conforms(
        "ThreadStartResponse",
        &fixtures::thread_start_response(THREAD_ID),
    )?;
    conforms(
        "ThreadResumeResponse",
        &fixtures::thread_resume_response(THREAD_ID),
    )?;
    conforms("TurnStartResponse", &fixtures::turn_start_response(TURN_ID))?;
    conforms(
        "TurnStartedNotification",
        &fixtures::turn_started(THREAD_ID, TURN_ID),
    )?;
    conforms(
        "TurnCompletedNotification",
        &fixtures::turn_completed(THREAD_ID, TURN_ID, TurnStatus::Completed, items.to_vec()),
    )?;
    conforms("TurnSteerResponse", &fixtures::turn_steer_response(TURN_ID))?;
    conforms(
        "TurnInterruptResponse",
        &fixtures::turn_interrupt_response(),
    )?;
    conforms(
        "TurnInterruptParams",
        &fixtures::turn_interrupt_params(THREAD_ID, TURN_ID),
    )?;
    for item in items {
        conforms(
            "ItemCompletedNotification",
            &fixtures::item_completed(THREAD_ID, TURN_ID, item),
        )?;
    }
    conforms(
        "ThreadTokenUsageUpdatedNotification",
        &fixtures::token_usage(THREAD_ID, TURN_ID, tokens, tokens),
    )?;
    conforms(
        "WarningNotification",
        &fixtures::warning(THREAD_ID, "careful"),
    )?;
    conforms(
        "ErrorNotification",
        &fixtures::error(THREAD_ID, TURN_ID, true, "retrying"),
    )?;
    conforms(
        "CommandExecutionRequestApprovalParams",
        &fixtures::command_approval_params(THREAD_ID, TURN_ID, "c"),
    )
}

/// A payload the schema refuses is refused, naming where it breaks it.
#[gtest]
fn a_payload_the_schema_refuses_is_refused() {
    let refused = check_definition("TurnSteerResponse", &serde_json::json!({"turnId": 7}));
    assert_that!(refused, err(contains_substring("turnId")));
}

/// What is not a message codex could exchange is refused, whoever sends it: not an object, no
/// method or id, a method that is not a string, and an answer with both or neither outcome.
#[gtest]
fn what_is_no_message_is_refused() {
    for line in [
        "null",
        r#"{"method":null}"#,
        r#"{"id":1}"#,
        r#"{"id":1,"result":{},"error":{"code":1,"message":"both"}}"#,
        r#"{"id":1,"method":null,"result":null}"#,
        r#"{"id":1,"method":7}"#,
    ] {
        let message: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
        for side in [Side::Client, Side::Server] {
            expect_that!(check(side, &message), err(anything()), "{line}");
        }
    }
}
