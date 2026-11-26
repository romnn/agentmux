//! The driver against an in-process Claude: no claude, no network.
//!
//! Each test plays Claude on a thread of its own — what it expects the driver to write to its
//! stdin, and what it writes back — runs the driver on another thread exactly as the
//! `claude-stream` subcommand would, question in and capture out, and steers the running turn from
//! outside through the same mailbox files the run store uses.
//! The two are connected by OS pipes, so a Claude that stops reading fills a real pipe.
//! The capture is then folded by the ordinary Claude parser, as the run store folds it.

use std::io::{BufRead as _, BufReader, PipeReader, PipeWriter, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use googletest::prelude::*;
use serde_json::{Value, json};

use super::DriverPlan;
use super::driver::{ClaudeProcess, DriverOutcome, DriverTimeouts, drive_with};
use crate::appserver::mailbox::{Mailbox, SteerId, SteerReply};
use crate::delegate::{Billing, Vendor};
use crate::launch::ExitStatus;
use crate::stream::{self, LaunchMode, TurnInput};
use crate::transcript::{Message, MessageSource, Outcome, Usage};

/// The question every driven turn is asked.
const QUESTION: &str = "Review the auth module.";

/// The session every played Claude announces.
const SESSION: &str = "2f1c6e0a-7d4b-4e1a-9c3f-5b8a0d2e6f71";

/// A clean exit.
const EXITED_CLEANLY: ExitStatus = ExitStatus {
    code: Some(0),
    signal: None,
};

/// Claude's side of the pipes.
struct FakeClaude {
    input: BufReader<PipeReader>,
    output: PipeWriter,
}

impl FakeClaude {
    /// The next line the driver wrote to Claude's stdin, or `None` once it closed it.
    fn next_input(&mut self) -> Result<Option<Value>> {
        let mut line = String::new();
        if self.input.read_line(&mut line).or_fail()? == 0 {
            return Ok(None);
        }
        serde_json::from_str(&line).map(Some).or_fail()
    }

    /// Write one line to Claude's stdout.
    fn say(&mut self, line: &Value) -> Result<()> {
        writeln!(self.output, "{line}").or_fail()?;
        self.output.flush().or_fail()
    }
}

/// Claude's process as the driver sees it, and what the driver did to it.
#[derive(Clone, Default)]
struct Process(Arc<Mutex<ProcessState>>);

#[derive(Default)]
struct ProcessState {
    exit: Option<ExitStatus>,
    terminated: bool,
}

impl Process {
    fn state(&self) -> std::sync::MutexGuard<'_, ProcessState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn terminated(&self) -> bool {
        self.state().terminated
    }
}

impl ClaudeProcess for Process {
    fn exit_status(&mut self) -> Option<ExitStatus> {
        self.state().exit
    }

    fn terminate(&mut self) {
        self.state().terminated = true;
    }

    fn kill(&mut self) {
        self.state().terminated = true;
    }
}

/// The capture the driver writes.
#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    /// Holds the driver once it has recorded a line holding this text, until `release` says
    /// so.
    hold: Option<(&'static str, Receiver<()>)>,
}

impl Capture {
    /// A capture that holds the driver once it records a line holding `text`.
    fn holding_at(text: &'static str, release: Receiver<()>) -> Self {
        Self {
            bytes: Vec::new(),
            hold: Some((text, release)),
        }
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some((text, _)) = &self.hold
            && String::from_utf8_lossy(&self.bytes).contains(text)
            && let Some((_, release)) = self.hold.take()
        {
            release.recv_timeout(Duration::from_secs(10)).ok();
        }
        Ok(())
    }
}

/// One finished run of the driver.
struct DriverRun {
    outcome: DriverOutcome,
    /// The capture the driver wrote, which the store would have folded.
    capture: String,
    /// Whether the driver asked Claude to stop.
    terminated: bool,
}

/// What played Claude does: its side of the conversation, returning how it exits.
type ClaudeScript = Box<dyn FnOnce(&mut FakeClaude, &Process) -> Result<ExitStatus> + Send>;

/// A plan whose files live in `dir`.
fn plan(dir: &Path) -> DriverPlan {
    DriverPlan {
        program: "claude".to_owned(),
        args: vec!["--print".to_owned()],
        cwd: dir.to_path_buf(),
        billing: Billing::Subscription,
        turn_dir: dir.to_path_buf(),
    }
}

/// Run the driver against `claude`, while `during` acts on the running turn from outside.
///
/// Claude exits as its script says once the script is done, after closing both its pipes.
fn run(
    dir: &Path,
    timeouts: DriverTimeouts,
    mut capture: Capture,
    claude: impl FnOnce(&mut FakeClaude, &Process) -> Result<ExitStatus> + Send + 'static,
    during: impl FnOnce(&DriverPlan) -> Result<()>,
) -> Result<DriverRun> {
    let plan = plan(dir);
    let (claude_in, to_claude) = std::io::pipe().or_fail()?;
    let (from_claude, claude_out) = std::io::pipe().or_fail()?;
    let process = Process::default();
    let script: ClaudeScript = Box::new(claude);
    let playing = std::thread::spawn({
        let process = process.clone();
        move || {
            let mut fake = FakeClaude {
                input: BufReader::new(claude_in),
                output: claude_out,
            };
            let played = script(&mut fake, &process);
            drop(fake);
            process.state().exit = Some(*played.as_ref().unwrap_or(&ExitStatus::from(1)));
            played
        }
    });
    let driving = std::thread::spawn({
        let plan = plan.clone();
        let mut process = process.clone();
        move || {
            let outcome = drive_with(
                &plan,
                QUESTION,
                &mut capture,
                to_claude,
                BufReader::new(from_claude),
                &mut process,
                timeouts,
            );
            (outcome, capture.bytes)
        }
    });

    let acted = during(&plan);
    let (outcome, capture) = join_with_timeout(driving, "the driver")?;
    join_with_timeout(playing, "the played Claude")??;
    acted?;
    Ok(DriverRun {
        outcome,
        capture: String::from_utf8(capture).or_fail()?,
        terminated: process.terminated(),
    })
}

/// Join `thread`, failing the test rather than hanging it when it has not finished within a
/// minute.
fn join_with_timeout<T>(thread: std::thread::JoinHandle<T>, what: &str) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            fail!("{what} did not finish within a minute")?;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    thread.join().ok().or_fail()
}

/// The driver's reply to `steer_id`, waiting up to ten seconds for it.
fn reply_to(plan: &DriverPlan, steer_id: &SteerId) -> Result<SteerReply> {
    let mailbox = Mailbox::new(plan.mailbox_dir());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(reply) = mailbox.reply(steer_id).or_fail()? {
            return Ok(reply);
        }
        if Instant::now() >= deadline {
            fail!("the driver did not answer the steer within ten seconds")?;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Claude announcing its session.
fn init() -> Value {
    json!({
        "type": "system",
        "subtype": "init",
        "session_id": SESSION,
        "claude_code_version": "2.1.332",
        "apiKeySource": "none",
    })
}

/// Claude echoing an input line it took, as `--replay-user-messages` makes it.
fn replay(input: &Value) -> Value {
    json!({
        "type": "user",
        "message": input.get("message"),
        "session_id": SESSION,
        "parent_tool_use_id": null,
        "uuid": input.get("uuid"),
        "isReplay": true,
    })
}

fn assistant(text: &str) -> Value {
    json!({
        "type": "assistant",
        "message": {"model": "claude-haiku-5-5", "content": [{"type": "text", "text": text}]},
        "parent_tool_use_id": null,
    })
}

/// A successful `result` that spent `output_tokens`, with the process's running cost so far.
fn result(output_tokens: u64, total_cost_usd: f64) -> Value {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": "",
        "session_id": SESSION,
        "total_cost_usd": total_cost_usd,
        "usage": {"input_tokens": 2, "output_tokens": output_tokens},
    })
}

/// The text of an input line the driver wrote.
fn input_text(input: &Value) -> Option<&str> {
    input
        .pointer("/message/content/0/text")
        .and_then(Value::as_str)
}

/// A steer sent while a tool runs is written as soon as Claude takes input, taken after the
/// tool, and lands in the capture where Claude took it; its sender hears it was accepted.
#[gtest]
fn a_steer_taken_mid_tool_is_recorded_where_claude_took_it() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let driven = run(
        dir.path(),
        DriverTimeouts::default(),
        Capture::default(),
        |claude, _| {
            let question = claude.next_input()?.or_fail()?;
            verify_that!(input_text(&question), some(eq(QUESTION)))?;
            verify_that!(question.get("priority"), none())?;
            claude.say(&init())?;
            // The question's replay, which the capture leaves out.
            claude.say(&replay(&question))?;
            claude.say(&json!({
                "type": "assistant",
                "message": {"content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash",
                                         "input": {"command": "sleep 5"}}]},
                "parent_tool_use_id": null,
            }))?;
            let steer = claude.next_input()?.or_fail()?;
            verify_that!(steer.get("priority"), some(eq(&json!("next"))))?;
            claude.say(&json!({
                "type": "user",
                "message": {"role": "user", "content": [{"type": "tool_result",
                                                         "tool_use_id": "toolu_1",
                                                         "content": "done"}]},
                "parent_tool_use_id": null,
            }))?;
            claude.say(&replay(&steer))?;
            claude.say(&assistant("Only auth: it is fine."))?;
            claude.say(&result(100, 0.01))?;
            // Claude's input closes once it has reported its result.
            verify_that!(claude.next_input()?, none())?;
            Ok(EXITED_CLEANLY)
        },
        |plan| {
            let steer_id = Mailbox::new(plan.mailbox_dir()).submit("Only look at auth.")?;
            verify_that!(reply_to(plan, &steer_id)?, eq(&SteerReply::Accepted))
        },
    )?;

    assert_that!(driven.outcome, eq(&DriverOutcome::Exited(EXITED_CLEANLY)));
    // Claude's own lines, but for the question's replay, with the steer where Claude took it and
    // the driver's end last.
    let kinds: Vec<String> = driven
        .capture
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap_or_default();
            value["type"].as_str().unwrap_or_default().to_owned()
        })
        .collect();
    assert_that!(
        kinds,
        elements_are![
            eq("system"),
            eq("assistant"),
            eq("user"),
            eq("agentmux.steer"),
            eq("assistant"),
            eq("result"),
            eq("agentmux.end"),
        ]
    );
    assert_that!(driven.capture, not(contains_substring(QUESTION)));

    Ok(())
}

/// A steer Claude takes only after its result is run as a further turn of the CLI, whose answer
/// is the turn's: the settled transcript holds it below the steer, and the turn spent what both
/// results reported.
#[gtest]
fn a_steer_taken_after_the_result_is_answered_by_a_further_turn() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let driven = run(
        dir.path(),
        DriverTimeouts::default(),
        Capture::default(),
        |claude, _| {
            claude.next_input()?.or_fail()?;
            claude.say(&init())?;
            // Sent while the answer is being written, and not taken before it ends.
            let steer = claude.next_input()?.or_fail()?;
            claude.say(&assistant("FIRST ANSWER"))?;
            claude.say(&result(100, 0.001))?;
            verify_that!(claude.next_input()?, none())?;
            // The queued message, run as a further turn once the input has closed.
            claude.say(&init())?;
            claude.say(&replay(&steer))?;
            claude.say(&assistant("SECOND ANSWER"))?;
            claude.say(&result(50, 0.0014))?;
            Ok(EXITED_CLEANLY)
        },
        |plan| {
            let steer_id = Mailbox::new(plan.mailbox_dir()).submit("End with PELICAN.")?;
            verify_that!(reply_to(plan, &steer_id)?, eq(&SteerReply::Accepted))
        },
    )?;
    assert_that!(driven.outcome, eq(&DriverOutcome::Exited(EXITED_CLEANLY)));

    let folded = stream::fold(
        Vendor::Claude,
        LaunchMode::ClaudeStream,
        0,
        QUESTION,
        &driven.capture,
    );
    assert_that!(
        folded.turn.outcome,
        matches_pattern!(Outcome::Completed {
            usage: eq(&Usage {
                input_tokens: Some(4),
                output_tokens: Some(150),
                ..Usage::default()
            }),
            cost_usd: some(eq(&0.0014)),
        })
    );
    assert_that!(
        folded.turn.messages,
        elements_are![
            field!(Message.text, eq("FIRST ANSWER")),
            matches_pattern!(Message {
                source: eq(&MessageSource::Steer),
                text: eq("End with PELICAN."),
                ..
            }),
            field!(Message.text, eq("SECOND ANSWER")),
        ]
    );
    Ok(())
}

/// Once Claude has reported its result it takes nothing more: a steer sent then is rejected
/// unsent, and the capture says the turn takes no input, which the store refuses a steer for.
#[gtest]
fn a_steer_sent_after_the_result_is_rejected() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let (input_closed, closed) = channel::<()>();
    let (release, released): (Sender<()>, Receiver<()>) = channel();
    let driven = run(
        dir.path(),
        DriverTimeouts::default(),
        Capture::default(),
        move |claude, _| {
            claude.next_input()?.or_fail()?;
            claude.say(&init())?;
            claude.say(&assistant("THE ANSWER"))?;
            claude.say(&result(100, 0.01))?;
            verify_that!(claude.next_input()?, none())?;
            // Still running, as Claude is for a moment after its input closes.
            input_closed.send(()).or_fail()?;
            released.recv_timeout(Duration::from_secs(10)).or_fail()?;
            Ok(EXITED_CLEANLY)
        },
        move |plan| {
            closed.recv_timeout(Duration::from_secs(10)).or_fail()?;
            let steer_id = Mailbox::new(plan.mailbox_dir()).submit("Too late.")?;
            let reply = reply_to(plan, &steer_id)?;
            release.send(()).or_fail()?;
            verify_that!(
                reply,
                matches_pattern!(SteerReply::Rejected {
                    reason: contains_substring("reported its result"),
                })
            )
        },
    )?;
    assert_that!(driven.capture, not(contains_substring("Too late.")));

    // Before the driver's end, the result has closed the turn's input.
    let (before_end, _) = driven.capture.trim_end().rsplit_once('\n').or_fail()?;
    let folded = stream::fold(
        Vendor::Claude,
        LaunchMode::ClaudeStream,
        0,
        QUESTION,
        &format!("{before_end}\n"),
    );
    assert_that!(folded.input, eq(TurnInput::Closed));
    assert_that!(folded.turn.outcome, eq(&Outcome::Running));
    Ok(())
}

/// A `result` the driver has read but not yet handled when a steer arrives still closes the
/// input first: the steer is rejected unsent rather than run after the answer.
#[gtest]
fn a_result_already_read_closes_the_input_before_a_steer_is_claimed() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let (all_said, said) = channel::<()>();
    let (release, released) = channel::<()>();
    let driven = run(
        dir.path(),
        DriverTimeouts::default(),
        // The driver stops at this line while what follows it waits in its queue.
        Capture::holding_at("HOLD", released),
        move |claude, _| {
            claude.next_input()?.or_fail()?;
            claude.say(&init())?;
            claude.say(&assistant("HOLD"))?;
            claude.say(&assistant("THE ANSWER"))?;
            claude.say(&result(100, 0.01))?;
            all_said.send(()).or_fail()?;
            verify_that!(claude.next_input()?, none())?;
            Ok(EXITED_CLEANLY)
        },
        move |plan| {
            said.recv_timeout(Duration::from_secs(10)).or_fail()?;
            // Long enough for the driver to have read the result into its queue.
            std::thread::sleep(Duration::from_millis(200));
            let steer_id = Mailbox::new(plan.mailbox_dir()).submit("Too late.")?;
            release.send(()).or_fail()?;
            verify_that!(
                reply_to(plan, &steer_id)?,
                matches_pattern!(SteerReply::Rejected {
                    reason: contains_substring("reported its result"),
                })
            )
        },
    )?;
    assert_that!(driven.outcome, eq(&DriverOutcome::Exited(EXITED_CLEANLY)));
    assert_that!(driven.capture, not(contains_substring("Too late.")));
    Ok(())
}

/// A Claude that stops reading leaves a steer half-written when it reports its result; the
/// driver waits for that write only so long, then stops Claude, and a steer Claude still takes
/// before it leaves is confirmed.
#[gtest]
fn a_write_claude_does_not_read_after_its_result_stops_claude() -> Result<()> {
    let dir = tempfile::tempdir().or_fail()?;
    let (claimed, was_claimed) = channel::<()>();
    let stopped = ExitStatus {
        code: None,
        signal: Some(15),
    };
    let driven = run(
        dir.path(),
        DriverTimeouts {
            input_close: Duration::from_millis(300),
        },
        Capture::default(),
        move |claude, process| {
            claude.next_input()?.or_fail()?;
            claude.say(&init())?;
            // Claude reads nothing more until it is asked to stop, so the steer fills the pipe
            // and blocks.
            was_claimed
                .recv_timeout(Duration::from_secs(10))
                .or_fail()?;
            claude.say(&assistant("THE ANSWER"))?;
            claude.say(&result(100, 0.01))?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while !process.terminated() {
                verify_that!(Instant::now() < deadline, eq(true))?;
                std::thread::sleep(Duration::from_millis(20));
            }
            let steer = claude.next_input()?.or_fail()?;
            claude.say(&replay(&steer))?;
            Ok(stopped)
        },
        move |plan| {
            let mailbox = Mailbox::new(plan.mailbox_dir());
            // Larger than any pipe's buffer.
            let steer_id = mailbox.submit(&"x".repeat(4 << 20))?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while !mailbox.is_claimed(&steer_id) {
                verify_that!(Instant::now() < deadline, eq(true))?;
                std::thread::sleep(Duration::from_millis(20));
            }
            claimed.send(()).or_fail()?;
            verify_that!(reply_to(plan, &steer_id)?, eq(&SteerReply::Accepted))
        },
    )?;

    assert_that!(driven.terminated, eq(true));
    assert_that!(driven.outcome, eq(&DriverOutcome::Exited(stopped)));
    // What Claude took after its result went unanswered, so the turn is not sealed as answered.
    let folded = stream::fold(
        Vendor::Claude,
        LaunchMode::ClaudeStream,
        0,
        QUESTION,
        &driven.capture,
    );
    assert_that!(
        folded.turn.outcome,
        matches_pattern!(Outcome::Failed {
            detail: contains_substring("after its last result"),
            ..
        })
    );
    Ok(())
}
