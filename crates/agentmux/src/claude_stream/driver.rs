//! The driver: one Claude turn over stream-json input, captured as Claude writes it.
//!
//! See [`super`] for where it sits and what it says to Claude.
//!
//! Two threads move bytes and the driver's own thread decides everything: one reads Claude's
//! stdout into a channel, one writes the lines it is handed to Claude's stdin and says when each
//! is through.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read as _, Write};
use std::path::Path;
use std::process::Child;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use super::DriverPlan;
use crate::appserver::mailbox::{ClaimedSteer, Mailbox, SteerReply};
use crate::drivers::{self, PipedChild, ProcessIdentity};
use crate::launch::ExitStatus;
use crate::stream::claude::{CaptureEvent, InputBoundary};

/// What `claude` is run with beyond the arguments `--print` would have had: its input read as
/// stream-json lines for as long as stdin is open, and each message it takes echoed back.
const STREAM_INPUT_ARGS: [&str; 3] = ["--input-format", "stream-json", "--replay-user-messages"];

/// How often the cancel marker, the mailbox and Claude's process are looked at.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long output is still read once Claude is seen to have exited.
///
/// What it wrote last may still be on its way through the pipe, while something it started can
/// hold the pipe open long after it has gone.
const EXIT_DRAIN: Duration = POLL_INTERVAL.saturating_mul(3);

/// How long Claude is given to leave once asked to stop, before it is made to.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// How long Claude is waited for once it has been made to stop.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// The longest a cancelled driver takes to leave once it has seen the cancellation: a look at
/// the marker, Claude's grace, the wait once it was made to stop, the last of its output, and
/// the sweep of what it started.
pub(crate) const STOP_BUDGET: Duration = POLL_INTERVAL
    .saturating_add(STOP_GRACE)
    .saturating_add(KILL_GRACE)
    .saturating_add(EXIT_DRAIN)
    .saturating_add(drivers::SWEEP_BUDGET);

/// How long the driver waits on Claude where a deadline ends the wait.
///
/// [`DriverTimeouts::default`] is what every driven turn runs with; a test shortens it to reach
/// the path the deadline ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DriverTimeouts {
    /// How long, once Claude has reported its result, the message still being written to it is
    /// waited for before Claude is stopped instead.
    ///
    /// Claude reads its input as it comes, so only a Claude that has stopped reading reaches it.
    pub(super) input_close: Duration,
}

impl Default for DriverTimeouts {
    fn default() -> Self {
        Self {
            input_close: Duration::from_secs(10),
        }
    }
}

/// How a driven Claude turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DriverOutcome {
    /// Claude exited with this status, which the capture's last line records.
    Exited(ExitStatus),
    /// The caller cancelled the turn and Claude was stopped; the capture does not say how the
    /// turn ended, so the store's own record of the cancellation decides it.
    Cancelled,
    /// The question could not be handed to Claude, or the capture could not be written.
    Failed(String),
}

impl DriverOutcome {
    /// The driver's exit status: Claude's own, where it exited with a code, so a turn whose
    /// capture says nothing of its end is explained by its exit record as a direct launch is;
    /// non-zero for a signal or a failure, which stderr explains.
    fn exit_code(&self) -> i32 {
        match self {
            Self::Exited(ExitStatus {
                code: Some(code), ..
            }) => *code,
            Self::Cancelled => 0,
            Self::Exited(_) | Self::Failed(_) => 1,
        }
    }
}

/// The `claude` process a driver runs, as the driver sees it.
///
/// [`drive`] runs a real one; a test plays Claude in-process behind [`drive_with`].
pub(super) trait ClaudeProcess {
    /// How Claude exited, once it has; `None` while it runs.
    fn exit_status(&mut self) -> Option<ExitStatus>;
    /// Ask Claude to stop.
    fn terminate(&mut self);
    /// Make Claude stop.
    fn kill(&mut self);
}

/// The real `claude`, and what ran below it when it was asked to stop.
struct Claude {
    child: Child,
    /// Claude's descendants as they were just before it was asked to stop, once it was.
    ///
    /// Read while Claude still holds them: once it has gone they belong to the system, and
    /// nothing below the driver says they were there.
    below_at_stop: Option<Vec<ProcessIdentity>>,
}

impl ClaudeProcess for Claude {
    fn exit_status(&mut self) -> Option<ExitStatus> {
        match self.child.try_wait() {
            Ok(status) => status.map(ExitStatus::from),
            // A child that cannot be waited for has nothing more to say.
            Err(err) => {
                eprintln!("agentmux: cannot wait for claude: {err}");
                Some(ExitStatus {
                    code: None,
                    signal: None,
                })
            }
        }
    }

    fn terminate(&mut self) {
        let pid = self.child.id();
        self.below_at_stop
            .get_or_insert_with(|| drivers::descendants(pid));
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(pid)
            && let Err(err) = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            )
        {
            eprintln!("agentmux: cannot ask claude to stop: {err}");
        }
        // Windows has no polite request.
        #[cfg(not(unix))]
        ClaudeProcess::kill(self);
    }

    fn kill(&mut self) {
        if let Err(err) = self.child.kill() {
            eprintln!("agentmux: cannot stop claude: {err}");
        }
    }
}

/// Run the Claude turn a plan describes, as the `claude-stream` subcommand.
///
/// The question is read from stdin and the capture is written to stdout, which the launcher has
/// pointed at the turn's files; a turn that breaks says why on stderr.
/// Nothing is started until the turn's launch record names this process.
/// A Claude the driver stopped is stopped with whatever it was running then.
/// Returns the process's exit status.
#[must_use]
pub fn drive(plan_path: &Path) -> i32 {
    let plan = match DriverPlan::read(plan_path) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!(
                "agentmux: cannot read the Claude stream plan at {}: {error}",
                plan_path.display()
            );
            return 2;
        }
    };
    // Confirmed once, before anything starts, so every file this driver later opens by path
    // stays its own turn's.
    if let Err(reason) = drivers::confirm_launch(&plan.turn(), drivers::LAUNCH_RECORD_WAIT) {
        eprintln!("agentmux: {reason}, so claude was not started");
        return 2;
    }
    let mut question = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut question) {
        eprintln!("agentmux: cannot read the question: {error}");
        return 2;
    }

    drivers::contain_descendants("claude");
    let args: Vec<String> = plan
        .args
        .iter()
        .map(String::as_str)
        .chain(STREAM_INPUT_ARGS)
        .map(str::to_owned)
        .collect();
    let PipedChild { mut child, input } =
        match drivers::spawn_piped(&plan.program, &args, &plan.cwd, plan.billing) {
            Ok(spawned) => spawned,
            Err(error) => {
                eprintln!("agentmux: {error}");
                return 1;
            }
        };
    let output = child.stdout.take();
    let mut claude = Claude {
        child,
        below_at_stop: None,
    };
    let Some(output) = output else {
        claude.kill();
        eprintln!("agentmux: claude did not offer its stdout");
        return 1;
    };

    let stdout = std::io::stdout();
    let mut capture_writer = stdout.lock();
    let outcome = drive_with(
        &plan,
        &question,
        &mut capture_writer,
        input,
        BufReader::new(output),
        &mut claude,
        DriverTimeouts::default(),
    );
    if let Some(below) = &claude.below_at_stop {
        drivers::sweep(below, |_| {});
    }
    match &outcome {
        DriverOutcome::Failed(reason) => eprintln!("agentmux: {reason}"),
        DriverOutcome::Exited(status) if status.code.is_none() => {
            eprintln!("agentmux: claude exited with {status}");
        }
        DriverOutcome::Exited(_) | DriverOutcome::Cancelled => {}
    }
    outcome.exit_code()
}

/// Run one turn against a Claude already connected through `to_claude` and `from_claude`.
///
/// This is all of [`drive`] but the process and what it leaves: a test drives it against an
/// in-process Claude.
/// `to_claude` is dropped once Claude's input closes, which is what tells a real Claude it has
/// nothing more to take.
pub(super) fn drive_with(
    plan: &DriverPlan,
    question: &str,
    capture_writer: &mut dyn Write,
    to_claude: impl Write + Send + 'static,
    from_claude: impl BufRead + Send + 'static,
    claude: &mut dyn ClaudeProcess,
    timeouts: DriverTimeouts,
) -> DriverOutcome {
    let (inbox, events) = channel();
    let reader_inbox = inbox.clone();
    let writer_inbox = inbox.clone();
    std::thread::spawn(move || read_claude(from_claude, &reader_inbox));
    let (outbox, queue) = channel::<String>();
    std::thread::spawn(move || write_claude(to_claude, &queue, &writer_inbox));

    let question_uuid = fresh_uuid();
    let mut driver = TurnDriver {
        plan,
        capture_writer,
        claude,
        timeouts,
        mailbox: Mailbox::new(plan.mailbox_dir()),
        inbox,
        events,
        outbox: Some(outbox),
        writing: None,
        awaiting_replay: HashMap::new(),
        input: Input::Waiting,
        cancelled: false,
        stopping: None,
        exited: None,
        output_closed: false,
        capture_broken: false,
        failure: None,
        controlled_at: Instant::now(),
        question_uuid,
    };
    if !driver.hand_over(
        question_line(&driver.question_uuid, question),
        Writing::Question,
    ) {
        driver.failure = Some("cannot hand Claude the question: its input has closed".to_owned());
    }
    driver.run()
}

/// The question, as Claude's first input line.
fn question_line(uuid: &str, text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "uuid": uuid,
        "message": user_message(text),
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// A steered instruction, which Claude takes after the tool it is running, before its next model
/// call.
fn steer_line(uuid: &str, text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "uuid": uuid,
        "priority": "next",
        "message": user_message(text),
        "parent_tool_use_id": null,
    })
    .to_string()
}

fn user_message(text: &str) -> serde_json::Value {
    serde_json::json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

/// A `uuid` for one input line, which Claude's replay of it carries back unchanged.
fn fresh_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Hand every line Claude writes to the driver, then say that its output has ended.
fn read_claude(mut from_claude: impl BufRead, inbox: &Sender<Event>) {
    loop {
        let mut line = Vec::new();
        match from_claude.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => {
                // A driver that has stopped listening needs to hear nothing more.
                inbox.send(Event::OutputClosed).ok();
                return;
            }
            Ok(_) => {
                if inbox.send(Event::Line(line)).is_err() {
                    return;
                }
            }
        }
    }
}

/// Write each line handed over to Claude, say when it is through, and close Claude's input once
/// the driver hands over nothing more.
fn write_claude(mut to_claude: impl Write, queue: &Receiver<String>, inbox: &Sender<Event>) {
    for line in queue {
        let written = writeln!(to_claude, "{line}")
            .and_then(|()| to_claude.flush())
            .map_err(|err| err.to_string());
        let failed = written.is_err();
        if inbox.send(Event::Written(written)).is_err() || failed {
            return;
        }
    }
}

/// What the two threads hand over, in the order it happened.
enum Event {
    /// A line Claude wrote, as it was read.
    Line(Vec<u8>),
    /// Claude's stdout ended, or could not be read any further.
    OutputClosed,
    /// The writer is through with the line it was handed: written whole, or why not.
    Written(Result<(), String>),
    /// Everything queued ahead of it has been handled: see [`TurnDriver::catch_up`].
    CaughtUp,
}

/// The line the writer is writing.
enum Writing {
    Question,
    /// A steer, by the `uuid` it was sent with.
    Steer(String),
}

/// Where the turn's input stands.
#[derive(Clone, Copy)]
enum Input {
    /// Claude has not announced its session; messages wait in the mailbox.
    Waiting,
    /// The mailbox is served, one message at a time.
    Serving,
    /// Nothing more is taken, since `since`, for `why`, and Claude's input closes once the
    /// message being written is through.
    Closing { since: Instant, why: &'static str },
}

/// Claude's way out, once it was asked to stop.
struct Stopping {
    since: Instant,
    /// When it was made to, after its grace.
    killed_at: Option<Instant>,
}

/// How Claude exited, and when the driver saw it.
#[derive(Clone, Copy)]
struct Exited {
    status: ExitStatus,
    at: Instant,
}

struct TurnDriver<'a> {
    plan: &'a DriverPlan,
    capture_writer: &'a mut dyn Write,
    claude: &'a mut dyn ClaudeProcess,
    timeouts: DriverTimeouts,
    mailbox: Mailbox,
    /// The driver's own way into its queue, which also keeps the queue from ever disconnecting.
    inbox: Sender<Event>,
    events: Receiver<Event>,
    /// Lines for Claude's input, until it is closed.
    outbox: Option<Sender<String>>,
    /// What the writer has been handed and is not yet through with.
    writing: Option<Writing>,
    /// Every steer handed to Claude whose replay has not come, by the `uuid` it was sent with.
    awaiting_replay: HashMap<String, ClaimedSteer>,
    input: Input,
    /// Whether the caller has cancelled the turn.
    cancelled: bool,
    stopping: Option<Stopping>,
    exited: Option<Exited>,
    /// Whether Claude's stdout has ended.
    output_closed: bool,
    /// Whether the capture could not be written, after which nothing more is written to it.
    capture_broken: bool,
    /// Why the turn broke, when it did.
    failure: Option<String>,
    /// When the files and Claude's process were last looked at.
    controlled_at: Instant,
    /// The `uuid` the question was sent with, whose replay is dropped.
    question_uuid: String,
}

/// Whether a line Claude wrote is its echo of an input line, and of which.
#[derive(Debug, Default, Deserialize)]
struct ReplayProbe {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "isReplay")]
    is_replay: bool,
    #[serde(default)]
    uuid: Option<String>,
}

impl TurnDriver<'_> {
    /// Follow the turn until Claude has exited and what it wrote has been read, then end it.
    fn run(mut self) -> DriverOutcome {
        while !self.finished() {
            // The driver holds a sender of its own, so waiting ends only in an event or the
            // timeout.
            if let Ok(event) = self.events.recv_timeout(POLL_INTERVAL) {
                self.handle(event);
            }
            // The files are looked at on a clock rather than per line, so a chatty turn does not
            // stat the cancel marker hundreds of times a second.
            if self.controlled_at.elapsed() >= POLL_INTERVAL {
                self.controlled_at = Instant::now();
                self.control();
            }
        }
        // What was read before the turn was seen over is part of it; what something Claude
        // started keeps writing to its output after that is not.
        self.catch_up();
        self.finish()
    }

    /// Handle everything already queued, up to a marker queued behind it now.
    ///
    /// Bounded by what was queued when it began, however fast Claude, or something it started,
    /// keeps writing.
    fn catch_up(&mut self) {
        if self.inbox.send(Event::CaughtUp).is_err() {
            return;
        }
        while let Ok(event) = self.events.recv() {
            if matches!(event, Event::CaughtUp) {
                return;
            }
            self.handle(event);
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Line(line) if !self.capture_broken => {
                if let Err(reason) = self.forward(&line) {
                    self.capture_broken = true;
                    self.failure.get_or_insert(reason);
                    // A turn whose capture cannot be written has lost its deliverable.
                    self.stop_claude();
                }
            }
            Event::Line(_) | Event::CaughtUp => {}
            Event::OutputClosed => self.output_closed = true,
            Event::Written(written) => self.written(written),
        }
    }

    /// Put one line Claude wrote into the capture, or what it stands for, and act on it.
    fn forward(&mut self, line: &[u8]) -> Result<(), String> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let value = serde_json::from_slice::<Value>(line).unwrap_or_default();
        let probe = ReplayProbe::deserialize(&value).unwrap_or_default();
        if probe.kind == "user"
            && probe.is_replay
            && let Some(uuid) = &probe.uuid
        {
            if *uuid == self.question_uuid {
                return Ok(());
            }
            if let Some(claim) = self.awaiting_replay.remove(uuid) {
                return self.accept(&claim);
            }
        }
        self.record(line)?;
        match InputBoundary::of(&value) {
            Some(InputBoundary::Opens) if matches!(self.input, Input::Waiting) => {
                self.input = Input::Serving;
            }
            Some(InputBoundary::Closes) => {
                self.close_input("Claude has reported its result and takes nothing more");
            }
            _ => {}
        }
        Ok(())
    }

    /// Record a steer Claude has taken where it took it, then tell its sender.
    fn accept(&mut self, claim: &ClaimedSteer) -> Result<(), String> {
        // Recorded before the sender hears of it, so a transcript read after a successful
        // `steer` always holds the instruction.
        let recorded = self.record_event(&CaptureEvent::Steer {
            text: claim.text.clone(),
        });
        self.reply(
            claim,
            &match &recorded {
                Ok(()) => SteerReply::Accepted,
                Err(reason) => SteerReply::Unconfirmed {
                    reason: format!("Claude took it, but agentmux could not record it: {reason}"),
                },
            },
        );
        recorded
    }

    /// Act on the writer being through with its line.
    fn written(&mut self, written: Result<(), String>) {
        let (Some(writing), Err(reason)) = (self.writing.take(), written) else {
            return;
        };
        match writing {
            Writing::Question => {
                self.failure
                    .get_or_insert(format!("cannot hand Claude the question: {reason}"));
            }
            // Only a line written whole can be taken, so a steer that was not is known unsent.
            Writing::Steer(uuid) => {
                if let Some(claim) = self.awaiting_replay.remove(&uuid) {
                    self.reply(
                        &claim,
                        &SteerReply::Rejected {
                            reason: format!("it could not be written to Claude: {reason}"),
                        },
                    );
                }
            }
        }
    }

    /// Hand a line to the writer, which is through with the one before.
    ///
    /// Returns `false` when the writer has stopped, after a line it could not write, or Claude's
    /// input is closed.
    fn hand_over(&mut self, line: String, writing: Writing) -> bool {
        let handed = self
            .outbox
            .as_ref()
            .is_some_and(|outbox| outbox.send(line).is_ok());
        if handed {
            self.writing = Some(writing);
        }
        handed
    }

    /// Look at the files and at Claude's process, and do what they call for.
    fn control(&mut self) {
        if self.exited.is_none()
            && let Some(status) = self.claude.exit_status()
        {
            self.exited = Some(Exited {
                status,
                at: Instant::now(),
            });
            self.close_input("Claude has exited");
        }
        // A Claude that has already exited ended the turn itself.
        if !self.cancelled && self.exited.is_none() && self.plan.cancel_marker_path().exists() {
            self.cancelled = true;
            self.close_input("the consultation is being cancelled");
            self.stop_claude();
        }
        match self.input {
            Input::Waiting => {}
            Input::Serving => self.serve_next(),
            Input::Closing { since, why } => {
                for claim in self.mailbox.claim_all() {
                    self.reply(
                        &claim,
                        &SteerReply::Rejected {
                            reason: why.to_owned(),
                        },
                    );
                }
                if self.writing.is_none() {
                    self.outbox = None;
                } else if since.elapsed() >= self.timeouts.input_close {
                    // A Claude that has stopped reading would keep its input open for good.
                    // The message still being written keeps its claim: a replay read before the
                    // turn ends confirms it.
                    self.stop_claude();
                }
            }
        }
        if let Some(stopping) = &mut self.stopping
            && self.exited.is_none()
            && stopping.killed_at.is_none()
            && stopping.since.elapsed() >= STOP_GRACE
        {
            self.claude.kill();
            stopping.killed_at = Some(Instant::now());
        }
    }

    /// Claim the oldest waiting message and hand it to Claude, once the writer is through with
    /// the one before.
    ///
    /// What Claude has written so far is handled first, so a `result` already read closes the
    /// input before another message can be claimed.
    fn serve_next(&mut self) {
        if self.writing.is_some() {
            return;
        }
        self.catch_up();
        if !matches!(self.input, Input::Serving) {
            return;
        }
        let Some(claim) = self.mailbox.claim_next() else {
            return;
        };
        let uuid = fresh_uuid();
        if self.hand_over(steer_line(&uuid, &claim.text), Writing::Steer(uuid.clone())) {
            self.awaiting_replay.insert(uuid, claim);
        } else {
            self.reply(
                &claim,
                &SteerReply::Rejected {
                    reason: "Claude's input has closed".to_owned(),
                },
            );
        }
    }

    /// Stop serving the mailbox, for `why`, once and for good.
    fn close_input(&mut self, why: &'static str) {
        if !matches!(self.input, Input::Closing { .. }) {
            self.input = Input::Closing {
                since: Instant::now(),
                why,
            };
        }
    }

    /// Ask Claude to stop, once, unless it has already gone.
    fn stop_claude(&mut self) {
        if self.stopping.is_none() && self.exited.is_none() {
            self.claude.terminate();
            self.stopping = Some(Stopping {
                since: Instant::now(),
                killed_at: None,
            });
        }
    }

    /// Whether the turn is over: Claude has exited and its output has been read, or it was made
    /// to stop and cannot be waited for any longer.
    fn finished(&self) -> bool {
        match self.exited {
            Some(Exited { at, .. }) => self.output_closed || at.elapsed() >= EXIT_DRAIN,
            None => self
                .stopping
                .as_ref()
                .and_then(|stopping| stopping.killed_at)
                .is_some_and(|at| at.elapsed() >= KILL_GRACE),
        }
    }

    /// End the turn: record how Claude exited, unless the turn was cancelled, and answer every
    /// message still open.
    fn finish(mut self) -> DriverOutcome {
        let status = self.exited.map_or(
            ExitStatus {
                code: None,
                signal: None,
            },
            |exited| exited.status,
        );
        // A cancelled turn's end is the store's to record, as it is for `claude --print`.
        if !self.cancelled
            && !self.capture_broken
            && let Err(reason) = self.record_event(&CaptureEvent::End { status })
        {
            self.failure.get_or_insert(reason);
        }
        for claim in std::mem::take(&mut self.awaiting_replay).into_values() {
            self.reply(
                &claim,
                &SteerReply::Unconfirmed {
                    reason: "Claude did not confirm delivery before the turn ended".to_owned(),
                },
            );
        }
        for claim in self.mailbox.claim_all() {
            self.reply(
                &claim,
                &SteerReply::Rejected {
                    reason: "the turn has ended".to_owned(),
                },
            );
        }
        match self.failure {
            Some(reason) => DriverOutcome::Failed(reason),
            None if self.cancelled => DriverOutcome::Cancelled,
            None => DriverOutcome::Exited(status),
        }
    }

    /// Append one line to the capture.
    ///
    /// Flushed at once: the capture is what every reader folds, and a line held in a buffer is a
    /// line a `tail` cannot see.
    fn record(&mut self, line: &[u8]) -> Result<(), String> {
        self.capture_writer
            .write_all(line)
            .and_then(|()| self.capture_writer.write_all(b"\n"))
            .and_then(|()| self.capture_writer.flush())
            .map_err(|err| format!("cannot write the event capture: {err}"))
    }

    fn record_event(&mut self, event: &CaptureEvent) -> Result<(), String> {
        let line = serde_json::to_vec(event)
            .map_err(|err| format!("cannot write the event capture: {err}"))?;
        self.record(&line)
    }

    fn reply(&self, claim: &ClaimedSteer, reply: &SteerReply) {
        if let Err(err) = self.mailbox.write_reply(claim, reply) {
            eprintln!("agentmux: cannot answer a steered message: {err}");
        }
    }
}
