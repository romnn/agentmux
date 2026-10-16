//! The Codex app-server driver against a scripted server: no codex, no network.
//!
//! Each test scripts the server's side of the conversation with a [`Peer`] on a thread of its own
//! — what it expects the driver to send, and what it answers, in order — runs the driver on
//! another thread exactly as the `codex-app-server` subcommand would, question in and capture out,
//! and acts on the running turn from outside through the same files the run store uses.
//! The two are connected by OS pipes, so a server that stops reading fills a real pipe.
//! The capture is then folded by the ordinary Codex parser, because the point of the driver is
//! that nothing downstream can tell which launch mode wrote it.

use std::io::BufReader;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};
use std::time::{Duration, Instant};

use agentmux::appserver::mailbox::{Letter, Mailbox, Reply};
use agentmux::appserver::{Deadlines, Ending, Plan, UNDECODED_EVENT, drive_with};
use agentmux::delegate::{AppServerSettings, Billing, CodexSandbox, ModelId, SessionRef, Vendor};
use agentmux::stream::{self, Fold};
use agentmux::transcript::{MessageSource, Outcome, render_turn};
use codex_app_server_mock::conformance::{self, Side};
use codex_app_server_mock::fixtures::{self, THREAD, TURN, Tokens};
use codex_app_server_mock::{Peer, PeerError};
use codex_app_server_protocol::envelope::{METHOD_NOT_FOUND, Message, RequestId};
use codex_app_server_protocol::methods as m;
use codex_app_server_protocol::types::TurnStatus;
use googletest::prelude::*;
use serde_json::{Value, json};

/// The question every driven turn is asked.
const QUESTION: &str = "Review the auth module.";

/// A server's side of one conversation.
type Script<R> = Box<dyn FnOnce(&mut Peer) -> std::result::Result<R, PeerError> + Send>;

/// One finished run of the driver.
struct Run<R = ()> {
    ending: Ending,
    /// The capture the driver wrote, which the store would have folded.
    capture: String,
    /// Every message the server received, in order.
    requests: Vec<Value>,
    /// What the script returned.
    script: R,
}

/// How the driver runs, beyond the plan: where it captures to, how it learns the server has
/// exited, how long it waits, and how much of the server's output it reads at once.
struct Driver<W> {
    out: W,
    exited: Box<dyn FnMut() -> bool + Send>,
    deadlines: Deadlines,
    read_capacity: usize,
}

impl Driver<Vec<u8>> {
    /// A driver capturing into memory, whose server never exits, on the production deadlines.
    fn standard() -> Self {
        Self::capturing(Vec::new())
    }
}

impl<W> Driver<W> {
    fn capturing(out: W) -> Self {
        Self {
            out,
            exited: Box::new(|| false),
            deadlines: Deadlines::default(),
            read_capacity: 8 << 10,
        }
    }
}

/// One finished run of a [`Driver`], with the capture it was given back.
struct Driven<W, R> {
    ending: Ending,
    out: W,
    requests: Vec<Value>,
    script: R,
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

/// Run the driver against `script`, capturing into memory, while `during` acts on the running
/// turn from outside.
fn run<R: Send + 'static>(
    plan: &Plan,
    script: impl FnOnce(&mut Peer) -> std::result::Result<R, PeerError> + Send + 'static,
    during: impl FnOnce(&Plan),
) -> Result<Run<R>> {
    let driven = run_with(plan, Driver::standard(), script, during)?;
    Ok(Run {
        ending: driven.ending,
        capture: String::from_utf8(driven.out).or_fail()?,
        requests: driven.requests,
        script: driven.script,
    })
}

/// [`run`], with the driver set up as `driver` says.
///
/// Once the script is done, the server keeps the conversation open until the driver closes it, as
/// a server with nothing more to say does; a script that fails closes it at once, so the driver
/// ends, and the script's failure is the test's.
/// Every message the driver sent must be one codex's own schema allows.
fn run_with<W: std::io::Write + Send + 'static, R: Send + 'static>(
    plan: &Plan,
    driver: Driver<W>,
    script: impl FnOnce(&mut Peer) -> std::result::Result<R, PeerError> + Send + 'static,
    during: impl FnOnce(&Plan),
) -> Result<Driven<W, R>> {
    let (server_in, to_server) = std::io::pipe().or_fail()?;
    let (from_server, server_out) = std::io::pipe().or_fail()?;
    let script: Script<R> = Box::new(script);
    let server = std::thread::spawn(move || {
        let mut peer = Peer::new(BufReader::new(server_in), server_out);
        let outcome = script(&mut peer);
        let drained = match &outcome {
            Ok(_) => peer.wait_until_closed(),
            Err(_) => Ok(()),
        };
        let received = peer.received().to_vec();
        (outcome, drained, received)
    });
    let Driver {
        out,
        mut exited,
        deadlines,
        read_capacity,
    } = driver;
    let driving = std::thread::spawn({
        let plan = plan.clone();
        move || {
            let mut out = out;
            let ending = drive_with(
                &plan,
                QUESTION,
                &mut out,
                to_server,
                BufReader::with_capacity(read_capacity, from_server),
                &mut exited,
                deadlines,
            );
            (ending, out)
        }
    });

    during(plan);

    let (ending, out) = finished(driving, "the driver")?;
    let (outcome, drained, received) = finished(server, "the scripted server")?;
    let script = outcome.or_fail()?;
    drained.or_fail()?;
    let mut requests = Vec::new();
    for line in &received {
        let request: Value = serde_json::from_str(line).or_fail()?;
        if let Err(reason) = conformance::check(Side::Client, &request) {
            fail!("the driver sent what codex's schema refuses: {request}\n{reason}")?;
        }
        requests.push(request);
    }
    Ok(Driven {
        ending,
        out,
        requests,
        script,
    })
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

/// Wait for the script to say a point was reached, for at most ten seconds.
fn reached(signal: &Receiver<()>) -> bool {
    signal.recv_timeout(Duration::from_secs(10)).is_ok()
}

/// Say from a script that a point was reached; a test that stopped listening needs no telling.
fn signal(reached: &Sender<()>) {
    reached.send(()).ok();
}

/// Submit a steer and wait for the driver's reply.
fn steer(plan: &Plan, text: &str) -> Option<Reply> {
    let mailbox = Mailbox::new(&plan.mailbox);
    let letter: Letter = mailbox.submit(text).ok()?;
    replied(&mailbox, &letter)
}

/// Wait for the driver's reply to `letter`.
fn replied(mailbox: &Mailbox, letter: &Letter) -> Option<Reply> {
    let mut reply = None;
    eventually(|| {
        reply = mailbox.reply(letter).ok().flatten();
        reply.is_some()
    });
    reply
}

/// The requests the server received with `name`.
fn sent<'a, R>(run: &'a Run<R>, name: &str) -> Vec<&'a Value> {
    run.requests
        .iter()
        .filter(|request| request.get("method").and_then(Value::as_str) == Some(name))
        .collect()
}

/// The kinds of the events in a capture, in order.
fn kinds(capture: &str) -> Vec<String> {
    capture
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| event.get("type").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// How many events of `kind` a capture holds.
fn count(capture: &str, kind: &str) -> usize {
    kinds(capture).iter().filter(|seen| *seen == kind).count()
}

/// One reply the delegate gives while its turn goes on.
fn says(peer: &mut Peer, id: &str, text: &str) -> std::result::Result<(), PeerError> {
    peer.notify::<m::ItemCompleted>(&fixtures::item_completed(
        THREAD,
        TURN,
        fixtures::agent_message(id, text),
    ))
}

/// The delegate's reply, the token total, and the end of the turn, as codex sends them.
fn finish(peer: &mut Peer, text: &str) -> std::result::Result<(), PeerError> {
    says(peer, "i-reply", text)?;
    peer.notify::<m::ThreadTokenUsageUpdated>(&fixtures::token_usage(
        THREAD,
        TURN,
        Tokens {
            input: 10,
            output: 5,
            ..Tokens::default()
        },
        Tokens {
            input: 900,
            cached_input: 100,
            output: 50,
            reasoning_output: 7,
        },
    ))?;
    end(peer, TurnStatus::Completed, Vec::new())
}

/// The end of the turn, in `status`, with the items the turn records.
fn end(
    peer: &mut Peer,
    status: TurnStatus,
    items: Vec<codex_app_server_protocol::types::ThreadItem>,
) -> std::result::Result<(), PeerError> {
    peer.notify::<m::TurnCompleted>(&fixtures::turn_completed(THREAD, TURN, status, items))
}

/// A turn that answers only with a plan, carried in `turn/completed`'s items.
const PLAN: &str = "1. read the auth module\n2. fix the token check";

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
    let (started, turn_running) = channel();

    let mut reply = None;
    let run = run(
        &plan,
        move |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            // Something already said, so the steer has a place to land after it.
            says(peer, "i-before", "Reading the whole repository.")?;
            signal(&started);
            let steer = peer.expect::<m::TurnSteer>()?;
            peer.reply(&steer, &fixtures::turn_steer_response(TURN))?;
            finish(peer, "Narrowed to auth: no findings.")
        },
        |plan| {
            // Steered once the turn has said something, as a caller reading `tail` would.
            if reached(&turn_running) {
                reply = steer(plan, "Only look at the auth module.");
            }
        },
    )?;

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
    let Fold { turn, session, .. } = stream::fold(Vendor::Codex, 0, QUESTION, &run.capture);
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

    let mut reply = None;
    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            let steer = peer.expect::<m::TurnSteer>()?;
            peer.refuse(&steer, -32600, "expected turn tu-0 but tu-1 is active")?;
            finish(peer, "done")
        },
        |plan| reply = steer(plan, "REFUSED-SENTINEL"),
    )?;

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
    let (started, turn_running) = channel();

    let run = run(
        &plan,
        move |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            signal(&started);
            let interrupt = peer.expect::<m::TurnInterrupt>()?;
            peer.reply(&interrupt, &fixtures::turn_interrupt_response())?;
            end(peer, TurnStatus::Interrupted, Vec::new())
        },
        |plan| {
            // Cancelled only once the turn is running, as `cancel` would find it.
            if reached(&turn_running) {
                std::fs::write(&plan.cancel_marker, "").ok();
            }
        },
    )?;

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

    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            finish(peer, "BANANAPHONE")
        },
        |_| {},
    )?;

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

    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            let approval = peer.request::<m::ItemCommandExecutionRequestApproval>(
                RequestId::from("srv-1"),
                &fixtures::command_approval(THREAD, TURN, "c1"),
            )?;
            // The turn carries on once it is answered, and finishes.
            let answer = peer.expect_answer(&approval)?;
            finish(peer, "ran without it")?;
            Ok(answer)
        },
        |_| {},
    )?;

    assert_that!(run.ending, eq(&Ending::Finished));
    // The answer decodes as the method's own response type, and declines.
    assert_that!(run.script, ok(anything()));
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

    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            // Its output closing is the server exiting.
            peer.close();
            Ok(())
        },
        |_| {},
    )?;

    assert_that!(
        run.ending,
        matches_pattern!(Ending::Broken(contains_substring("exited while the turn")))
    );
    assert_that!(run.ending.exit_code(), eq(1));
    Ok(())
}

/// A new thread is opened with the consultation's own settings, persisted so a follow-up can
/// resume it, and the turn starts with the question exactly as it was asked.
#[gtest]
fn a_new_thread_is_opened_with_the_consultations_settings() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            finish(peer, "ok")
        },
        |_| {},
    )?;

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
            "input": [{"type": "text", "text": QUESTION}],
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

    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            let start = peer.expect::<m::TurnStart>()?;
            peer.notify::<m::TurnStarted>(&fixtures::turn_started(THREAD, TURN))?;
            finish(peer, "Done before you asked.")?;
            peer.reply(&start, &fixtures::turn_start_response(TURN))
        },
        |_| {},
    )?;

    assert_that!(run.ending, eq(&Ending::Finished));
    assert_that!(count(&run.capture, "turn.completed"), eq(1));
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

    let mut reply = None;
    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            let steer = peer.expect::<m::TurnSteer>()?;
            // The completion overtakes the steer's own answer.
            finish(peer, "finished anyway")?;
            peer.reply(&steer, &fixtures::turn_steer_response(TURN))
        },
        |plan| reply = steer(plan, "last word"),
    )?;

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

    let mut reply = None;
    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            peer.expect::<m::TurnSteer>()?;
            finish(peer, "ended without a word about it")
        },
        |plan| reply = steer(plan, "lost in transit"),
    )?;

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

    let mut reply = None;
    let driven = run_with(
        &plan,
        Driver::capturing(RefusesSteers(Vec::new())),
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            let steer = peer.expect::<m::TurnSteer>()?;
            peer.reply(&steer, &fixtures::turn_steer_response(TURN))
        },
        |plan| reply = steer(plan, "unrecordable"),
    )?;

    assert_that!(
        reply,
        some(matches_pattern!(Reply::Unconfirmed {
            reason: contains_substring("could not record")
        }))
    );
    assert_that!(
        driven.ending,
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

    let run = run(
        &plan,
        move |peer| {
            let initialize = peer.expect::<m::Initialize>()?;
            peer.reply(&initialize, &fixtures::initialize_response())?;
            peer.expect_notification::<m::Initialized>()?;
            let start = peer.expect::<m::ThreadStart>()?;
            // `cancel` marks the turn while the server is still opening the thread.
            std::fs::write(&marker, "").ok();
            peer.reply(&start, &fixtures::thread_start_response(THREAD))
        },
        |_| {},
    )?;

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
    let ask = move |peer: &mut Peer, phase: &str| -> std::result::Result<(), PeerError> {
        for kind in &methods {
            peer.send(&Message::Request {
                id: RequestId::String(format!("{phase}:{kind}")),
                method: (*kind).to_owned(),
                params: json!({}),
            })?;
        }
        Ok(())
    };
    let asked = kinds.len();

    let run = run(
        &plan,
        move |peer| {
            // Asked as soon as the handshake is answered, so the driver answers while it opens
            // the thread.
            let initialize = peer.expect::<m::Initialize>()?;
            peer.reply(&initialize, &fixtures::initialize_response())?;
            ask(peer, "setup")?;
            let mut answered = 0;
            let mut thread_start = None;
            while answered < asked || thread_start.is_none() {
                match peer.recv()? {
                    Message::Response { .. } => answered += 1,
                    Message::Request { id, method, .. } if method == "thread/start" => {
                        thread_start = Some(id);
                    }
                    _ => {}
                }
            }
            if let Some(id) = thread_start {
                peer.send(&Message::result::<m::ThreadStart>(
                    id,
                    &fixtures::thread_start_response(THREAD),
                )?)?;
            }
            // And again while the turn runs, which ends once every request is answered.
            peer.start_turn(THREAD, TURN)?;
            ask(peer, "turn")?;
            for _ in 0..asked {
                peer.recv()?;
            }
            finish(peer, "carried on")
        },
        |_| {},
    )?;

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
///
/// The peer reads only when its script asks, so a script that stops asking leaves the steer in
/// the pipe until the driver has finished.
#[gtest]
fn a_server_that_stops_reading_cannot_stall_the_turn() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let (server_in, to_server) = std::io::pipe().or_fail()?;
    let (from_server, server_out) = std::io::pipe().or_fail()?;
    let (started_tx, started) = channel();
    let (go_tx, go) = channel::<()>();
    let (stop_tx, stop) = channel::<()>();

    // Answers setup, then reads nothing more until the test has seen the driver finish.
    let server = std::thread::spawn(move || -> std::result::Result<(), PeerError> {
        let mut peer = Peer::new(BufReader::new(server_in), server_out);
        peer.open_thread(THREAD)?;
        peer.start_turn(THREAD, TURN)?;
        signal(&started_tx);
        go.recv_timeout(Duration::from_secs(20)).ok();
        finish(&mut peer, "ended regardless")?;
        stop.recv_timeout(Duration::from_secs(20)).ok();
        Ok(())
    });
    let (done_tx, done) = channel();
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
                Deadlines::default(),
            );
            done_tx.send(ending).ok();
        }
    });

    assert_that!(reached(&started), eq(true));
    // Far more than a pipe holds, so writing it blocks for as long as the server does not read.
    let mailbox = Mailbox::new(&plan.mailbox);
    let letter = mailbox.submit(&"x".repeat(4 << 20)).or_fail()?;
    assert_that!(eventually(|| mailbox.is_claimed(&letter)), eq(true));
    go_tx.send(()).or_fail()?;

    let ending = done.recv_timeout(Duration::from_secs(20)).or_fail()?;
    let reply = mailbox.reply(&letter).or_fail()?;
    stop_tx.send(()).ok();
    finished(server, "the scripted server")?.or_fail()?;

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
    type During = fn(&mut Peer) -> std::result::Result<(), PeerError>;
    let cases: [(&str, During, &str); 2] = [
        (
            "a completed turn, with a warning and a retried error",
            |peer| {
                peer.notify::<m::ItemCompleted>(&fixtures::item_completed(
                    THREAD,
                    TURN,
                    fixtures::reasoning("r", &["thinking"]),
                ))?;
                peer.notify::<m::ItemCompleted>(&fixtures::item_completed(
                    THREAD,
                    TURN,
                    fixtures::command("c", "ls", "src", 0),
                ))?;
                peer.notify::<m::Warning>(&fixtures::warning(THREAD, "heads up"))?;
                peer.notify::<m::Error>(&fixtures::error(THREAD, TURN, true, "reconnecting"))?;
                finish(peer, "FINDINGS")
            },
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
            |peer| {
                peer.notify::<m::Error>(&fixtures::error(
                    THREAD,
                    TURN,
                    false,
                    "model not supported",
                ))?;
                end(peer, TurnStatus::Failed, Vec::new())
            },
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
        let run = run(
            &plan,
            move |peer| {
                peer.open_thread(THREAD)?;
                peer.start_turn(THREAD, TURN)?;
                during(peer)
            },
            |_| {},
        )?;

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

    // The answer never comes, so a driver that waited for it would never finish, and fail.
    let run = run(
        &plan,
        move |peer| {
            peer.open_thread(THREAD)?;
            // The turn starts and is announced, but its answer never comes; the caller cancels.
            peer.expect::<m::TurnStart>()?;
            std::fs::write(&marker, "").ok();
            peer.notify::<m::TurnStarted>(&fixtures::turn_started(THREAD, TURN))?;
            let interrupt = peer.expect::<m::TurnInterrupt>()?;
            peer.reply(&interrupt, &fixtures::turn_interrupt_response())?;
            end(peer, TurnStatus::Interrupted, Vec::new())
        },
        |_| {},
    )?;

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

    let mut reply = None;
    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.expect::<m::TurnStart>()?;
            peer.notify::<m::TurnStarted>(&fixtures::turn_started(THREAD, TURN))?;
            let steer = peer.expect::<m::TurnSteer>()?;
            peer.reply(&steer, &fixtures::turn_steer_response(TURN))?;
            finish(peer, "STEERED")
        },
        |plan| reply = steer(plan, "only auth"),
    )?;

    assert_that!(reply, some(eq(&Reply::Accepted)));
    assert_that!(run.ending, eq(&Ending::Finished));
    assert_that!(sent(&run, "turn/steer").len(), eq(1));
    assert_that!(
        kinds(&run.capture),
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

    let began = Instant::now();
    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.expect::<m::TurnStart>()?;
            finish(peer, "Done unasked.")
        },
        |_| {},
    )?;

    assert_that!(run.ending, eq(&Ending::Finished));
    assert_that!(began.elapsed() < Duration::from_secs(30), eq(true));
    assert_that!(count(&run.capture, "turn.completed"), eq(1));
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

/// The server's side of a turn that answers only with a plan, which arrives in `turn/completed`.
fn plan_only(peer: &mut Peer) -> std::result::Result<(), PeerError> {
    peer.open_thread(THREAD)?;
    peer.start_turn(THREAD, TURN)?;
    end(
        peer,
        TurnStatus::Completed,
        vec![fixtures::plan("p1", PLAN)],
    )
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

    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let witness = WitnessAtEnd {
        capture: Vec::new(),
        last_message: plan.last_message.clone(),
        ended: false,
        seen: None,
    };

    let driven = run_with(&plan, Driver::capturing(witness), plan_only, |_| {})?;
    let witness = driven.out;

    assert_that!(driven.ending, eq(&Ending::Finished));
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
    let usage = |thread: &str, turn: &str, input: i64| {
        let tokens = Tokens {
            input,
            output: 1,
            ..Tokens::default()
        };
        fixtures::token_usage(thread, turn, tokens, tokens)
    };
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let run = run(
        &plan,
        move |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            peer.notify::<m::ThreadTokenUsageUpdated>(&usage(THREAD, TURN, 300))?;
            peer.notify::<m::ItemCompleted>(&fixtures::item_completed(
                CHILD,
                "tu-child",
                fixtures::agent_message("c1", "CHILD-ANSWER"),
            ))?;
            peer.notify::<m::Error>(&fixtures::error(CHILD, "tu-child", false, "CHILD-ERROR"))?;
            peer.notify::<m::ThreadTokenUsageUpdated>(&usage(CHILD, "tu-child", 9_000))?;
            // Each id on its own: this thread's earlier turn, and another thread that happens to
            // name this turn's id.
            peer.notify::<m::ThreadTokenUsageUpdated>(&usage(THREAD, "tu-earlier", 7_000))?;
            peer.notify::<m::ItemCompleted>(&fixtures::item_completed(
                CHILD,
                TURN,
                fixtures::agent_message("c2", "CHILD-SAME-TURN-ID"),
            ))?;
            peer.notify::<m::TurnCompleted>(&fixtures::turn_completed(
                CHILD,
                "tu-child",
                TurnStatus::Failed,
                Vec::new(),
            ))?;
            end(
                peer,
                TurnStatus::Completed,
                vec![fixtures::plan("p1", PLAN)],
            )
        },
        |_| {},
    )?;

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

/// A driver capturing slowly, whose server counts as exited once `exited` is set.
fn slow_until_exited(exited: &Arc<AtomicBool>) -> Driver<SlowCapture> {
    let exited = Arc::clone(exited);
    Driver {
        exited: Box::new(move || exited.load(Ordering::Acquire)),
        ..Driver::capturing(SlowCapture(Vec::new()))
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
    let exited = Arc::new(AtomicBool::new(false));

    let driven = run_with(
        &plan,
        slow_until_exited(&exited),
        move |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            for n in 0..8 {
                peer.notify::<m::Warning>(&fixtures::warning(THREAD, &format!("busy {n}")))?;
            }
            finish(peer, "QUEUED-ANSWER")?;
            exited.store(true, Ordering::Release);
            Ok(())
        },
        |_| {},
    )?;

    assert_that!(driven.ending, eq(&Ending::Finished));
    let capture = String::from_utf8(driven.out.0).or_fail()?;
    assert_that!(capture, contains_substring("QUEUED-ANSWER"));
    assert_that!(count(&capture, "turn.completed"), eq(1));
    Ok(())
}

/// A plan-only answer the closing-message file cannot hold is kept in the capture as the reply,
/// so the turn still ends with its report readable.
#[gtest]
fn a_plan_report_survives_a_closing_message_that_cannot_be_written() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let mut plan = plan(dir.path(), Billing::Subscription, None)?;
    // A directory that does not exist, so the file cannot be created.
    plan.last_message = dir.path().join("missing/last-message.md");

    let run = run(&plan, plan_only, |_| {})?;

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
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            peer.notify::<m::ItemCompleted>(&fixtures::item_completed(
                THREAD,
                TURN,
                fixtures::plan("p1", PLAN),
            ))?;
            // Its output closing is the server dying, with the turn unfinished.
            peer.close();
            Ok(())
        },
        |_| {},
    )?;

    assert_that!(run.ending, matches_pattern!(Ending::Broken(_)));
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
    let dir = tempfile::tempdir().or_fail()?;
    let mut plan = plan(dir.path(), Billing::Subscription, None)?;
    plan.last_message = dir.path().join("missing/last-message.md");

    let driven = run_with(
        &plan,
        Driver::capturing(RefusesReplies(Vec::new())),
        plan_only,
        |_| {},
    )?;

    assert_that!(driven.ending, matches_pattern!(Ending::Broken(_)));
    let capture = String::from_utf8(driven.out.0).or_fail()?;
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
    let exited = Arc::new(AtomicBool::new(false));

    let driven = run_with(
        &plan,
        slow_until_exited(&exited),
        move |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            // The server is gone; what it started keeps writing until nobody reads.
            exited.store(true, Ordering::Release);
            let chatter = fixtures::warning(THREAD, "still here");
            while peer.notify::<m::Warning>(&chatter).is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        },
        |_| {},
    )?;

    assert_that!(
        driven.ending,
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
    let dir = tempfile::tempdir().or_fail()?;
    let mut plan = plan(dir.path(), Billing::Subscription, None)?;
    plan.last_message = dir.path().join("missing/last-message.md");
    let capture = RefusesFirstEnd {
        capture: Vec::new(),
        refused: false,
    };

    let driven = run_with(&plan, Driver::capturing(capture), plan_only, |_| {})?;

    assert_that!(driven.ending, matches_pattern!(Ending::Broken(_)));
    let capture = String::from_utf8(driven.out.capture).or_fail()?;
    let Fold { turn, .. } = stream::fold(Vendor::Codex, 0, "q", &capture);
    assert_that!(render_turn(&turn).matches(PLAN).count(), eq(1));
    Ok(())
}

/// A deadline short enough for a test to wait out; only the one a test reaches is shortened, so a
/// loaded machine cannot fail the steps before it.
const SHORT: Duration = Duration::from_millis(500);

/// A setup request the server never answers breaks the turn at its deadline, naming the request,
/// rather than holding the consultation for ever.
#[gtest]
fn a_setup_request_left_unanswered_breaks_the_turn_at_its_deadline() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let mut driver = Driver::standard();
    driver.deadlines.setup = SHORT;

    let driven = run_with(
        &plan,
        driver,
        |peer| peer.expect::<m::Initialize>().map(|_| ()),
        |_| {},
    )?;

    assert_that!(
        driven.ending,
        matches_pattern!(Ending::Broken(contains_substring(
            "did not answer `initialize` within 500ms"
        )))
    );
    Ok(())
}

/// A setup request the server refuses breaks the turn with the server's own reason, or says it
/// gave none; an answer to a request the driver never made is passed over on the way.
#[gtest]
fn a_refused_setup_request_breaks_the_turn_with_the_servers_reason() -> Result<()> {
    for (resume, reason, said) in [
        (
            None,
            "no such model",
            "refused `thread/start`: no such model",
        ),
        (
            Some(THREAD),
            "",
            "refused `thread/resume`: the server gave no reason",
        ),
    ] {
        let dir = tempfile::tempdir().or_fail()?;
        let plan = plan(dir.path(), Billing::Subscription, resume)?;

        let run = run(
            &plan,
            move |peer| {
                let initialize = peer.expect::<m::Initialize>()?;
                peer.reply(&initialize, &fixtures::initialize_response())?;
                peer.expect_notification::<m::Initialized>()?;
                let Message::Request { id, .. } = peer.recv()? else {
                    return Ok(());
                };
                // An answer to nothing the driver asked.
                peer.send(&Message::Response {
                    id: RequestId::Integer(999),
                    outcome: Ok(json!({})),
                })?;
                peer.send(&Message::error(id, -32600, reason))
            },
            |_| {},
        )?;

        expect_that!(
            run.ending,
            matches_pattern!(Ending::Broken(contains_substring(said))),
            "{said}"
        );
        expect_that!(sent(&run, "turn/start"), is_empty(), "{said}");
    }
    Ok(())
}

/// A cancellation while the turn is being started, before anything has named it, ends the turn
/// within the interrupt's grace without interrupting anything, because there is no turn id to
/// interrupt.
#[gtest]
fn a_cancellation_before_the_turn_is_known_interrupts_nothing() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let mut driver = Driver::standard();
    driver.deadlines.interrupt = SHORT;
    let (asked, starting) = channel();

    let driven = run_with(
        &plan,
        driver,
        move |peer| {
            peer.open_thread(THREAD)?;
            // Neither answered nor announced.
            peer.expect::<m::TurnStart>()?;
            signal(&asked);
            Ok(())
        },
        |plan| {
            if reached(&starting) {
                std::fs::write(&plan.cancel_marker, "").ok();
            }
        },
    )?;

    assert_that!(driven.ending, eq(&Ending::Cancelled));
    let interrupts = driven
        .requests
        .iter()
        .filter(|request| request.get("method") == Some(&json!("turn/interrupt")))
        .count();
    assert_that!(interrupts, eq(0));
    Ok(())
}

/// An interrupt the server refuses is said in the transcript, and the turn the server then ends
/// is still a cancellation.
#[gtest]
fn an_interrupt_the_server_refuses_is_noted() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let (started, turn_running) = channel();

    let run = run(
        &plan,
        move |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            signal(&started);
            let interrupt = peer.expect::<m::TurnInterrupt>()?;
            peer.refuse(&interrupt, -32600, "the turn is already finishing")?;
            end(peer, TurnStatus::Interrupted, Vec::new())
        },
        |plan| {
            if reached(&turn_running) {
                std::fs::write(&plan.cancel_marker, "").ok();
            }
        },
    )?;

    assert_that!(run.ending, eq(&Ending::Cancelled));
    assert_that!(
        run.capture,
        contains_substring("codex refused to interrupt the turn: the turn is already finishing")
    );
    assert_that!(count(&run.capture, "turn.failed"), eq(0));
    Ok(())
}

/// An interrupt the server never answers, nor follows with the turn's end, does not hold the
/// driver: it leaves once the interrupt's grace is over, as a cancellation.
#[gtest]
fn an_interrupt_never_answered_ends_the_turn_at_its_grace() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let mut driver = Driver::standard();
    driver.deadlines.interrupt = SHORT;
    let (started, turn_running) = channel();

    let driven = run_with(
        &plan,
        driver,
        move |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            signal(&started);
            peer.expect::<m::TurnInterrupt>().map(|_| ())
        },
        |plan| {
            if reached(&turn_running) {
                std::fs::write(&plan.cancel_marker, "").ok();
            }
        },
    )?;

    assert_that!(driven.ending, eq(&Ending::Cancelled));
    let capture = String::from_utf8(driven.out).or_fail()?;
    assert_that!(count(&capture, "turn.completed"), eq(0));
    assert_that!(count(&capture, "turn.failed"), eq(0));
    Ok(())
}

/// Two steers in flight at once are each answered with their own outcome, however the server
/// orders its answers, and an answer twice over, or to nothing asked, changes neither.
#[gtest]
fn concurrent_steers_are_each_answered_with_their_own_outcome() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let mut replies = (None, None);
    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            let first = peer.expect::<m::TurnSteer>()?;
            let second = peer.expect::<m::TurnSteer>()?;
            let text = |steer: &codex_app_server_mock::Received<m::TurnSteer>| {
                steer
                    .params
                    .input
                    .first()
                    .and_then(|input| input.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            let (accept, refuse) = if text(&first).as_deref() == Some("KEEP") {
                (first, second)
            } else {
                (second, first)
            };
            // Answered in the opposite order to the asking, the accepted one twice.
            peer.refuse(&refuse, -32600, "one instruction at a time")?;
            peer.reply(&accept, &fixtures::turn_steer_response(TURN))?;
            peer.reply(&accept, &fixtures::turn_steer_response(TURN))?;
            peer.send(&Message::Response {
                id: RequestId::Integer(424_242),
                outcome: Ok(json!({"turnId": TURN})),
            })?;
            finish(peer, "done")
        },
        |plan| {
            let mailbox = Mailbox::new(&plan.mailbox);
            let (Ok(keep), Ok(drop)) = (mailbox.submit("KEEP"), mailbox.submit("DROP")) else {
                return;
            };
            replies = (replied(&mailbox, &keep), replied(&mailbox, &drop));
        },
    )?;

    let (keep, drop) = replies;
    assert_that!(keep, some(eq(&Reply::Accepted)));
    assert_that!(
        drop,
        some(matches_pattern!(Reply::Rejected {
            reason: contains_substring("one instruction at a time")
        }))
    );
    // The accepted one is recorded once, and the refused one not at all.
    assert_that!(count(&run.capture, "agentmux.steer"), eq(1));
    assert_that!(run.capture, contains_substring("KEEP"));
    assert_that!(run.capture, not(contains_substring("DROP")));
    assert_that!(run.ending, eq(&Ending::Finished));
    Ok(())
}

/// A request the server makes while setup is waiting on its own answer is answered at once, and
/// the server's id is never mistaken for the driver's, even when they are the same number.
#[gtest]
fn a_request_during_setup_is_answered_and_its_id_kept_apart() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let run = run(
        &plan,
        |peer| {
            let initialize = peer.expect::<m::Initialize>()?;
            // The same id the driver gave `initialize`.
            let approval = peer.request::<m::ItemCommandExecutionRequestApproval>(
                initialize.id.clone(),
                &fixtures::command_approval(THREAD, TURN, "c0"),
            )?;
            let answer = peer.expect_answer(&approval)?;
            peer.reply(&initialize, &fixtures::initialize_response())?;
            peer.expect_notification::<m::Initialized>()?;
            let start = peer.expect::<m::ThreadStart>()?;
            peer.reply(&start, &fixtures::thread_start_response(THREAD))?;
            peer.start_turn(THREAD, TURN)?;
            finish(peer, "set up")?;
            Ok(answer)
        },
        |_| {},
    )?;

    assert_that!(run.script, ok(anything()));
    assert_that!(run.ending, eq(&Ending::Finished));
    Ok(())
}

/// A request that arrives while the turn's end is held for a steer's answer is still answered,
/// and the steer then still lands before the end.
#[gtest]
fn a_request_while_the_turn_settles_is_still_answered() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let mut reply = None;
    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            let steer = peer.expect::<m::TurnSteer>()?;
            finish(peer, "ended")?;
            let late = RequestId::from("late");
            peer.send(&Message::Request {
                id: late.clone(),
                method: "item/tool/call".to_owned(),
                params: json!({}),
            })?;
            let answer = peer.expect_raw_answer(&late)?;
            peer.reply(&steer, &fixtures::turn_steer_response(TURN))?;
            Ok(answer)
        },
        |plan| reply = steer(plan, "while it settles"),
    )?;

    assert_that!(
        run.script,
        err(field!(
            codex_app_server_protocol::envelope::RpcError.code,
            eq(&METHOD_NOT_FOUND)
        ))
    );
    assert_that!(reply, some(eq(&Reply::Accepted)));
    let order = kinds(&run.capture);
    let steer_at = order.iter().position(|kind| kind == "agentmux.steer");
    let end_at = order.iter().position(|kind| kind == "turn.completed");
    assert_that!(steer_at.zip(end_at).map(|(s, e)| s < e), some(eq(true)));
    Ok(())
}

/// A turn known only from the answer to `turn/start` takes steers, and a `turn/started` that
/// arrives later does not announce it twice.
#[gtest]
fn a_turn_known_from_its_answer_alone_takes_steers() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let mut reply = None;
    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            let start = peer.expect::<m::TurnStart>()?;
            peer.reply(&start, &fixtures::turn_start_response(TURN))?;
            let steer = peer.expect::<m::TurnSteer>()?;
            peer.reply(&steer, &fixtures::turn_steer_response(TURN))?;
            peer.notify::<m::TurnStarted>(&fixtures::turn_started(THREAD, TURN))?;
            finish(peer, "steered")
        },
        |plan| reply = steer(plan, "only auth"),
    )?;

    assert_that!(reply, some(eq(&Reply::Accepted)));
    assert_that!(
        kinds(&run.capture),
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

/// What the server says after the turn ended — another reply, a second end — is not part of the
/// turn: the capture ends once, with the reply that ended it.
#[gtest]
fn what_the_server_says_after_the_end_changes_nothing() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            // A steer still unanswered holds the end, so what follows arrives while it is held.
            let steer = peer.expect::<m::TurnSteer>()?;
            finish(peer, "FIRST")?;
            says(peer, "i-late", "LATE")?;
            end(peer, TurnStatus::Failed, Vec::new())?;
            peer.reply(&steer, &fixtures::turn_steer_response(TURN))
        },
        |plan| {
            steer(plan, "hold the end");
        },
    )?;

    assert_that!(run.ending, eq(&Ending::Finished));
    assert_that!(count(&run.capture, "turn.completed"), eq(1));
    assert_that!(count(&run.capture, "turn.failed"), eq(0));
    assert_that!(run.capture, not(contains_substring("LATE")));
    assert_that!(
        std::fs::read_to_string(&plan.last_message).or_fail()?,
        eq("FIRST")
    );
    Ok(())
}

/// Messages are read whole however the server's output falls apart: half a message in one write,
/// the rest with the next message in another, a reply of several megabytes, and all of it read
/// sixteen bytes at a time.
#[gtest]
fn messages_are_read_whole_however_the_writes_fall() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;
    let large = "y".repeat(3 << 20);
    let mut driver = Driver::standard();
    driver.read_capacity = 16;

    let driven = run_with(
        &plan,
        driver,
        {
            let large = large.clone();
            move |peer| {
                peer.open_thread(THREAD)?;
                peer.start_turn(THREAD, TURN)?;
                let line = |message: Message| format!("{}\n", message.to_line());
                let split = line(Message::notification::<m::ItemCompleted>(
                    &fixtures::item_completed(THREAD, TURN, fixtures::agent_message("a", "SPLIT")),
                )?);
                let joined = line(Message::notification::<m::Warning>(&fixtures::warning(
                    THREAD, "JOINED",
                ))?);
                let (head, tail) = split.split_at(split.len() / 2);
                peer.send_bytes(head.as_bytes())?;
                peer.send_bytes(format!("{tail}{joined}").as_bytes())?;
                finish(peer, &large)
            }
        },
        |_| {},
    )?;

    assert_that!(driven.ending, eq(&Ending::Finished));
    let capture = String::from_utf8(driven.out).or_fail()?;
    assert_that!(capture, contains_substring("SPLIT"));
    assert_that!(capture, contains_substring("JOINED"));
    assert_that!(
        std::fs::read_to_string(&plan.last_message).or_fail()?,
        eq(&large)
    );
    Ok(())
}

/// A message the server's exit cuts off is not read as one, and the turn breaks as one whose
/// server went away.
#[gtest]
fn a_message_cut_off_by_the_servers_exit_breaks_the_turn() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            let end = Message::notification::<m::TurnCompleted>(&fixtures::turn_completed(
                THREAD,
                TURN,
                TurnStatus::Completed,
                Vec::new(),
            ))?
            .to_line();
            let (head, _) = end.split_at(end.len() / 2);
            peer.send_bytes(head.as_bytes())?;
            peer.close();
            Ok(())
        },
        |_| {},
    )?;

    assert_that!(
        run.ending,
        matches_pattern!(Ending::Broken(contains_substring("exited")))
    );
    assert_that!(count(&run.capture, "turn.completed"), eq(0));
    Ok(())
}

/// An answer to `thread/start` that no longer matches the protocol is recorded as drift, and the
/// thread id it still carries is used; one without a thread id breaks the turn, naming why.
#[gtest]
fn a_thread_start_answer_that_drifted_is_drift_but_its_id_is_kept() -> Result<()> {
    for (answer, works) in [
        (json!({"thread": {"id": THREAD}}), true),
        (json!({"thread": {}}), false),
    ] {
        let dir = tempfile::tempdir().or_fail()?;
        let plan = plan(dir.path(), Billing::Subscription, None)?;

        let run = run(
            &plan,
            move |peer| {
                let initialize = peer.expect::<m::Initialize>()?;
                peer.reply(&initialize, &fixtures::initialize_response())?;
                peer.expect_notification::<m::Initialized>()?;
                let start = peer.expect::<m::ThreadStart>()?;
                peer.send(&Message::Response {
                    id: start.id,
                    outcome: Ok(answer),
                })?;
                if works {
                    peer.start_turn(THREAD, TURN)?;
                    finish(peer, "despite it")?;
                }
                Ok(())
            },
            |_| {},
        )?;

        let drift: Vec<Value> = run
            .capture
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| event["type"] == UNDECODED_EVENT)
            .collect();
        if works {
            expect_that!(run.ending, eq(&Ending::Finished));
            expect_that!(
                drift.first().map(|event| event["method"].clone()),
                some(eq(&json!("thread/start")))
            );
        } else {
            expect_that!(
                run.ending,
                matches_pattern!(Ending::Broken(contains_substring("no thread id")))
            );
        }
    }
    Ok(())
}

/// An answer that carries neither a result nor an error acknowledges nothing: the steer it names
/// is neither accepted nor refused, because whether codex took it is still unknown.
#[gtest]
fn an_answer_with_neither_result_nor_error_acknowledges_no_steer() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let plan = plan(dir.path(), Billing::Subscription, None)?;

    let mut reply = None;
    let run = run(
        &plan,
        |peer| {
            peer.open_thread(THREAD)?;
            peer.start_turn(THREAD, TURN)?;
            let steer = peer.expect::<m::TurnSteer>()?;
            peer.send_bytes(format!("{{\"id\":{}}}\n", steer.id).as_bytes())?;
            finish(peer, "ended")
        },
        |plan| reply = steer(plan, "ACKNOWLEDGED-BY-NOTHING"),
    )?;

    assert_that!(reply, some(matches_pattern!(Reply::Unconfirmed { .. })));
    assert_that!(
        run.capture,
        not(contains_substring("ACKNOWLEDGED-BY-NOTHING"))
    );
    Ok(())
}
