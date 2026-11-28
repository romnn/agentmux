//! The pi driver against a scripted pi: no pi, no model.
//!
//! Each test answers the driver's commands with a script on a thread of its own, connected to the
//! driver by OS pipes, runs the driver on another thread as the `pi-rpc` subcommand would, and
//! acts on the running turn from outside through the files the run store uses.
//! The happy path is the recorded capture's, which the conformance suite and the fold tests read;
//! these are the races and refusals no real run reproduces on demand.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::sync::mpsc::channel;
use std::time::Duration;

use agentmux::delegate::{Billing, Effort, ModelId, SessionRef};
use agentmux::driver::mailbox::{Mailbox, SteerReply};
use agentmux::driver::{DriverOutcome, DriverTimeouts};
use agentmux::vendor::pi::driver::{PiPlan, PiSession, drive_with};
use googletest::prelude::*;
use serde_json::{Value, json};

/// The session the scripted pi runs.
const SESSION: &str = "01a222df-e34a-76d1-9e3f-380c59ae8f6d";

/// A reply that makes the scripted pi close its output, as a pi that dies does.
const HANG_UP: &str = "hang up";

/// What a script writes back for one command it received.
type Script = Box<dyn FnMut(&Value) -> Vec<Value> + Send>;

/// One finished run of the driver.
struct DriverRun {
    outcome: DriverOutcome,
    /// The capture the driver wrote.
    capture: String,
    /// Every command pi received, in order.
    commands: Vec<Value>,
}

impl DriverRun {
    /// The `type` of every command pi received, in order.
    fn command_types(&self) -> Vec<String> {
        self.commands
            .iter()
            .filter_map(|command| command["type"].as_str().map(str::to_owned))
            .collect()
    }

    /// The capture's lines of `kind`.
    fn captured(&self, kind: &str) -> Vec<Value> {
        self.capture
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["type"] == kind)
            .collect()
    }
}

/// A plan for the model `ollama/qwen`, whose files live in `dir`.
fn plan(dir: &Path) -> Result<PiPlan> {
    Ok(PiPlan {
        program: "pi".to_owned(),
        args: Vec::new(),
        model: ModelId::parse("ollama/qwen").or_fail()?,
        effort: None,
        session: PiSession {
            file: dir.join("session.jsonl"),
            expect_id: None,
        },
        cwd: dir.to_path_buf(),
        billing: Billing::Subscription,
        turn_dir: dir.to_path_buf(),
    })
}

/// pi's answer to `command`, with `data`.
fn answer(command: &Value, data: &Value) -> Value {
    json!({
        "id": command["id"],
        "type": "response",
        "command": command["type"],
        "success": true,
        "data": data,
    })
}

/// pi's refusal of `command`.
fn refuse(command: &Value, error: &str) -> Value {
    json!({
        "id": command["id"],
        "type": "response",
        "command": command["type"],
        "success": false,
        "error": error,
    })
}

/// pi answering the setup with one model, `ollama/qwen`, at the thinking level `thinking`, and
/// starting the run that `prompt` asks for, echoing the question as pi does; every other command
/// goes to `rest`.
fn set_up_then(
    thinking: &'static str,
    mut rest: impl FnMut(&Value) -> Vec<Value> + Send + 'static,
) -> Script {
    Box::new(move |command| match command["type"].as_str() {
        Some("get_state") => vec![answer(
            command,
            &json!({"sessionId": SESSION, "thinkingLevel": thinking}),
        )],
        Some("get_available_models") => vec![answer(
            command,
            &json!({"models": [{"provider": "ollama", "id": "qwen"}]}),
        )],
        Some("set_model" | "set_thinking_level") => vec![answer(command, &Value::Null)],
        Some("prompt") => vec![
            answer(command, &json!({"disposition": "started"})),
            json!({"type": "agent_start"}),
            json!({"type": "message_start", "message": {"role": "user", "content": "q"}}),
        ],
        _ => rest(command),
    })
}

/// Run the driver against `script` with short deadlines, as the `pi-rpc` subcommand would.
///
/// pi's side ends when the driver closes its input, as pi's does.
fn run(plan: &PiPlan, mut script: Script) -> Result<DriverRun> {
    let (pi_in, to_pi) = std::io::pipe().or_fail()?;
    let (from_pi, mut pi_out) = std::io::pipe().or_fail()?;
    let (received, commands) = channel();
    let pi = std::thread::spawn(move || {
        for line in BufReader::new(pi_in).lines() {
            let Ok(command) = line
                .map_err(drop)
                .and_then(|line| serde_json::from_str::<Value>(&line).map_err(drop))
            else {
                return;
            };
            for reply in script(&command) {
                if reply == HANG_UP || writeln!(pi_out, "{reply}").is_err() {
                    return;
                }
            }
            if received.send(command).is_err() {
                return;
            }
        }
    });
    let mut capture = Vec::new();
    let outcome = drive_with(
        plan,
        "q",
        &mut capture,
        to_pi,
        BufReader::new(from_pi),
        &mut || false,
        DriverTimeouts {
            setup_response: Duration::from_secs(1),
            interrupt_completion: Duration::from_secs(2),
            steer_settlement: Duration::from_millis(300),
        },
    );
    pi.join().ok().or_fail()?;
    Ok(DriverRun {
        outcome,
        capture: String::from_utf8(capture).or_fail()?,
        commands: commands.try_iter().collect(),
    })
}

/// Wait for the driver's reply to every steer in `mailbox`, in the order they were submitted.
fn replies(
    mailbox: &Mailbox,
    ids: &[agentmux::driver::mailbox::SteerId],
) -> Result<Vec<SteerReply>> {
    ids.iter()
        .map(|id| mailbox.reply(id).or_fail()?.or_fail())
        .collect()
}

/// Three steers wait when the turn starts, and pi answers each its own way: it takes the first,
/// queues the second without taking it before the turn settles, and settles before it answers the
/// third.
/// Only the taken steer is in the capture and accepted; the other two never reached the turn and
/// are rejected.
/// The effort pi did not take is a warning in the capture.
#[gtest]
fn steers_race_the_settlement() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let mut plan = plan(dir.path())?;
    plan.effort = Some(Effort::parse("xhigh").or_fail()?);
    let mailbox = Mailbox::new(dir.path().join("steer"));
    let ids = ["take this", "hold this", "lose this"]
        .map(|text| mailbox.submit(text))
        .into_iter()
        .collect::<std::io::Result<Vec<_>>>()
        .or_fail()?;
    let mut steers = 0;
    let script = set_up_then("high", move |command| {
        if command["type"] != "steer" {
            return Vec::new();
        }
        steers += 1;
        match steers {
            1 => vec![
                answer(command, &json!({"disposition": "queued"})),
                json!({
                    "type": "message_start",
                    "message": {"role": "user", "content": "take this"},
                }),
            ],
            2 => vec![answer(command, &json!({"disposition": "queued"}))],
            _ => vec![json!({"type": "agent_settled", "aborted": false})],
        }
    });

    let driven = run(&plan, script)?;

    expect_that!(driven.outcome, eq(&DriverOutcome::TurnEnded));
    expect_that!(
        replies(&mailbox, &ids)?,
        elements_are![
            eq(&SteerReply::Accepted),
            matches_pattern!(SteerReply::Rejected { .. }),
            matches_pattern!(SteerReply::Rejected { .. })
        ]
    );
    expect_that!(
        driven.captured("agentmux.steer"),
        elements_are![eq(&json!({"type": "agentmux.steer", "text": "take this"}))]
    );
    expect_that!(driven.captured("agent_settled").len(), eq(1));
    let warnings: Vec<String> = driven
        .captured("agentmux.warning")
        .iter()
        .map(|warning| warning["message"].to_string())
        .collect();
    expect_that!(
        warnings,
        elements_are![contains_substring("`high`, not the `xhigh`")]
    );
    Ok(())
}

/// A cancellation seen before the question is sent stops the setup: no prompt reaches pi, and the
/// turn ends cancelled.
#[gtest]
fn a_cancel_before_the_prompt_sends_nothing_more() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path())?;
    let marker = dir.path().join("cancelling");
    let script = set_up_then("medium", |_| Vec::new());
    let script: Script = {
        let mut script = script;
        Box::new(move |command| {
            // The caller cancels while pi lists its models.
            if command["type"] == "get_available_models" && std::fs::write(&marker, "").is_err() {
                return Vec::new();
            }
            script(command)
        })
    };

    let driven = run(&plan, script)?;

    expect_that!(driven.outcome, eq(&DriverOutcome::Cancelled));
    expect_that!(
        driven.command_types(),
        elements_are![eq("get_state"), eq("get_available_models")]
    );
    Ok(())
}

/// A cancellation seen while pi checks the question waits for its answer: the run that started is
/// aborted, and its end is left to the store, so the capture has no `agent_settled`.
#[gtest]
fn a_cancel_during_preflight_aborts_the_run_that_started() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path())?;
    let marker = dir.path().join("cancelling");
    let set_up = set_up_then("medium", |command| match command["type"].as_str() {
        Some("abort") => vec![
            json!({"type": "agent_settled", "aborted": true}),
            answer(command, &Value::Null),
        ],
        _ => Vec::new(),
    });
    let script: Script = {
        let mut set_up = set_up;
        Box::new(move |command| {
            if command["type"] == "prompt" {
                // The caller cancels before pi has said whether it started the run.
                if std::fs::write(&marker, "").is_err() {
                    return Vec::new();
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            set_up(command)
        })
    };

    let driven = run(&plan, script)?;

    expect_that!(driven.outcome, eq(&DriverOutcome::Cancelled));
    expect_that!(driven.command_types().last(), some(eq("abort")));
    expect_that!(driven.captured("agent_settled"), is_empty());
    Ok(())
}

/// A question pi refuses fails the turn with pi's reason, which the store reads from stderr.
#[gtest]
fn a_refused_prompt_fails_with_pis_reason() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let set_up = set_up_then("medium", |_| Vec::new());
    let script: Script = {
        let mut set_up = set_up;
        Box::new(move |command| match command["type"].as_str() {
            Some("prompt") => vec![refuse(command, "No API key found for ollama")],
            _ => set_up(command),
        })
    };

    let driven = run(&plan(dir.path())?, script)?;

    expect_that!(
        driven.outcome,
        matches_pattern!(DriverOutcome::Failed(contains_substring(
            "No API key found for ollama"
        )))
    );
    Ok(())
}

/// A follow-up whose file holds another session than the one it continues fails before it asks
/// pi anything else.
#[gtest]
fn a_follow_up_into_another_session_fails() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let mut plan = plan(dir.path())?;
    plan.session.expect_id =
        Some(SessionRef::parse("01a22003-2387-70c1-8ca5-f7c07cf68cb0").or_fail()?);

    let driven = run(&plan, set_up_then("medium", |_| Vec::new()))?;

    expect_that!(
        driven.outcome,
        matches_pattern!(DriverOutcome::Failed(contains_substring(
            "not `01a22003-2387-70c1-8ca5-f7c07cf68cb0`"
        )))
    );
    expect_that!(driven.command_types(), elements_are![eq("get_state")]);
    Ok(())
}

/// The model must name exactly one of pi's models as `provider/id`: one it does not name, and one
/// two of them spell alike, both fail naming what pi offers, and no model is set.
#[gtest]
fn a_model_must_name_exactly_one_of_pis() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    for (model, offered) in [
        (
            "ollama/llama",
            json!([{"provider": "ollama", "id": "qwen"}]),
        ),
        (
            "my/slashed/qwen",
            json!([
                {"provider": "my", "id": "slashed/qwen"},
                {"provider": "my/slashed", "id": "qwen"},
            ]),
        ),
    ] {
        let mut plan = plan(dir.path())?;
        plan.model = ModelId::parse(model).or_fail()?;
        let set_up = set_up_then("medium", |_| Vec::new());
        let script: Script = {
            let mut set_up = set_up;
            let offered = offered.clone();
            Box::new(move |command| match command["type"].as_str() {
                Some("get_available_models") => {
                    vec![answer(command, &json!({"models": offered}))]
                }
                _ => set_up(command),
            })
        };

        let driven = run(&plan, script)?;

        expect_that!(
            driven.outcome,
            matches_pattern!(DriverOutcome::Failed(contains_substring("which are: "))),
            "{model}"
        );
        expect_that!(
            driven.command_types(),
            elements_are![eq("get_state"), eq("get_available_models")],
            "{model}"
        );
    }
    Ok(())
}

/// A cancellation that pi's own end overtakes leaves that end in place: the run settled without
/// being aborted, so the turn ended as pi says, not as a cancellation.
#[gtest]
fn a_run_that_settles_on_its_own_while_cancelled_keeps_its_end() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path())?;
    let marker = dir.path().join("cancelling");
    let set_up = set_up_then("medium", |command| match command["type"].as_str() {
        Some("abort") => vec![
            json!({"type": "agent_settled", "aborted": false}),
            answer(command, &Value::Null),
        ],
        _ => Vec::new(),
    });
    let script: Script = {
        let mut set_up = set_up;
        Box::new(move |command| {
            // The caller cancels once the run has started.
            if command["type"] == "prompt" && std::fs::write(&marker, "").is_err() {
                return Vec::new();
            }
            set_up(command)
        })
    };

    let driven = run(&plan, script)?;

    expect_that!(driven.outcome, eq(&DriverOutcome::TurnEnded));
    expect_that!(driven.command_types().last(), some(eq("abort")));
    expect_that!(driven.captured("agent_settled").len(), eq(1));
    Ok(())
}

/// A cancellation before pi has said it started the run sends nothing, because pi takes an
/// `abort` that arrives first on an idle session and starts the run anyway; with no answer within
/// the grace, the turn ends cancelled.
#[gtest]
fn no_abort_is_sent_before_pi_starts_the_run() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path())?;
    let marker = dir.path().join("cancelling");
    let set_up = set_up_then("medium", |_| Vec::new());
    let script: Script = {
        let mut set_up = set_up;
        Box::new(move |command| match command["type"].as_str() {
            // The caller cancels, and pi never answers the question.
            Some("prompt") => {
                if std::fs::write(&marker, "").is_err() {
                    return vec![json!(HANG_UP)];
                }
                Vec::new()
            }
            _ => set_up(command),
        })
    };

    let driven = run(&plan, script)?;

    expect_that!(driven.outcome, eq(&DriverOutcome::Cancelled));
    expect_that!(driven.command_types().last(), some(eq("prompt")));
    Ok(())
}

/// A question pi never answers fails the turn once a setup answer would have.
#[gtest]
fn a_prompt_pi_never_answers_fails_at_its_deadline() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let set_up = set_up_then("medium", |_| Vec::new());
    let script: Script = {
        let mut set_up = set_up;
        Box::new(move |command| match command["type"].as_str() {
            Some("prompt") => Vec::new(),
            _ => set_up(command),
        })
    };

    let driven = run(&plan(dir.path())?, script)?;

    expect_that!(
        driven.outcome,
        matches_pattern!(DriverOutcome::Failed(contains_substring(
            "did not answer `prompt`"
        )))
    );
    Ok(())
}

/// A pi that dies mid-answer, after saving its session, fails the turn, and the capture still
/// names the session, so a follow-up can resume it.
#[gtest]
fn a_pi_that_dies_still_names_the_session_it_saved() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path())?;
    let file = plan.session.file.clone();
    let set_up = set_up_then("medium", |_| Vec::new());
    let script: Script = {
        let mut set_up = set_up;
        Box::new(move |command| match command["type"].as_str() {
            Some("prompt") => {
                let header = json!({"type": "session", "version": 3, "id": SESSION});
                if std::fs::write(&file, format!("{header}\n")).is_err() {
                    return vec![json!(HANG_UP)];
                }
                let mut replies = set_up(command);
                replies.extend([
                    json!({"type": "message_start", "message": {"role": "assistant"}}),
                    json!({
                        "type": "message_update",
                        "assistantMessageEvent": {"type": "text_delta", "delta": "Half"},
                    }),
                    json!(HANG_UP),
                ]);
                replies
            }
            _ => set_up(command),
        })
    };

    let driven = run(&plan, script)?;

    expect_that!(driven.outcome, matches_pattern!(DriverOutcome::Failed(_)));
    expect_that!(
        driven.captured("session"),
        elements_are![eq(&json!({"type": "session", "id": SESSION}))]
    );
    Ok(())
}
