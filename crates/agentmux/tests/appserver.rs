//! The Codex app-server driver against an in-process JSON-RPC server: no codex, no network.
//!
//! Each test scripts what the server answers to each message the driver sends, runs the driver on
//! a thread exactly as the `codex-app-server` subcommand would — question in, capture out — and
//! acts on the running turn from outside through the same files the run store uses.
//! The capture is then folded by the ordinary Codex parser, because the point of the driver is
//! that nothing downstream can tell which launch mode wrote it.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use agentmux::appserver::mailbox::{Letter, Mailbox, Reply};
use agentmux::appserver::{Ending, Plan, drive_with};
use agentmux::delegate::{AppServerSettings, Billing, CodexSandbox, ModelId, SessionRef, Vendor};
use agentmux::stream::{self, Fold};
use agentmux::transcript::{MessageSource, Outcome, render_turn};
use googletest::prelude::*;
use serde_json::{Value, json};

const THREAD: &str = "th-fake";
const TURN: &str = "tu-1";

/// Everything the server received, in order.
type Requests = Arc<Mutex<Vec<Value>>>;

/// What the server sends back for one message from the driver.
type Script = Box<dyn FnMut(&Value) -> Vec<Value> + Send>;

/// One finished run of the driver.
struct Run {
    ending: Ending,
    /// The capture the driver wrote, which the store would have folded.
    capture: String,
    requests: Vec<Value>,
}

fn method(message: &Value) -> &str {
    message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn respond(message: &Value, result: &Value) -> Value {
    json!({"id": message.get("id"), "result": result})
}

fn notify(method: &str, params: &Value) -> Value {
    json!({"method": method, "params": params})
}

/// What `thread/start` and `thread/resume` answer: every member codex's schema requires.
///
/// Complete, because the driver decodes it into the generated type, and a fake that left members
/// out would be recorded as protocol drift.
fn thread_opened() -> Value {
    json!({
        "approvalPolicy": "never",
        "approvalsReviewer": "user",
        "cwd": "/work/project",
        "model": "gpt-6-astra",
        "modelProvider": "openai",
        "sandbox": {"type": "readOnly"},
        "thread": {
            "cliVersion": "0.159.3",
            "createdAt": 1_790_000_000,
            "cwd": "/work/project",
            "ephemeral": false,
            "id": THREAD,
            "modelProvider": "openai",
            "preview": "",
            "projectId": null,
            "sessionId": "se-fake",
            "source": "appServer",
            "status": {"type": "idle"},
            "turns": [],
            "updatedAt": 1_790_000_000,
        },
    })
}

/// How the server answers the setup a driver always goes through, or `None` for anything else.
fn setup(message: &Value) -> Option<Vec<Value>> {
    Some(match method(message) {
        "initialize" => vec![respond(
            message,
            &json!({"codexHome": "/home/dev/.codex", "platformFamily": "unix",
                    "platformOs": "linux", "userAgent": "fake"}),
        )],
        "initialized" => Vec::new(),
        "thread/start" | "thread/resume" => vec![respond(message, &thread_opened())],
        "turn/start" => vec![
            respond(
                message,
                &json!({"turn": {"id": TURN, "items": [], "status": "inProgress"}}),
            ),
            notify("turn/started", &turn_in("inProgress")),
        ],
        _ => return None,
    })
}

/// The running turn, as `turn/started` and `turn/completed` carry it, in `status`.
fn turn_in(status: &str) -> Value {
    json!({"threadId": THREAD, "turn": {"id": TURN, "items": [], "status": status}})
}

/// A reply, the token total, and the end of the turn.
fn reply_and_finish(text: &str) -> Vec<Value> {
    vec![
        notify(
            "item/completed",
            &json!({"threadId": THREAD, "turnId": TURN, "completedAtMs": 1,
                   "item": {"id": "i-reply", "type": "agentMessage", "text": text}}),
        ),
        notify(
            "thread/tokenUsage/updated",
            &json!({"threadId": THREAD, "turnId": TURN, "tokenUsage": {
                "last": {"inputTokens": 10, "cachedInputTokens": 0, "outputTokens": 5,
                         "reasoningOutputTokens": 0, "totalTokens": 15},
                "total": {"inputTokens": 900, "cachedInputTokens": 100, "outputTokens": 50,
                          "reasoningOutputTokens": 7, "totalTokens": 950}}}),
        ),
        notify("turn/completed", &turn_in("completed")),
    ]
}

/// A plan whose files live in `dir`.
fn plan(dir: &Path, billing: Billing, resume: Option<&str>) -> Result<Plan> {
    Ok(Plan {
        settings: AppServerSettings {
            program: "codex".to_owned(),
            args: vec!["app-server".to_owned()],
            model: ModelId::parse("gpt-6-astra").or_fail()?,
            sandbox: CodexSandbox::ReadOnly,
        },
        cwd: dir.to_path_buf(),
        resume: resume.map(SessionRef::parse).transpose().or_fail()?,
        billing,
        last_message: dir.join("last-message.md"),
        cancel_marker: dir.join("cancelling"),
        mailbox: dir.join("steer"),
    })
}

/// Run the driver against `script`, while `during` acts on the running turn from outside.
fn run(plan: &Plan, script: Script, during: impl FnOnce(&Plan, &Requests)) -> Result<Run> {
    let (ending, capture, requests) = run_into(plan, script, Vec::new(), during)?;
    Ok(Run {
        ending,
        capture: String::from_utf8(capture).or_fail()?,
        requests,
    })
}

/// [`run`], writing the capture into `out`, which is handed back with what the driver wrote.
fn run_into<W: std::io::Write + Send + 'static>(
    plan: &Plan,
    script: Script,
    out: W,
    during: impl FnOnce(&Plan, &Requests),
) -> Result<(Ending, W, Vec<Value>)> {
    let (server_in, to_server) = std::io::pipe().or_fail()?;
    let (from_server, mut server_out) = std::io::pipe().or_fail()?;
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));

    let server = std::thread::spawn({
        let requests = Arc::clone(&requests);
        let mut script = script;
        move || {
            for line in BufReader::new(server_in).lines() {
                let Ok(line) = line else { return };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                requests
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(message.clone());
                for answer in script(&message) {
                    if writeln!(server_out, "{answer}").is_err() {
                        return;
                    }
                }
            }
        }
    });
    let driver = std::thread::spawn({
        let plan = plan.clone();
        move || {
            let mut out = out;
            let ending = drive_with(
                &plan,
                "Review the auth module.",
                &mut out,
                to_server,
                BufReader::new(from_server),
                &mut || false,
            );
            (ending, out)
        }
    });

    during(plan, &requests);

    let (ending, out) = finished(driver, "the driver")?;
    finished(server, "the fake server")?;
    let requests = requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    Ok((ending, out, requests))
}

/// Join `thread`, failing the test rather than hanging it when it has not finished within a
/// minute.
fn finished<T>(thread: std::thread::JoinHandle<T>, what: &str) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            fail!("{what} did not finish within a minute")?;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    thread.join().ok().or_fail()
}

/// Run the driver on a thread of its own against pipes a fake server already holds the other
/// ends of, failing the test rather than hanging it when the driver does not finish within a
/// minute.
fn drive_bounded<W: std::io::Write + Send + 'static>(
    plan: &Plan,
    out: W,
    to_server: std::io::PipeWriter,
    from_server: std::io::PipeReader,
    mut exited: impl FnMut() -> bool + Send + 'static,
) -> Result<(Ending, W)> {
    let plan = plan.clone();
    let driver = std::thread::spawn(move || {
        let mut out = out;
        let ending = drive_with(
            &plan,
            "q",
            &mut out,
            to_server,
            BufReader::new(from_server),
            &mut exited,
        );
        (ending, out)
    });
    finished(driver, "the driver")
}

/// Wait for `condition`, for at most ten seconds.
fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Submit a steer and wait for the driver's reply.
fn steer(plan: &Plan, text: &str) -> Option<Reply> {
    let mailbox = Mailbox::new(&plan.mailbox);
    let letter: Letter = mailbox.submit(text).ok()?;
    let mut reply = None;
    eventually(|| {
        reply = mailbox.reply(&letter).ok().flatten();
        reply.is_some()
    });
    reply
}

/// The requests the server received with `name`.
fn sent<'a>(run: &'a Run, name: &str) -> Vec<&'a Value> {
    run.requests.iter().filter(|m| method(m) == name).collect()
}

/// One reply the delegate gives while its turn goes on.
fn says(id: &str, text: &str) -> Value {
    notify(
        "item/completed",
        &json!({"threadId": THREAD, "turnId": TURN, "completedAtMs": 1,
               "item": {"id": id, "type": "agentMessage", "text": text}}),
    )
}

/// The kinds of the events in a capture, in order.
fn kinds(capture: &str) -> Vec<String> {
    capture
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| event.get("type").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// A steer reaches the running turn, is answered, and is recorded where the server accepted it.
///
/// The whole path a steer takes: the caller's request in the mailbox, `turn/steer` naming the
/// running turn, the server's acceptance, the capture line, and the rendered transcript, which
/// shows the instruction as a user turn-fragment between what came before it and the reply it
/// produced.
#[gtest]
fn a_steered_turn_records_the_instruction_where_the_server_accepted_it() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        setup(message).map_or_else(
            || match method(message) {
                "turn/steer" => {
                    let mut answers = vec![respond(message, &json!({"turnId": TURN}))];
                    answers.extend(reply_and_finish("Narrowed to auth: no findings."));
                    answers
                }
                _ => Vec::new(),
            },
            |mut answers| {
                // Something already said, so the steer has a place to land after it.
                if method(message) == "turn/start" {
                    answers.push(says("i-before", "Reading the whole repository."));
                }
                answers
            },
        )
    });

    let mut reply = None;
    let run = run(&plan, script, |plan, requests| {
        // Steered once the turn has said something, as a caller reading `tail` would.
        eventually(|| {
            requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .any(|m| method(m) == "turn/start")
        });
        reply = steer(plan, "Only look at the auth module.");
    })?;

    // The caller heard that the instruction was accepted, and the turn then ended normally.
    assert_that!(reply, some(eq(&Reply::Accepted)));
    assert_that!(run.ending, eq(&Ending::Finished));

    // The steer named the turn that was running, in the thread the driver opened.
    let steers = sent(&run, "turn/steer");
    assert_that!(steers.len(), eq(1));
    assert_that!(
        steers.first().and_then(|m| m.get("params")),
        some(eq(&json!({
            "threadId": THREAD,
            "expectedTurnId": TURN,
            "input": [{"type": "text", "text": "Only look at the auth module."}],
        })))
    );
    // The handshake asked the server to leave out the token-by-token deltas.
    let opted_out = sent(&run, "initialize")
        .first()
        .and_then(|m| m.pointer("/params/capabilities/optOutNotificationMethods"))
        .cloned();
    assert_that!(
        opted_out.and_then(|list| list.as_array().cloned()),
        some(contains(eq(&json!("item/agentMessage/delta"))))
    );

    // The capture is an exec stream with the steer in it, in the order things happened.
    assert_that!(
        kinds(&run.capture),
        elements_are![
            eq("thread.started"),
            eq("turn.started"),
            eq("item.completed"),
            eq("agentmux.steer"),
            eq("item.completed"),
            eq("turn.completed"),
        ]
    );

    // Folded like any Codex turn: the instruction is a steer between the two replies, and the
    // usage is the server's running total.
    let Fold { turn, session, .. } =
        stream::fold(Vendor::Codex, 0, "Review the auth module.", &run.capture);
    assert_that!(session.as_ref().map(SessionRef::as_str), some(eq(THREAD)));
    assert_that!(
        turn.messages.iter().map(|m| m.source).collect::<Vec<_>>(),
        elements_are![
            eq(&MessageSource::Delegate),
            eq(&MessageSource::Steer),
            eq(&MessageSource::Delegate)
        ]
    );
    let Outcome::Completed { usage, .. } = &turn.outcome else {
        return fail!("the turn did not complete: {:?}", turn.outcome);
    };
    assert_that!(usage.input_tokens, some(eq(900)));
    assert_that!(usage.reasoning_output_tokens, some(eq(7)));
    assert_that!(turn.unrecognised.is_empty(), eq(true));

    // Rendered, the steer is a marked user fragment between what came before it and the reply
    // it produced.
    let rendered = render_turn(&turn);
    let before_at = rendered
        .find("### assistant\n\nReading the whole repository.")
        .or_fail()?;
    let steer_at = rendered
        .find("### user (steer)\n\nOnly look at the auth module.")
        .or_fail()?;
    let reply_at = rendered
        .find("### assistant\n\nNarrowed to auth: no findings.")
        .or_fail()?;
    assert_that!(before_at < steer_at && steer_at < reply_at, eq(true));

    // And it grows only at its end, cut at any line, as every committed fixture does.
    let lines: Vec<&str> = run.capture.lines().collect();
    let mut previous = String::new();
    for cut in 0..=lines.len() {
        let prefix: String = lines
            .get(..cut)
            .unwrap_or_default()
            .iter()
            .flat_map(|line| [*line, "\n"])
            .collect();
        let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", &prefix);
        let rendered = render_turn(&turn);
        assert_that!(rendered.starts_with(&previous), eq(true), "cut at {cut}");
        previous = rendered;
    }

    // The closing message is written where `--output-last-message` would have put it.
    assert_that!(
        std::fs::read_to_string(&plan.last_message).or_fail()?,
        eq("Narrowed to auth: no findings.")
    );
    Ok(())
}

/// A steer the server refuses is answered as refused and never enters the transcript.
#[gtest]
fn a_steer_the_server_refuses_is_answered_and_not_recorded() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        setup(message).unwrap_or_else(|| match method(message) {
            "turn/steer" => {
                let refusal =
                    json!({"code": -32600, "message": "expected turn tu-0 but tu-1 is active"});
                let mut answers = vec![json!({"id": message.get("id"), "error": refusal})];
                answers.extend(reply_and_finish("done"));
                answers
            }
            _ => Vec::new(),
        })
    });

    let mut reply = None;
    let run = run(&plan, script, |plan, _| {
        reply = steer(plan, "REFUSED-SENTINEL");
    })?;

    assert_that!(
        reply,
        some(matches_pattern!(Reply::Rejected {
            reason: contains_substring("expected turn tu-0")
        }))
    );
    assert_that!(run.ending, eq(&Ending::Finished));
    // Not in the capture under any name, and so in nothing a reader is shown.
    assert_that!(run.capture, not(contains_substring("REFUSED-SENTINEL")));
    let folded = stream::fold(Vendor::Codex, 0, "q", &run.capture);
    assert_that!(
        render_turn(&folded.turn),
        not(contains_substring("REFUSED-SENTINEL"))
    );
    Ok(())
}

/// A cancellation interrupts the turn over the protocol and leaves no terminal event behind.
///
/// The store records the cancellation itself once the driver has gone; a terminal event here
/// would be read first and would settle the turn as something else.
#[gtest]
fn a_cancelled_turn_is_interrupted_over_the_protocol() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        setup(message).unwrap_or_else(|| match method(message) {
            "turn/interrupt" => vec![
                respond(message, &json!({})),
                notify("turn/completed", &turn_in("interrupted")),
            ],
            _ => Vec::new(),
        })
    });

    let run = run(&plan, script, |plan, requests| {
        // Cancelled only once the turn is running, as `cancel` would find it.
        let started = eventually(|| {
            requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .any(|m| method(m) == "turn/start")
        });
        if started {
            let _ = std::fs::write(&plan.cancel_marker, "");
        }
    })?;

    assert_that!(run.ending, eq(&Ending::Cancelled));
    assert_that!(
        sent(&run, "turn/interrupt")
            .first()
            .and_then(|m| m.get("params")),
        some(eq(&json!({"threadId": THREAD, "turnId": TURN})))
    );
    assert_that!(run.capture, not(contains_substring("turn.completed")));
    assert_that!(run.capture, not(contains_substring("turn.failed")));
    Ok(())
}

/// A follow-up resumes its thread, re-pinning the model, the sandbox and the approval policy.
#[gtest]
fn a_follow_up_resumes_its_thread_with_its_settings_pinned() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, Some(THREAD))?;
    let script: Script = Box::new(|message| {
        let mut answers = setup(message).unwrap_or_default();
        if method(message) == "turn/start" {
            answers.extend(reply_and_finish("BANANAPHONE"));
        }
        answers
    });

    let run = run(&plan, script, |_, _| {})?;

    assert_that!(run.ending, eq(&Ending::Finished));
    assert_that!(sent(&run, "thread/start"), is_empty());
    assert_that!(
        sent(&run, "thread/resume")
            .first()
            .and_then(|m| m.get("params")),
        some(eq(&json!({
            "threadId": THREAD,
            "model": "gpt-6-astra",
            "cwd": dir.path().to_string_lossy(),
            "sandbox": "read-only",
            "approvalPolicy": "never",
            "excludeTurns": true,
        })))
    );
    // The resumed thread is announced, which is what the fold checks continuity against.
    let Fold { session, .. } = stream::fold(Vendor::Codex, 1, "q", &run.capture);
    assert_that!(session.as_ref().map(SessionRef::as_str), some(eq(THREAD)));
    Ok(())
}

/// A request for approval is declined at once, and the transcript says so.
///
/// Nobody is there to approve anything, and the decline the schema documents lets the turn
/// continue with the action refused, which is what `codex exec` does with `approval: never`.
#[gtest]
fn an_approval_request_is_declined_and_noted() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        if let Some(mut answers) = setup(message) {
            if method(message) == "turn/start" {
                answers.push(
                    json!({"id": "srv-1", "method": "item/commandExecution/requestApproval",
                    "params": {"threadId": THREAD, "turnId": TURN, "itemId": "c1"}}),
                );
            }
            return answers;
        }
        // The driver's answer to the approval request: the turn carries on and finishes.
        if message.get("id") == Some(&json!("srv-1")) {
            return reply_and_finish("ran without it");
        }
        Vec::new()
    });

    let run = run(&plan, script, |_, _| {})?;

    assert_that!(run.ending, eq(&Ending::Finished));
    let answer = run
        .requests
        .iter()
        .find(|m| m.get("id") == Some(&json!("srv-1")))
        .cloned();
    assert_that!(
        answer.and_then(|m| m.get("result").cloned()),
        some(eq(&json!({"decision": "decline"})))
    );
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", &run.capture);
    assert_that!(
        render_turn(&turn),
        contains_substring("agentmux declined codex's `item/commandExecution/requestApproval`")
    );
    Ok(())
}

/// A server that goes away mid-turn breaks the turn, with an exit status that says so.
///
/// No terminal event is written, so the store settles the turn from the driver's exit and the
/// stderr it leaves, as it would for a `codex exec` that died.
#[gtest]
fn a_server_that_exits_mid_turn_breaks_the_turn() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    // The server answers setup, then exits on the first message after it.
    let script: Script = Box::new(|message| setup(message).unwrap_or_default());
    let (server_in, to_server) = std::io::pipe().or_fail()?;
    let (from_server, mut server_out) = std::io::pipe().or_fail()?;
    let server = std::thread::spawn({
        let mut script = script;
        move || {
            for line in BufReader::new(server_in).lines() {
                let Ok(line) = line else { return };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let started = method(&message) == "turn/start";
                for answer in script(&message) {
                    let _ = writeln!(server_out, "{answer}");
                }
                if started {
                    // Dropping the writer is the server's stdout closing.
                    return;
                }
            }
        }
    });

    let (ending, _) = drive_bounded(&plan, Vec::new(), to_server, from_server, || false)?;
    finished(server, "the fake server")?;

    assert_that!(
        ending,
        matches_pattern!(Ending::Broken(contains_substring("exited while the turn")))
    );
    assert_that!(ending.exit_code(), eq(1));
    Ok(())
}

/// A new thread is opened with the consultation's own settings, persisted so a follow-up can
/// resume it, and the turn starts with the question exactly as it was asked.
#[gtest]
fn a_new_thread_is_opened_with_the_consultations_settings() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        let mut answers = setup(message).unwrap_or_default();
        if method(message) == "turn/start" {
            answers.extend(reply_and_finish("ok"));
        }
        answers
    });

    let run = run(&plan, script, |_, _| {})?;

    assert_that!(run.ending, eq(&Ending::Finished));
    assert_that!(
        sent(&run, "thread/start")
            .first()
            .and_then(|m| m.get("params")),
        some(eq(&json!({
            "model": "gpt-6-astra",
            "cwd": dir.path().to_string_lossy(),
            "sandbox": "read-only",
            // Nobody is there to approve anything, as with `codex exec`.
            "approvalPolicy": "never",
            // Kept, so a follow-up can resume it.
            "ephemeral": false,
        })))
    );
    assert_that!(
        sent(&run, "turn/start")
            .first()
            .and_then(|m| m.get("params")),
        some(eq(&json!({
            "threadId": THREAD,
            "input": [{"type": "text", "text": "Review the auth module."}],
        })))
    );
    Ok(())
}

/// A turn the server finishes before it answers `turn/start` still ends, once, with its reply.
///
/// The server starts the work before it answers, so a fast turn can complete first; the driver
/// must not then wait for a second completion that never comes.
#[gtest]
fn a_turn_that_ends_before_turn_start_answers_still_ends() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        if method(message) == "turn/start" {
            let mut answers = vec![notify("turn/started", &turn_in("inProgress"))];
            answers.extend(reply_and_finish("Done before you asked."));
            answers.push(respond(
                message,
                &json!({"turn": {"id": TURN, "items": [], "status": "inProgress"}}),
            ));
            return answers;
        }
        setup(message).unwrap_or_default()
    });

    let run = run(&plan, script, |_, _| {})?;

    assert_that!(run.ending, eq(&Ending::Finished));
    let ends = kinds(&run.capture)
        .into_iter()
        .filter(|kind| kind == "turn.completed")
        .count();
    assert_that!(ends, eq(1));
    assert_that!(
        std::fs::read_to_string(&plan.last_message).or_fail()?,
        eq("Done before you asked.")
    );
    Ok(())
}

/// A steer the server accepts just as the turn ends is recorded before the end, where the fold
/// still reads it.
#[gtest]
fn a_steer_accepted_as_the_turn_ends_is_recorded_before_its_end() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        setup(message).unwrap_or_else(|| match method(message) {
            // The completion overtakes the steer's own answer.
            "turn/steer" => {
                let mut answers = reply_and_finish("finished anyway");
                answers.push(respond(message, &json!({"turnId": TURN})));
                answers
            }
            _ => Vec::new(),
        })
    });

    let mut reply = None;
    let run = run(&plan, script, |plan, _| {
        reply = steer(plan, "last word");
    })?;

    assert_that!(reply, some(eq(&Reply::Accepted)));
    let order = kinds(&run.capture);
    let steer_at = order.iter().position(|kind| kind == "agentmux.steer");
    let end_at = order.iter().position(|kind| kind == "turn.completed");
    assert_that!(steer_at.zip(end_at).map(|(s, e)| s < e), some(eq(true)));
    Ok(())
}

/// A steer the server never answers is neither accepted nor refused: it was sent, and whether
/// codex took it is unknown, so the caller is told exactly that and nothing is recorded.
#[gtest]
fn a_steer_that_is_never_answered_is_unconfirmed() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        setup(message).unwrap_or_else(|| match method(message) {
            "turn/steer" => reply_and_finish("ended without a word about it"),
            _ => Vec::new(),
        })
    });

    let mut reply = None;
    let run = run(&plan, script, |plan, _| {
        reply = steer(plan, "lost in transit");
    })?;

    assert_that!(reply, some(matches_pattern!(Reply::Unconfirmed { .. })));
    assert_that!(run.capture, not(contains_substring("lost in transit")));
    assert_that!(run.ending, eq(&Ending::Finished));
    Ok(())
}

/// A capture that refuses one line: the transcript the deliverable lives in.
struct RefusesSteers(Vec<u8>);

impl std::io::Write for RefusesSteers {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if String::from_utf8_lossy(buf).contains("agentmux.steer") {
            return Err(std::io::Error::other("disk full"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A steer the capture cannot record is never reported as accepted, and the turn stops, because
/// a transcript that cannot be written is no longer the record of the turn.
#[gtest]
fn a_steer_the_capture_cannot_record_is_never_reported_accepted() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        setup(message).unwrap_or_else(|| match method(message) {
            "turn/steer" => vec![respond(message, &json!({"turnId": TURN}))],
            _ => Vec::new(),
        })
    });

    let mut reply = None;
    let (ending, _, _) = run_into(&plan, script, RefusesSteers(Vec::new()), |plan, _| {
        reply = steer(plan, "unrecordable");
    })?;

    assert_that!(
        reply,
        some(matches_pattern!(Reply::Unconfirmed {
            reason: contains_substring("could not record")
        }))
    );
    assert_that!(
        ending,
        matches_pattern!(Ending::Broken(contains_substring("capture")))
    );
    Ok(())
}

/// A cancellation that arrives while the thread is being opened starts no turn at all.
#[gtest]
fn a_cancellation_during_setup_starts_no_turn() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let marker = plan.cancel_marker.clone();
    let script: Script = Box::new(move |message| {
        if method(message) == "thread/start" {
            // `cancel` marks the turn while the server is still opening the thread.
            let _ = std::fs::write(&marker, "");
        }
        setup(message).unwrap_or_default()
    });

    let run = run(&plan, script, |_, _| {})?;

    assert_that!(run.ending, eq(&Ending::Cancelled));
    assert_that!(sent(&run, "turn/start"), is_empty());
    Ok(())
}

/// Every request the server can make of a client is answered at once, during setup and while the
/// turn runs, so no turn ever waits on a person who is not there, and every answer refuses: an
/// approval is declined, a permission granted nothing, a question answered with nothing, and a
/// request no client of an unattended turn can serve is an error.
///
/// The driver answers by method alone, so the requests carry no parameters.
#[gtest]
fn every_request_the_server_can_make_is_refused() -> Result<()> {
    let denied = json!({"decision": {"denied": {"rejection":
        "agentmux runs consultations unattended, with nobody to approve this"}}});
    let not_served = json!({"error": {"code": -32601, "message":
        "agentmux runs consultations unattended and answers no requests of this kind"}});
    let kinds: Vec<(&str, Value)> = vec![
        (
            "item/commandExecution/requestApproval",
            json!({"result": {"decision": "decline"}}),
        ),
        (
            "item/fileChange/requestApproval",
            json!({"result": {"decision": "decline"}}),
        ),
        (
            "item/permissions/requestApproval",
            json!({"result": {"permissions": {}}}),
        ),
        ("execCommandApproval", json!({"result": denied})),
        ("applyPatchApproval", json!({"result": denied})),
        (
            "mcpServer/elicitation/request",
            json!({"result": {"action": "decline"}}),
        ),
        (
            "item/tool/requestUserInput",
            json!({"result": {"answers": {}}}),
        ),
        ("item/tool/call", not_served.clone()),
        ("attestation/generate", not_served.clone()),
        ("account/chatgptAuthTokens/refresh", not_served),
    ];
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let methods: Vec<&'static str> = kinds.iter().map(|(kind, _)| *kind).collect();
    let count = methods.len();
    let asked = move |phase: &str| -> Vec<Value> {
        methods
            .iter()
            .map(|kind| json!({"id": format!("{phase}:{kind}"), "method": kind, "params": {}}))
            .collect()
    };
    let mut answered = 0;
    let script: Script = Box::new(move |message| {
        if let Some(mut answers) = setup(message) {
            match method(message) {
                "initialize" => answers.extend(asked("setup")),
                "turn/start" => answers.extend(asked("turn")),
                _ => {}
            }
            return answers;
        }
        // The turn ends once every request has been answered.
        if message.get("id").and_then(Value::as_str).is_some() {
            answered += 1;
            if answered == 2 * count {
                return reply_and_finish("carried on");
            }
        }
        Vec::new()
    });

    let run = run(&plan, script, |_, _| {})?;

    assert_that!(run.ending, eq(&Ending::Finished));
    for phase in ["setup", "turn"] {
        for (kind, expected) in &kinds {
            let id = json!(format!("{phase}:{kind}"));
            let mut answer = run
                .requests
                .iter()
                .find(|m| m.get("id") == Some(&id))
                .cloned()
                .unwrap_or_default();
            if let Some(fields) = answer.as_object_mut() {
                fields.remove("id");
                fields.remove("jsonrpc");
            }
            expect_that!(answer, eq(expected), "{phase}: {kind}");
        }
    }
    Ok(())
}

/// A server that stops reading its input cannot stall the driver: the turn still ends when the
/// server says so, and a steer stuck in the pipe is reported as unconfirmed.
#[gtest]
fn a_server_that_stops_reading_cannot_stall_the_turn() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let (server_in, to_server) = std::io::pipe().or_fail()?;
    let (from_server, mut server_out) = std::io::pipe().or_fail()?;
    let (started_tx, started) = std::sync::mpsc::channel();
    let (go_tx, go) = std::sync::mpsc::channel::<()>();
    let (stop_tx, stop) = std::sync::mpsc::channel::<()>();

    // Answers setup, then reads nothing more until told to, and ends the turn when told to.
    let server = std::thread::spawn(move || {
        let mut lines = BufReader::new(server_in).lines();
        while let Some(Ok(line)) = lines.next() {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            for answer in setup(&message).unwrap_or_default() {
                let _ = writeln!(server_out, "{answer}");
            }
            if method(&message) == "turn/start" {
                break;
            }
        }
        let _ = started_tx.send(());
        let _ = go.recv_timeout(Duration::from_secs(20));
        for answer in reply_and_finish("ended regardless") {
            let _ = writeln!(server_out, "{answer}");
        }
        let _ = stop.recv_timeout(Duration::from_secs(20));
        drop(lines);
    });
    let (done_tx, done) = std::sync::mpsc::channel();
    std::thread::spawn({
        let plan = plan.clone();
        move || {
            let mut capture = Vec::new();
            let ending = drive_with(
                &plan,
                "q",
                &mut capture,
                to_server,
                BufReader::new(from_server),
                &mut || false,
            );
            let _ = done_tx.send(ending);
        }
    });

    started.recv_timeout(Duration::from_secs(10)).or_fail()?;
    // Far more than a pipe holds, so writing it blocks for as long as the server does not read.
    let mailbox = Mailbox::new(&plan.mailbox);
    let letter = mailbox.submit(&"x".repeat(4 << 20)).or_fail()?;
    assert_that!(eventually(|| mailbox.is_claimed(&letter)), eq(true));
    go_tx.send(()).or_fail()?;

    let ending = done.recv_timeout(Duration::from_secs(20)).or_fail()?;
    let reply = mailbox.reply(&letter).or_fail()?;
    let _ = stop_tx.send(());
    let _ = server.join();

    assert_that!(ending, eq(&Ending::Finished));
    assert_that!(reply, some(matches_pattern!(Reply::Unconfirmed { .. })));
    Ok(())
}

/// A turn driven over the app-server reads exactly like the same turn written by `codex exec`:
/// the same rendered transcript, the same outcome and usage, the same session, and nothing
/// either parser fails to recognise.
///
/// Each exec stream is what codex-cli 0.159.3's `codex exec --json` writes for the same
/// notifications (`codex-rs/exec/src/event_processor_with_jsonl_output.rs`), item ids aside.
/// `status`, `list`, `tail` and `result` read the capture only through this fold, so they agree
/// on it when it does; the closing-message file they also read is covered by
/// `a_plan_only_turn_keeps_its_report_for_store_readers`.
#[gtest]
fn an_app_server_turn_reads_like_the_same_turn_from_exec() -> Result<()> {
    let item = |item: Value| {
        notify(
            "item/completed",
            &json!({"threadId": THREAD, "turnId": TURN, "completedAtMs": 1, "item": item}),
        )
    };
    let error = |will_retry: bool, message: &str| {
        notify(
            "error",
            &json!({"threadId": THREAD, "turnId": TURN, "willRetry": will_retry,
                    "error": {"message": message}}),
        )
    };
    let completed_turn = || {
        let mut answers = vec![
            item(json!({"id": "r", "type": "reasoning", "summary": ["thinking"], "content": []})),
            item(
                json!({"id": "c", "type": "commandExecution", "command": "ls",
                "aggregatedOutput": "src", "exitCode": 0, "status": "completed",
                "commandActions": [], "cwd": "/work", "durationMs": 3}),
            ),
            notify(
                "warning",
                &json!({"threadId": THREAD, "message": "heads up"}),
            ),
            error(true, "reconnecting"),
        ];
        answers.extend(reply_and_finish("FINDINGS"));
        answers
    };
    let cases: [(&str, Vec<Value>, &str); 2] = [
        (
            "a completed turn, with a warning and a retried error",
            completed_turn(),
            indoc::indoc! {r#"
                {"type":"thread.started","thread_id":"th-fake"}
                {"type":"turn.started"}
                {"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"thinking"}}
                {"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"src","exit_code":0,"status":"completed"}}
                {"type":"item.completed","item":{"id":"item_2","type":"error","message":"heads up"}}
                {"type":"error","message":"reconnecting"}
                {"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"FINDINGS"}}
                {"type":"turn.completed","usage":{"input_tokens":900,"cached_input_tokens":100,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":7}}
            "#},
        ),
        (
            "a turn that failed on an error it did not retry",
            vec![
                error(false, "model not supported"),
                notify("turn/completed", &turn_in("failed")),
            ],
            indoc::indoc! {r#"
                {"type":"thread.started","thread_id":"th-fake"}
                {"type":"turn.started"}
                {"type":"error","message":"model not supported"}
                {"type":"turn.failed","error":{"message":"model not supported"}}
            "#},
        ),
    ];

    for (case, during, exec) in cases {
        let dir = tempfile::tempdir().or_fail()?;
        let plan = plan(dir.path(), Billing::Subscription, None)?;
        let mut during = Some(during);
        let script: Script = Box::new(move |message| {
            let mut answers = setup(message).unwrap_or_default();
            if method(message) == "turn/start" {
                answers.extend(during.take().unwrap_or_default());
            }
            answers
        });
        let run = run(&plan, script, |_, _| {})?;

        let driven = stream::fold(Vendor::Codex, 0, "q", &run.capture);
        let written = stream::fold(Vendor::Codex, 0, "q", exec);
        expect_that!(
            render_turn(&driven.turn),
            eq(&render_turn(&written.turn)),
            "{case}"
        );
        expect_that!(driven.turn.outcome, eq(&written.turn.outcome), "{case}");
        expect_that!(driven.session, eq(&written.session), "{case}");
        expect_that!(driven.turn.unrecognised.is_empty(), eq(true), "{case}");
        expect_that!(written.turn.unrecognised.is_empty(), eq(true), "{case}");
    }
    Ok(())
}

/// A cancellation while `turn/start` is still unanswered interrupts the turn as soon as its id is
/// known from `turn/started`, rather than waiting on the answer.
#[gtest]
fn a_turn_cancelled_before_turn_start_answers_is_interrupted_by_its_announced_id() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let marker = plan.cancel_marker.clone();
    let script: Script = Box::new(move |message| match method(message) {
        // The turn starts and is announced, but its answer never comes; the caller cancels.
        "turn/start" => {
            let _ = std::fs::write(&marker, "");
            vec![notify("turn/started", &turn_in("inProgress"))]
        }
        "turn/interrupt" => vec![
            respond(message, &json!({})),
            notify("turn/completed", &turn_in("interrupted")),
        ],
        _ => setup(message).unwrap_or_default(),
    });

    // The answer never comes, so a driver that waited for it would never finish, and fail.
    let run = run(&plan, script, |_, _| {})?;

    assert_that!(run.ending, eq(&Ending::Cancelled));
    assert_that!(
        sent(&run, "turn/interrupt")
            .first()
            .and_then(|m| m.get("params")),
        some(eq(&json!({"threadId": THREAD, "turnId": TURN})))
    );
    Ok(())
}

/// A turn whose `turn/start` is never answered is followed from `turn/started` all the same: it
/// is announced as begun, takes a steer exactly once, and ends when the server says so.
///
/// A driver that waited for the answer would serve no mailbox, leave the steer to be withdrawn,
/// and hold the turn until the setup timeout.
#[gtest]
fn a_turn_start_left_unanswered_is_followed_from_turn_started() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| match method(message) {
        "turn/start" => vec![notify("turn/started", &turn_in("inProgress"))],
        "turn/steer" => {
            let mut answers = vec![respond(message, &json!({"turnId": TURN}))];
            answers.extend(reply_and_finish("STEERED"));
            answers
        }
        _ => setup(message).unwrap_or_default(),
    });

    let mut reply = None;
    let run = run(&plan, script, |plan, _| {
        reply = steer(plan, "only auth");
    })?;

    assert_that!(reply, some(eq(&Reply::Accepted)));
    assert_that!(run.ending, eq(&Ending::Finished));
    assert_that!(sent(&run, "turn/steer").len(), eq(1));
    let order = kinds(&run.capture);
    assert_that!(
        order,
        elements_are![
            eq("thread.started"),
            eq("turn.started"),
            eq("agentmux.steer"),
            eq("item.completed"),
            eq("turn.completed"),
        ]
    );
    Ok(())
}

/// A turn the server finishes without ever answering `turn/start` is written at once, not after
/// the setup timeout.
#[gtest]
fn a_turn_finished_before_turn_start_is_ever_answered_ends_at_once() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| match method(message) {
        "turn/start" => reply_and_finish("Done unasked."),
        _ => setup(message).unwrap_or_default(),
    });

    let began = Instant::now();
    let run = run(&plan, script, |_, _| {})?;

    assert_that!(run.ending, eq(&Ending::Finished));
    assert_that!(began.elapsed() < Duration::from_secs(30), eq(true));
    let ends = kinds(&run.capture)
        .into_iter()
        .filter(|kind| kind == "turn.completed")
        .count();
    assert_that!(ends, eq(1));
    Ok(())
}

/// A capture that notes, as the terminal event is written, what the closing-message file holds.
struct WitnessAtEnd {
    capture: Vec<u8>,
    last_message: std::path::PathBuf,
    /// Whether the terminal event has been written.
    ended: bool,
    /// The file's contents when the terminal event was written, if it had any.
    seen: Option<String>,
}

impl std::io::Write for WitnessAtEnd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.ended && String::from_utf8_lossy(buf).contains("turn.completed") {
            self.ended = true;
            self.seen = std::fs::read_to_string(&self.last_message).ok();
        }
        self.capture.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A turn whose only answer is a plan has that plan on disk before its end is written, so a
/// reader that sees the turn end finds the report, and the store renders it exactly once, both
/// when it first recovers it and when the consultation is read again.
///
/// The plan arrives only in `turn/completed`'s items, with no reply of its own, so the capture
/// alone has no report to show.
#[gtest]
fn a_plan_only_turn_keeps_its_report_for_store_readers() -> Result<()> {
    use agentmux::run::{Retention, RunStore, StartRequest};
    use agentmux::testing::{Script as Child, ScriptedLauncher};

    const PLAN: &str = "1. read the auth module\n2. fix the token check";
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(|message| {
        let mut answers = setup(message).unwrap_or_default();
        if method(message) == "turn/start" {
            answers.push(notify(
                "turn/completed",
                &json!({"threadId": THREAD, "turn": {"id": TURN, "status": "completed",
                        "items": [{"id": "p1", "type": "plan", "text": PLAN}]}}),
            ));
        }
        answers
    });
    let witness = WitnessAtEnd {
        capture: Vec::new(),
        last_message: plan.last_message.clone(),
        ended: false,
        seen: None,
    };

    let (ending, witness, _) = run_into(&plan, script, witness, |_, _| {})?;

    assert_that!(ending, eq(&Ending::Finished));
    assert_that!(witness.ended, eq(true));
    assert_that!(witness.seen, some(eq(PLAN)));

    // The store reads the same two files the driver left.
    let state = tempfile::tempdir().or_fail()?;
    let capture = String::from_utf8(witness.capture).or_fail()?;
    let store = RunStore::open(
        state.path(),
        Arc::new(ScriptedLauncher::new([Child::completed(capture)])),
        [("PATH", "/usr/bin"), ("HOME", "/home/dev")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
    )
    .or_fail()?;
    let started = store
        .start(&StartRequest {
            delegate: agentmux::delegate::Delegate::Codex {
                model: ModelId::parse("gpt-6-astra").or_fail()?,
                effort: None,
                sandbox: CodexSandbox::ReadOnly,
                account: None,
                isolation: None,
            },
            question: "q".to_owned(),
            cwd: dir.path().to_path_buf(),
            retention: Retention::Ttl,
            env: std::collections::BTreeMap::new(),
            billing: None,
            codex_exec: false,
        })
        .or_fail()?;
    let turn = state
        .path()
        .join("runs")
        .join(started.run_id.as_str())
        .join("turns/0000");
    std::fs::copy(&plan.last_message, turn.join("last-message.md")).or_fail()?;

    let (_, first) = store.view(&started.run_id, 0, 1_000_000).or_fail()?;
    assert_that!(first.text.matches(PLAN).count(), eq(1));
    let reopened = RunStore::open(
        state.path(),
        Arc::new(ScriptedLauncher::new([])),
        std::collections::BTreeMap::new(),
    )
    .or_fail()?;
    let (_, again) = reopened.view(&started.run_id, 0, 1_000_000).or_fail()?;
    assert_that!(again.text.starts_with(&first.text), eq(true));
    assert_that!(again.text.matches(PLAN).count(), eq(1));
    Ok(())
}

/// Notifications about another thread — an agent the delegate started for itself — are not the
/// consultation's: their replies, errors, usage and even their end leave this turn's transcript,
/// closing message and token total alone.
#[gtest]
fn child_thread_notifications_do_not_contaminate_the_consultation() -> Result<()> {
    const CHILD: &str = "th-child";
    const PLAN: &str = "1. read the auth module\n2. fix the token check";
    let usage = |thread: &str, turn: &str, input: u64| {
        notify(
            "thread/tokenUsage/updated",
            &json!({"threadId": thread, "turnId": turn, "tokenUsage": {
                "last": {"inputTokens": input, "cachedInputTokens": 0, "outputTokens": 1,
                         "reasoningOutputTokens": 0, "totalTokens": input + 1},
                "total": {"inputTokens": input, "cachedInputTokens": 0, "outputTokens": 1,
                          "reasoningOutputTokens": 0, "totalTokens": input + 1}}}),
        )
    };
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let script: Script = Box::new(move |message| {
        let mut answers = setup(message).unwrap_or_default();
        if method(message) == "turn/start" {
            answers.extend([
                usage(THREAD, TURN, 300),
                notify(
                    "item/completed",
                    &json!({"threadId": CHILD, "turnId": "tu-child", "completedAtMs": 1,
                           "item": {"id": "c1", "type": "agentMessage", "text": "CHILD-ANSWER"}}),
                ),
                notify(
                    "error",
                    &json!({"threadId": CHILD, "turnId": "tu-child", "willRetry": false,
                           "error": {"message": "CHILD-ERROR"}}),
                ),
                usage(CHILD, "tu-child", 9_000),
                // Each id on its own: this thread's earlier turn, and another thread that happens
                // to name this turn's id.
                usage(THREAD, "tu-earlier", 7_000),
                notify(
                    "item/completed",
                    &json!({"threadId": CHILD, "turnId": TURN, "completedAtMs": 1,
                           "item": {"id": "c2", "type": "agentMessage", "text": "CHILD-SAME-TURN-ID"}}),
                ),
                notify(
                    "turn/completed",
                    &json!({"threadId": CHILD,
                           "turn": {"id": "tu-child", "items": [], "status": "failed"}}),
                ),
                notify(
                    "turn/completed",
                    &json!({"threadId": THREAD, "turn": {"id": TURN, "status": "completed",
                            "items": [{"id": "p1", "type": "plan", "text": PLAN}]}}),
                ),
            ]);
        }
        answers
    });

    let run = run(&plan, script, |_, _| {})?;

    // The consultation's own turn ended, once, with its own usage.
    assert_that!(run.ending, eq(&Ending::Finished));
    let ends: Vec<Value> = run
        .capture
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["type"] == "turn.completed")
        .collect();
    assert_that!(ends.len(), eq(1));
    assert_that!(
        ends.first().map(|end| end["usage"]["input_tokens"].clone()),
        some(eq(&json!(300)))
    );
    // Nothing the child said reached the capture, and the plan is the closing message.
    assert_that!(run.capture, not(contains_substring("CHILD-")));
    assert_that!(
        std::fs::read_to_string(&plan.last_message).or_fail()?,
        eq(PLAN)
    );
    Ok(())
}

/// A capture that takes its time over every line, as a slow disk would.
struct SlowCapture(Vec<u8>);

impl std::io::Write for SlowCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }
}

/// A completion the server wrote before it exited ends the turn as completed, however long the
/// messages queued ahead of it take to record.
///
/// The server writes a burst of warnings, the reply and the end, and exits at once; recording each
/// line takes 100 ms, so the end is still queued well after the server is seen to have gone.
#[gtest]
fn a_completion_queued_before_the_server_exits_still_ends_the_turn() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (server_in, to_server) = std::io::pipe().or_fail()?;
    let (from_server, mut server_out) = std::io::pipe().or_fail()?;
    let server = std::thread::spawn({
        let exited = Arc::clone(&exited);
        move || {
            for line in BufReader::new(server_in).lines() {
                let Ok(line) = line else { return };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let mut answers = setup(&message).unwrap_or_default();
                if method(&message) == "turn/start" {
                    for n in 0..8 {
                        answers.push(notify(
                            "warning",
                            &json!({"threadId": THREAD, "message": format!("busy {n}")}),
                        ));
                    }
                    answers.extend(reply_and_finish("QUEUED-ANSWER"));
                }
                for answer in answers {
                    let _ = writeln!(server_out, "{answer}");
                }
                if method(&message) == "turn/start" {
                    exited.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        }
    });

    let (ending, capture) = drive_bounded(
        &plan,
        SlowCapture(Vec::new()),
        to_server,
        from_server,
        move || exited.load(std::sync::atomic::Ordering::Acquire),
    )?;
    finished(server, "the fake server")?;

    assert_that!(ending, eq(&Ending::Finished));
    let capture = String::from_utf8(capture.0).or_fail()?;
    assert_that!(capture, contains_substring("QUEUED-ANSWER"));
    let ends = kinds(&capture)
        .into_iter()
        .filter(|kind| kind == "turn.completed")
        .count();
    assert_that!(ends, eq(1));
    Ok(())
}

/// A plan-only answer the closing-message file cannot hold is kept in the capture as the reply,
/// so the turn still ends with its report readable.
#[gtest]
fn a_plan_report_survives_a_closing_message_that_cannot_be_written() -> Result<()> {
    const PLAN: &str = "1. read the auth module\n2. fix the token check";
    let dir = tempfile::tempdir().or_fail()?;
    let mut plan = plan(dir.path(), Billing::Subscription, None)?;
    // A directory that does not exist, so the file cannot be created.
    plan.last_message = dir.path().join("missing/last-message.md");
    let script: Script = Box::new(|message| {
        let mut answers = setup(message).unwrap_or_default();
        if method(message) == "turn/start" {
            answers.push(notify(
                "turn/completed",
                &json!({"threadId": THREAD, "turn": {"id": TURN, "status": "completed",
                        "items": [{"id": "p1", "type": "plan", "text": PLAN}]}}),
            ));
        }
        answers
    });

    let run = run(&plan, script, |_, _| {})?;

    assert_that!(run.ending, eq(&Ending::Finished));
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", &run.capture);
    assert_that!(render_turn(&turn).matches(PLAN).count(), eq(1));
    assert_that!(turn.outcome, matches_pattern!(Outcome::Completed { .. }));
    Ok(())
}

/// A turn whose server dies after the plan arrived still leaves the plan where a failed turn's
/// reply is recovered from.
#[gtest]
fn a_completed_plan_survives_a_server_that_dies() -> Result<()> {
    const PLAN: &str = "1. read the auth module\n2. fix the token check";
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let (server_in, to_server) = std::io::pipe().or_fail()?;
    let (from_server, mut server_out) = std::io::pipe().or_fail()?;
    let server = std::thread::spawn(move || {
        for line in BufReader::new(server_in).lines() {
            let Ok(line) = line else { return };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            for answer in setup(&message).unwrap_or_default() {
                let _ = writeln!(server_out, "{answer}");
            }
            if method(&message) == "turn/start" {
                let item = notify(
                    "item/completed",
                    &json!({"threadId": THREAD, "turnId": TURN, "completedAtMs": 1,
                           "item": {"id": "p1", "type": "plan", "text": PLAN}}),
                );
                let _ = writeln!(server_out, "{item}");
                // Dropping the writer is the server's stdout closing, with the turn unfinished.
                return;
            }
        }
    });

    let (ending, _) = drive_bounded(&plan, Vec::new(), to_server, from_server, || false)?;
    finished(server, "the fake server")?;

    assert_that!(ending, matches_pattern!(Ending::Broken(_)));
    assert_that!(
        std::fs::read_to_string(&plan.last_message).or_fail()?,
        eq(PLAN)
    );
    Ok(())
}

/// A capture that refuses the line carrying a reply and takes every other.
struct RefusesReplies(Vec<u8>);

impl std::io::Write for RefusesReplies {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if String::from_utf8_lossy(buf).contains("agent_message") {
            return Err(std::io::Error::other("disk full, for now"));
        }
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A plan-only answer that neither its file nor the capture could keep does not end as a
/// completed turn with nothing in it: the turn breaks, and says so.
#[gtest]
fn a_plan_nothing_could_keep_breaks_the_turn() -> Result<()> {
    const PLAN: &str = "1. read the auth module\n2. fix the token check";
    let dir = tempfile::tempdir().or_fail()?;
    let mut plan = plan(dir.path(), Billing::Subscription, None)?;
    plan.last_message = dir.path().join("missing/last-message.md");
    let script: Script = Box::new(|message| {
        let mut answers = setup(message).unwrap_or_default();
        if method(message) == "turn/start" {
            answers.push(notify(
                "turn/completed",
                &json!({"threadId": THREAD, "turn": {"id": TURN, "status": "completed",
                        "items": [{"id": "p1", "type": "plan", "text": PLAN}]}}),
            ));
        }
        answers
    });

    let (ending, capture, _) = run_into(&plan, script, RefusesReplies(Vec::new()), |_, _| {})?;

    assert_that!(ending, matches_pattern!(Ending::Broken(_)));
    let capture = String::from_utf8(capture.0).or_fail()?;
    assert_that!(capture, not(contains_substring("turn.completed")));
    Ok(())
}

/// Output something a dead server started keeps writing faster than the capture records it
/// cannot hold the turn open: what was read before the server was seen gone is drained, and the
/// turn then ends as broken.
#[gtest]
fn a_dead_servers_continuous_output_cannot_extend_the_drain() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (server_in, to_server) = std::io::pipe().or_fail()?;
    let (from_server, mut server_out) = std::io::pipe().or_fail()?;
    let server = std::thread::spawn({
        let exited = Arc::clone(&exited);
        move || {
            let mut lines = BufReader::new(server_in).lines();
            while let Some(Ok(line)) = lines.next() {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                for answer in setup(&message).unwrap_or_default() {
                    let _ = writeln!(server_out, "{answer}");
                }
                if method(&message) == "turn/start" {
                    break;
                }
            }
            // The server is gone; what it started keeps writing until nobody reads.
            exited.store(true, std::sync::atomic::Ordering::Release);
            let chatter = notify(
                "warning",
                &json!({"threadId": THREAD, "message": "still here"}),
            );
            while writeln!(server_out, "{chatter}").is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    });

    let (ending, _) = drive_bounded(
        &plan,
        SlowCapture(Vec::new()),
        to_server,
        from_server,
        move || exited.load(std::sync::atomic::Ordering::Acquire),
    )?;
    finished(server, "the fake server")?;

    assert_that!(
        ending,
        matches_pattern!(Ending::Broken(contains_substring("exited")))
    );
    Ok(())
}

/// A capture that refuses the first terminal event written to it and takes everything else.
struct RefusesFirstEnd {
    capture: Vec<u8>,
    refused: bool,
}

impl std::io::Write for RefusesFirstEnd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.refused && String::from_utf8_lossy(buf).contains("turn.completed") {
            self.refused = true;
            return Err(std::io::Error::other("disk full, for now"));
        }
        self.capture.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A plan kept in the capture because its file could not be written is kept there once, even
/// when the turn then breaks writing its end.
#[gtest]
fn a_plan_fallback_is_recorded_once_if_the_end_cannot_be_written() -> Result<()> {
    const PLAN: &str = "1. read the auth module\n2. fix the token check";
    let dir = tempfile::tempdir().or_fail()?;
    let mut plan = plan(dir.path(), Billing::Subscription, None)?;
    plan.last_message = dir.path().join("missing/last-message.md");
    let script: Script = Box::new(|message| {
        let mut answers = setup(message).unwrap_or_default();
        if method(message) == "turn/start" {
            answers.push(notify(
                "turn/completed",
                &json!({"threadId": THREAD, "turn": {"id": TURN, "status": "completed",
                        "items": [{"id": "p1", "type": "plan", "text": PLAN}]}}),
            ));
        }
        answers
    });
    let capture = RefusesFirstEnd {
        capture: Vec::new(),
        refused: false,
    };

    let (ending, capture, _) = run_into(&plan, script, capture, |_, _| {})?;

    assert_that!(ending, matches_pattern!(Ending::Broken(_)));
    let capture = String::from_utf8(capture.capture).or_fail()?;
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", &capture);
    assert_that!(render_turn(&turn).matches(PLAN).count(), eq(1));
    Ok(())
}
