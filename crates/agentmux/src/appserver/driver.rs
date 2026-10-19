//! The driver: one turn over `codex app-server`, written out as `codex exec --json` would write it.
//!
//! See [`super`] for where it sits and why it is a process of its own.
//!
//! Two threads move bytes and the driver's own thread decides everything: one reads the server's
//! stdout into a channel, one writes the queued lines to its stdin.
//! A server that stops reading therefore never blocks the driver, which keeps watching the cancel
//! marker and its own deadlines whatever the pipes do.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, PipeWriter, Read as _, Write};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use codex_app_server_protocol::envelope::{
    INTERNAL_ERROR, METHOD_NOT_FOUND, Message, RequestId, RpcError,
};
use codex_app_server_protocol::methods::{self, AnyServerRequest};
use codex_app_server_protocol::{ClientRequest, Method as _, TypedParams, TypedResponse};
use serde_json::Value;

use super::DriverPlan;
use super::mailbox::{ClaimedSteer, Mailbox, SteerReply};
use super::protocol;
use super::protocol::{ThreadId, TurnId, TurnTarget};
use super::translate::{self, Translator, TurnEndStatus};
use crate::delegate::{Billing, scrub_billing_env};
use crate::run::layout::TurnDir;
use crate::stream::codex::CaptureEvent;

/// How often the cancel marker, the mailbox and the server process are looked at while a turn
/// runs.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long output is still waited for once the server is seen to have exited.
///
/// What it wrote last may still be on its way through the pipe, and a terminal event among it is
/// how the turn ended.
const EXIT_DRAIN: Duration = POLL_INTERVAL.saturating_mul(3);

/// How long an interrupted turn is given to close before the driver leaves anyway, unless its
/// [`DriverTimeouts`] say otherwise.
const INTERRUPT_GRACE: Duration = Duration::from_secs(8);

/// How long the server is given to exit once its stdin is closed.
///
/// It writes the thread's rollout on the way out, which a follow-up resumes from, so it is asked
/// to leave by end of input and only then by signal.
const SERVER_EXIT_GRACE: Duration = Duration::from_secs(5);

/// How long what the server left running is given between the polite signal and the forceful one.
const SWEEP_GRACE: Duration = Duration::from_secs(2);

/// How long, once the forceful signal is due, what is found below the driver keeps being killed
/// and looked for again.
///
/// A process forked after one read of the process table is only found by the next, and the
/// driver's exit would not stop it: an adopted orphan is handed on to another parent, not killed.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// The longest a cancelled driver takes to leave once it has seen the cancellation: the
/// interrupt, which covers waiting for a turn being started, the server's own exit, and the sweep
/// of whatever it left behind, both signals included.
///
/// The store's `cancel` waits longer than this before it signals the driver, so a driver that
/// honours the cancellation is never killed in the middle of cleaning up.
pub const STOP_BUDGET: Duration = INTERRUPT_GRACE
    .saturating_add(SERVER_EXIT_GRACE)
    .saturating_add(SWEEP_GRACE)
    .saturating_add(KILL_GRACE);

/// How long a driver waits for its turn's launch record to name it before it gives the turn up.
///
/// The caller writes that record as soon as the spawn returns, so a driver normally finds it on
/// its first look, and one that never comes means the caller died in between.
/// Generous, because giving up breaks a turn that is otherwise sound, while waiting only delays a
/// driver whose caller is already gone.
const LAUNCH_RECORD_WAIT: Duration = Duration::from_secs(10);

/// How often the launch record is looked for while it is not there yet.
const LAUNCH_RECORD_POLL: Duration = Duration::from_millis(20);

/// How long the driver waits on the server at each point where it waits for an answer.
///
/// [`DriverTimeouts::default`] is what every driven turn runs with; a test shortens them to reach
/// the paths a deadline ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverTimeouts {
    /// How long one setup request may go unanswered.
    ///
    /// Generous, because resuming a long thread reads its whole rollout before it answers.
    pub setup_response: Duration,
    /// How long an interrupted turn, or a cancelled one still being started, is given to close
    /// before the driver leaves anyway.
    pub interrupt_completion: Duration,
    /// How long, once the turn has ended, a steer the server has not answered yet is waited for.
    pub steer_settlement: Duration,
}

impl Default for DriverTimeouts {
    fn default() -> Self {
        Self {
            setup_response: Duration::from_secs(120),
            interrupt_completion: INTERRUPT_GRACE,
            steer_settlement: Duration::from_secs(2),
        }
    }
}

/// How a driven turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverOutcome {
    /// The turn closed and the capture ends in the terminal event that says how.
    TurnEnded,
    /// The caller cancelled the turn; the capture carries no terminal event, so the store's own
    /// record of the cancellation decides it.
    Cancelled,
    /// The turn never reached an end the capture could record: the server could not be started,
    /// refused to set the turn up, went away part-way through, or the capture could not be
    /// written.
    Failed(String),
}

impl DriverOutcome {
    /// The driver's exit status: zero when the capture or the store says how the turn ended,
    /// non-zero when only stderr does.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::TurnEnded | Self::Cancelled => 0,
            Self::Failed(_) => 1,
        }
    }
}

/// Run the turn a plan describes, as the `codex-app-server` subcommand.
///
/// The question is read from stdin and the capture is written to stdout, which the launcher has
/// pointed at the turn's files; a turn that breaks says why on stderr.
/// Nothing is started until the turn's launch record names this process, which the caller writes
/// once the spawn returns.
/// Returns the process's exit status.
#[must_use]
pub fn drive(plan_path: &Path) -> i32 {
    let plan = match DriverPlan::read(plan_path) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!(
                "agentmux: cannot read the app-server plan at {}: {error}",
                plan_path.display()
            );
            return 2;
        }
    };
    // Confirmed once, before anything starts: once a turn's launch record exists, a
    // follow-up never takes its directory over as an abandoned claim, so every file this
    // driver later opens by path stays its own turn's.
    if let Err(reason) = confirm_launch(&plan.turn(), LAUNCH_RECORD_WAIT) {
        eprintln!("agentmux: {reason}, so codex app-server was not started");
        return 2;
    }
    let mut question = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut question) {
        eprintln!("agentmux: cannot read the question: {error}");
        return 2;
    }

    let adopting = adopt_orphans();
    contain_descendants();
    let Server {
        mut child,
        to_server,
    } = match spawn_server(&plan) {
        Ok(spawned) => spawned,
        Err(error) => {
            eprintln!(
                "agentmux: cannot run `{} app-server`: {error}. Is it installed and on PATH? \
                 Otherwise, {}.",
                plan.settings.program,
                super::EXEC_FALLBACK_HINT
            );
            return 1;
        }
    };
    // Nothing to watch on Windows, where the job object stops what the driver leaves behind.
    let descendant_monitor =
        (!cfg!(windows)).then(|| DescendantMonitor::start(child.id(), adopting));
    // The server's input stays open through a second handle until `stop_server` has read what
    // the server left running: closed earlier, the server could exit first and orphan it.
    let (Some(from_server), Ok(input)) = (child.stdout.take(), to_server.try_clone()) else {
        stop_server(
            &mut child,
            to_server,
            descendant_monitor
                .map(DescendantMonitor::stop)
                .unwrap_or_default(),
        );
        eprintln!("agentmux: codex app-server did not offer stdio");
        return 1;
    };

    let stdout = std::io::stdout();
    let mut capture_writer = stdout.lock();
    let outcome = drive_with(
        &plan,
        &question,
        &mut capture_writer,
        to_server,
        BufReader::new(from_server),
        &mut || !matches!(child.try_wait(), Ok(None)),
        DriverTimeouts::default(),
    );
    let recorded_descendants = descendant_monitor
        .map(DescendantMonitor::stop)
        .unwrap_or_default();
    stop_server(&mut child, input, recorded_descendants);
    if let DriverOutcome::Failed(reason) = &outcome {
        eprintln!("agentmux: {reason}");
        eprintln!("agentmux: {}.", super::EXEC_FALLBACK_HINT);
    }
    outcome.exit_code()
}

/// Wait up to `wait` for the turn's launch record to name this process.
///
/// A caller that dies between spawning the driver and recording it leaves a directory that a
/// follow-up takes over as an abandoned claim and recreates under the same path.
/// A driver that went on regardless would claim the new turn's steers and overwrite its closing
/// message, so it stops unless the record names it.
/// Returns why the turn is not this driver's: the record names another process, cannot be read,
/// or never appeared.
fn confirm_launch(turn: &TurnDir, wait: Duration) -> Result<(), String> {
    let driver_pid = std::process::id();
    let deadline = Instant::now() + wait;
    loop {
        match turn.read_launch_identity() {
            Ok(Some(launched)) if launched.pid == driver_pid => return Ok(()),
            Ok(Some(launched)) => {
                return Err(format!(
                    "the turn at {} was launched as process {}, not as this driver ({driver_pid})",
                    turn.root.display(),
                    launched.pid
                ));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(LAUNCH_RECORD_POLL),
            Ok(None) => {
                return Err(format!(
                    "no launch record for the turn at {} appeared within {wait:?}",
                    turn.root.display()
                ));
            }
            Err(err) => return Err(format!("cannot read the turn's launch record: {err}")),
        }
    }
}

/// Run one turn against a server already connected through `to_server` and `from_server`.
///
/// This is all of [`drive`] but the process: a test drives it against an in-process server.
/// `to_server` is closed once everything queued for it is written after the turn, which is what
/// tells a real server to exit.
/// `server_exited` says whether the server process has gone, which its output alone cannot say
/// while something it started still holds that output open.
pub fn drive_with(
    plan: &DriverPlan,
    question: &str,
    capture_writer: &mut dyn Write,
    to_server: impl Write + Send + 'static,
    from_server: impl BufRead + Send + 'static,
    server_exited: &mut dyn FnMut() -> bool,
    timeouts: DriverTimeouts,
) -> DriverOutcome {
    let (inbox, events) = channel();
    let exit_drain_sender = inbox.clone();
    std::thread::spawn(move || read_server(from_server, &inbox));
    let (outbox, queue) = channel::<String>();
    std::thread::spawn(move || write_server(to_server, &queue));

    let mut driver = TurnDriver {
        plan,
        capture_writer,
        outbox,
        events,
        exit_drain_sender: Some(exit_drain_sender),
        next_id: 0,
        translator: Translator::default(),
        mailbox: Mailbox::new(plan.mailbox_dir()),
        pending: HashMap::new(),
        pending_turn_end: None,
        cancellation_observed_at: None,
        server_exited,
        server_probed_at: Instant::now(),
        server_gone_since: None,
        closing_message_saved: false,
        timeouts,
    };
    let outcome = match driver.set_up(question) {
        Ok(thread_id) => driver.run_turn(&thread_id),
        // `set_up` hands a started turn to the running loop itself.
        Err(SetupExit::Cancelled | SetupExit::TurnObserved) => {
            // A turn that ends this way records no end, so there is nothing left to refuse.
            let _ = driver.save_closing_message();
            DriverOutcome::Cancelled
        }
        // An end the server already reported is how the turn ended, whatever broke after it.
        Err(SetupExit::Failed(reason)) => match driver.pending_turn_end.take() {
            Some(turn_end) => driver
                .close(&turn_end)
                .unwrap_or_else(DriverOutcome::Failed),
            None => DriverOutcome::Failed(reason),
        },
    };
    // A turn that broke after its answer arrived still leaves it where a failed turn's reply is
    // recovered from.
    if matches!(outcome, DriverOutcome::Failed(_)) {
        let _ = driver.save_closing_message();
    }
    outcome
}

/// Hand every JSON-RPC message the server writes to the driver, then say why it stopped.
fn read_server(mut from_server: impl BufRead, inbox: &Sender<Event>) {
    let mut line = String::new();
    loop {
        line.clear();
        match from_server.read_line(&mut line) {
            Ok(0) => {
                let _ = inbox.send(Event::OutputClosed(String::new()));
                return;
            }
            Ok(_) => {
                let line = line.trim_end();
                let event = match Message::parse(line) {
                    Ok(message) => Event::Message(message),
                    Err(reason) => Event::Malformed {
                        line: line.to_owned(),
                        reason: reason.to_string(),
                    },
                };
                if inbox.send(event).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = inbox.send(Event::OutputClosed(format!(": {error}")));
                return;
            }
        }
    }
}

/// Write each queued line to the server, and close its input once the queue is closed and empty.
fn write_server(mut to_server: impl Write, queue: &Receiver<String>) {
    for line in queue {
        if writeln!(to_server, "{line}")
            .and_then(|()| to_server.flush())
            .is_err()
        {
            return;
        }
    }
}

/// What the server-reading thread hands over.
enum Event {
    Message(Message),
    /// A line that is no JSON-RPC message, kept so the capture says what arrived rather than
    /// losing it: a turn's end that cannot be read would otherwise look like a server that never
    /// sent one.
    Malformed {
        line: String,
        reason: String,
    },
    /// The server's stdout ended, with what the read said about why.
    OutputClosed(String),
    /// Everything read before the server was seen gone has been handed over; the driver queues
    /// it itself, behind that output.
    ExitDrainComplete,
}

/// Why setting a turn up stopped.
enum SetupExit {
    /// The caller cancelled before the turn started, or before its id could be learned.
    Cancelled,
    /// The turn is running, or already over, before `turn/start` was answered: `turn/started`
    /// named it, or the server reported its end.
    /// The running loop takes it from there, so a steer or a cancellation is served and a
    /// finished turn is written without waiting for the answer.
    TurnObserved,
    /// The server could not be asked, or refused, or the capture could not be written.
    Failed(String),
}

/// A request of the driver's that the server has not answered yet.
enum PendingRequest {
    /// A steer, sent into the turn it names.
    Steer {
        claim: ClaimedSteer,
        expected_turn_id: TurnId,
    },
    Interrupt,
}

/// A request's answer, decoded as its method's response, or what arrived when it did not decode,
/// which has already been recorded as drift.
enum ResponsePayload<T> {
    Decoded(T),
    Undecoded(Value),
}

impl<T> ResponsePayload<T> {
    fn map<U>(self, decoded: impl FnOnce(T) -> U) -> ResponsePayload<U> {
        match self {
            Self::Decoded(answer) => ResponsePayload::Decoded(decoded(answer)),
            Self::Undecoded(raw) => ResponsePayload::Undecoded(raw),
        }
    }
}

impl ResponsePayload<String> {
    /// The decoded identifier, or the string at `pointer` of an answer that did not decode.
    fn salvage(self, pointer: &str) -> Option<String> {
        match self {
            Self::Decoded(id) => Some(id),
            Self::Undecoded(raw) => raw
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        }
    }
}

/// The end of the turn, as the server reported it, held until the steers still in flight are
/// settled, so that an accepted one is recorded before the event the fold stops at.
struct PendingTurnEnd {
    status: TurnEndStatus,
    events: Vec<CaptureEvent>,
}

struct TurnDriver<'a> {
    plan: &'a DriverPlan,
    capture_writer: &'a mut dyn Write,
    outbox: Sender<String>,
    events: Receiver<Event>,
    next_id: i64,
    translator: Translator,
    mailbox: Mailbox,
    pending: HashMap<RequestId, PendingRequest>,
    pending_turn_end: Option<PendingTurnEnd>,
    /// When the driver first saw the caller's cancellation.
    cancellation_observed_at: Option<Instant>,
    server_exited: &'a mut dyn FnMut() -> bool,
    /// When `server_exited` was last asked.
    server_probed_at: Instant,
    /// When the driver first found the server gone.
    server_gone_since: Option<Instant>,
    /// Queues [`Event::ExitDrainComplete`] behind the server's output, once.
    exit_drain_sender: Option<Sender<Event>>,
    /// Whether the closing message is already where a reader finds it.
    closing_message_saved: bool,
    timeouts: DriverTimeouts,
}

impl TurnDriver<'_> {
    /// Append lines to the capture.
    ///
    /// Flushed at once: the capture is what every reader folds, and a line held in a buffer is a
    /// line a `tail` cannot see.
    /// A capture that cannot be written ends the turn, because the transcript is the deliverable.
    fn record(&mut self, events: &[CaptureEvent]) -> Result<(), String> {
        events
            .iter()
            .try_for_each(|event| {
                serde_json::to_writer(&mut self.capture_writer, event)?;
                writeln!(self.capture_writer).map_err(serde_json::Error::io)
            })
            .and_then(|()| self.capture_writer.flush().map_err(serde_json::Error::io))
            .map_err(|error| format!("cannot write the event capture: {error}"))
    }

    fn enqueue_message(&self, message: &Message) -> Result<(), String> {
        self.outbox
            .send(message.to_line())
            .map_err(|_| "codex app-server closed its input".to_owned())
    }

    fn enqueue_request<M: ClientRequest + TypedParams>(
        &mut self,
        params: &M::Params,
    ) -> Result<RequestId, String> {
        self.next_id += 1;
        let id = RequestId::Integer(self.next_id);
        let request = Message::request::<M>(id.clone(), params)
            .map_err(|err| format!("{err}: {}", err.source))?;
        self.enqueue_message(&request)?;
        Ok(id)
    }

    fn cancel_requested(&self) -> bool {
        self.plan.cancel_marker_path().exists()
    }

    /// The next message from the server, waiting up to `wait` for one.
    ///
    /// Once the server has been gone for [`EXIT_DRAIN`], [`Event::ExitDrainComplete`] is queued
    /// behind what has been read so far, and the caller reads it as the server's departure.
    /// So a completion the server wrote before it left still ends the turn, however long the
    /// messages ahead of it take to record, while something it started that keeps writing to its
    /// output cannot push the boundary back.
    fn receive(&mut self, wait: Duration) -> Result<Event, RecvTimeoutError> {
        if self.exit_drain_elapsed()
            && let Some(exit_drain_sender) = self.exit_drain_sender.take()
        {
            let _ = exit_drain_sender.send(Event::ExitDrainComplete);
        }
        self.events.recv_timeout(wait)
    }

    /// Whether the server has gone and what it wrote before leaving has had time to arrive.
    ///
    /// Asked before every message as well as when none arrives, and answered from the process on
    /// a clock: something the server started can hold its output open and keep writing to it
    /// after the server has gone, and a turn whose server is gone has ended all the same.
    fn exit_drain_elapsed(&mut self) -> bool {
        if let Some(since) = self.server_gone_since {
            return since.elapsed() >= EXIT_DRAIN;
        }
        if self.server_probed_at.elapsed() >= POLL_INTERVAL {
            self.server_probed_at = Instant::now();
            if (self.server_exited)() {
                self.server_gone_since = Some(Instant::now());
            }
        }
        false
    }

    /// Whether the caller has cancelled, remembering when the driver first saw it, which is when
    /// the interrupt's grace starts however far the turn had got.
    fn notice_cancellation(&mut self) -> bool {
        if self.cancellation_observed_at.is_none() && self.cancel_requested() {
            self.cancellation_observed_at = Some(Instant::now());
        }
        self.cancellation_observed_at.is_some()
    }

    /// Answer a request from the server: an approval is declined, anything else refused.
    ///
    /// Said in the capture as well, because a refused action changes what the delegate did next.
    fn decline(&mut self, id: &RequestId, method: &str, params: &Value) -> Result<(), String> {
        let request = AnyServerRequest::classify(method, params);
        let response = match request.and_then(|request| protocol::decline_response(id, request)) {
            Some(Ok(response)) => response,
            Some(Err(err)) => Message::error_response(
                id.clone(),
                INTERNAL_ERROR,
                &format!("{err}: {}", err.source),
            ),
            None => Message::error_response(
                id.clone(),
                METHOD_NOT_FOUND,
                "agentmux runs consultations unattended and answers no requests of this kind",
            ),
        };
        // A server that has gone cannot be answered, and its departure is reported on its own.
        let _ = self.enqueue_message(&response);
        let note = self.translator.warning_event(&format!(
            "agentmux declined codex's `{method}` request: a consultation runs with nobody to \
             approve or answer it"
        ));
        self.record(&[note])
    }

    /// Translate one notification into the capture, holding back the end of the turn.
    ///
    /// Whatever the server says after the turn ended is not part of it, and is not even
    /// translated, so it cannot change the closing message either.
    fn handle_notification(
        &mut self,
        method: &str,
        params: &Value,
        cancelling: bool,
    ) -> Result<(), String> {
        if self.pending_turn_end.is_some() {
            return Ok(());
        }
        let translated = self
            .translator
            .translate_notification(method, params, cancelling);
        match translated.turn_end {
            Some(status) => {
                self.pending_turn_end = Some(PendingTurnEnd {
                    status,
                    events: translated.events,
                });
                Ok(())
            }
            None => self.record(&translated.events),
        }
    }

    /// Send a setup request and wait for its answer, recording whatever the server says
    /// meanwhile.
    ///
    /// Nothing new is sent once the caller has cancelled.
    /// A cancellation while the answer is awaited ends the setup at once, except for `turn/start`:
    /// that turn may already be running, so the setup goes on until the turn's id is known, and
    /// the running loop interrupts it, but for no longer than an interrupt itself is given.
    /// `turn/start` is over as soon as the turn is known — from `turn/started`, or from an end the
    /// server reported — whether or not it was answered, because a running turn takes steers and
    /// a finished one is written at once.
    ///
    /// An answer that does not decode as `M`'s response is recorded as drift and handed back as
    /// it arrived, for the caller to salvage what it needs.
    fn call<M: ClientRequest + TypedResponse>(
        &mut self,
        params: &M::Params,
    ) -> Result<ResponsePayload<M::Response>, SetupExit> {
        let method = M::METHOD;
        let starts_the_turn = method == methods::TurnStart::METHOD;
        if self.cancel_requested() {
            return Err(SetupExit::Cancelled);
        }
        let id = self
            .enqueue_request::<M>(params)
            .map_err(SetupExit::Failed)?;
        let mut deadline = Instant::now() + self.timeouts.setup_response;
        loop {
            if starts_the_turn
                && (self.translator.turn_id().is_some() || self.pending_turn_end.is_some())
            {
                return Err(SetupExit::TurnObserved);
            }
            if self.notice_cancellation() {
                if !starts_the_turn {
                    return Err(SetupExit::Cancelled);
                }
                if let Some(at) = self.cancellation_observed_at {
                    deadline = deadline.min(at + self.timeouts.interrupt_completion);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() && self.cancellation_observed_at.is_some() {
                return Err(SetupExit::Cancelled);
            }
            if remaining.is_zero() {
                return Err(SetupExit::Failed(format!(
                    "codex app-server did not answer `{method}` within {:?}",
                    self.timeouts.setup_response
                )));
            }
            match self.receive(remaining.min(POLL_INTERVAL)) {
                Ok(Event::Message(Message::Response {
                    id: response_id,
                    outcome,
                })) if response_id == id => {
                    let result = outcome.map_err(|error| {
                        SetupExit::Failed(format!(
                            "codex app-server refused `{method}`: {}",
                            refusal(&error)
                        ))
                    })?;
                    return match M::decode_response(&result) {
                        Ok(response) => Ok(ResponsePayload::Decoded(response)),
                        Err(reason) => {
                            self.record(&[translate::undecoded_message_event(
                                method,
                                &reason.to_string(),
                                &result,
                            )])
                            .map_err(SetupExit::Failed)?;
                            Ok(ResponsePayload::Undecoded(result))
                        }
                    };
                }
                Ok(Event::Message(Message::Response { .. })) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Event::Message(Message::Notification { method, params })) => {
                    let cancelling = self.cancel_requested();
                    self.handle_notification(&method, &params, cancelling)
                        .map_err(SetupExit::Failed)?;
                }
                Ok(Event::Message(Message::Request { id, method, params })) => {
                    self.decline(&id, &method, &params)
                        .map_err(SetupExit::Failed)?;
                }
                Ok(Event::Malformed { line, reason }) => {
                    self.record(&[translate::malformed_line_event(&line, &reason)])
                        .map_err(SetupExit::Failed)?;
                }
                Ok(Event::OutputClosed(why)) => {
                    return Err(SetupExit::Failed(format!(
                        "codex app-server exited before it answered `{method}`{why}"
                    )));
                }
                Ok(Event::ExitDrainComplete) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(SetupExit::Failed(format!(
                        "codex app-server exited before it answered `{method}`"
                    )));
                }
            }
        }
    }

    /// Handshake, open or resume the thread, and start the turn.
    ///
    /// Returns the thread id.
    fn set_up(&mut self, question: &str) -> Result<ThreadId, SetupExit> {
        // The answer says nothing the driver needs; the handshake is what counts.
        self.call::<methods::Initialize>(&protocol::initialize_params())?;
        let initialized = Message::notification::<methods::Initialized>(&())
            .map_err(|err| SetupExit::Failed(format!("{err}: {}", err.source)))?;
        self.enqueue_message(&initialized)
            .map_err(SetupExit::Failed)?;

        let settings = &self.plan.settings;
        let cwd = self.plan.cwd.to_string_lossy().into_owned();
        let model = settings.model.as_str().to_owned();
        let sandbox = settings.sandbox;
        let (method, thread_id_response) = if let Some(session) = &self.plan.resume_thread_id {
            let params = protocol::thread_resume_params(session, &model, &cwd, sandbox);
            let thread_resume_response = self.call::<methods::ThreadResume>(&params)?;
            (
                methods::ThreadResume::METHOD,
                thread_resume_response.map(|response| response.thread.id),
            )
        } else {
            let params = protocol::thread_start_params(&model, &cwd, sandbox);
            let thread_start_response = self.call::<methods::ThreadStart>(&params)?;
            (
                methods::ThreadStart::METHOD,
                thread_start_response.map(|response| response.thread.id),
            )
        };
        // Both answers carry the thread in the same place.
        let thread_id = thread_id_response
            .salvage("/thread/id")
            .map(ThreadId::new)
            .ok_or_else(|| {
                SetupExit::Failed(format!(
                    "codex app-server answered `{method}` with no thread id"
                ))
            })?;
        let announced = self.translator.thread_began(&thread_id);
        self.record(&[announced]).map_err(SetupExit::Failed)?;

        let start = protocol::turn_start_params(&thread_id, question);
        let turn_start_response = match self.call::<methods::TurnStart>(&start) {
            Ok(response) => response,
            Err(SetupExit::TurnObserved) => return Ok(thread_id),
            Err(other) => return Err(other),
        };
        let turn_id = turn_start_response
            .map(|response| response.turn.id)
            .salvage("/turn/id")
            .map(TurnId::new)
            .ok_or_else(|| {
                SetupExit::Failed(
                    "codex app-server answered `turn/start` with no turn id".to_owned(),
                )
            })?;
        if let Some(line) = self.translator.turn_began(&turn_id) {
            self.record(&[line]).map_err(SetupExit::Failed)?;
        }
        Ok(thread_id)
    }

    /// Follow the running turn to its end, delivering steers and honouring a cancellation.
    fn run_turn(&mut self, thread_id: &ThreadId) -> DriverOutcome {
        match self.follow(thread_id) {
            Ok(ending) => ending,
            Err(reason) => {
                self.mark_pending_steers_unconfirmed("the turn ended before codex answered it");
                DriverOutcome::Failed(reason)
            }
        }
    }

    fn follow(&mut self, thread_id: &ThreadId) -> Result<DriverOutcome, String> {
        let mut interrupt_attempted = false;
        let mut last_control_poll_at = Instant::now();
        loop {
            if let Some(turn_end) = self.pending_turn_end.take() {
                return self.close(&turn_end);
            }
            match self.receive(POLL_INTERVAL) {
                Ok(Event::Message(Message::Notification { method, params })) => {
                    let cancelling = self.cancellation_observed_at.is_some()
                        || (method == methods::TurnCompleted::METHOD && self.cancel_requested());
                    self.handle_notification(&method, &params, cancelling)?;
                }
                Ok(Event::Message(Message::Response { id, outcome })) => {
                    if let Some(request) = self.pending.remove(&id) {
                        self.resolve(request, outcome)?;
                    }
                }
                Ok(Event::Message(Message::Request { id, method, params })) => {
                    self.decline(&id, &method, &params)?;
                }
                Ok(Event::Malformed { line, reason }) => {
                    self.record(&[translate::malformed_line_event(&line, &reason)])?;
                }
                Ok(Event::OutputClosed(why)) => {
                    return Err(format!(
                        "codex app-server exited while the turn was still running{why}"
                    ));
                }
                Ok(Event::ExitDrainComplete) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(
                        "codex app-server exited while the turn was still running".to_owned()
                    );
                }
                Err(RecvTimeoutError::Timeout) => {}
            }

            // The files are looked at on a clock rather than per message, so a chatty turn does
            // not stat the cancel marker hundreds of times a second.
            if last_control_poll_at.elapsed() < POLL_INTERVAL || self.pending_turn_end.is_some() {
                continue;
            }
            last_control_poll_at = Instant::now();
            let Some(turn_id) = self.translator.turn_id().cloned() else {
                continue;
            };
            let target = TurnTarget {
                thread: thread_id,
                turn: &turn_id,
            };
            if self.notice_cancellation() {
                if self
                    .cancellation_observed_at
                    .is_some_and(|at| at.elapsed() >= self.timeouts.interrupt_completion)
                {
                    self.mark_pending_steers_unconfirmed(
                        "the turn was cancelled before codex answered it",
                    );
                    let _ = self.save_closing_message();
                    return Ok(DriverOutcome::Cancelled);
                }
                if !interrupt_attempted {
                    interrupt_attempted = true;
                    // Steers still waiting were never sent, and are not sent into a turn that is
                    // being stopped.
                    for claim in self.mailbox.claim_all() {
                        self.write_steer_reply(
                            &claim,
                            &SteerReply::Rejected {
                                reason: "the consultation is being cancelled".to_owned(),
                            },
                        );
                    }
                    let interrupt = protocol::turn_interrupt_params(target);
                    if let Ok(id) = self.enqueue_request::<methods::TurnInterrupt>(&interrupt) {
                        self.pending.insert(id, PendingRequest::Interrupt);
                    }
                }
                continue;
            }
            for claim in self.mailbox.claim_all() {
                let params = protocol::turn_steer_params(target, &claim.text);
                let sent = self.enqueue_request::<methods::TurnSteer>(&params);
                match sent {
                    Ok(id) => {
                        self.pending.insert(
                            id,
                            PendingRequest::Steer {
                                claim,
                                expected_turn_id: turn_id.clone(),
                            },
                        );
                    }
                    Err(reason) => self.write_steer_reply(&claim, &SteerReply::Rejected { reason }),
                }
            }
        }
    }

    /// Settle the steers still in flight, then write the end of the turn.
    fn close(&mut self, turn_end: &PendingTurnEnd) -> Result<DriverOutcome, String> {
        let deadline = Instant::now() + self.timeouts.steer_settlement;
        while self
            .pending
            .values()
            .any(|request| matches!(request, PendingRequest::Steer { .. }))
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.events.recv_timeout(remaining) {
                Ok(Event::Message(Message::Response { id, outcome })) => {
                    if let Some(request) = self.pending.remove(&id) {
                        self.resolve(request, outcome)?;
                    }
                }
                Ok(Event::Message(Message::Request { id, method, params })) => {
                    self.decline(&id, &method, &params)?;
                }
                // The turn is over, and what arrives now is not part of it.
                Ok(Event::Message(Message::Notification { .. }) | Event::Malformed { .. }) => {}
                Ok(Event::OutputClosed(_) | Event::ExitDrainComplete) | Err(_) => break,
            }
        }
        self.mark_pending_steers_unconfirmed("the turn ended before codex answered it");
        self.save_closing_message()?;
        self.record(&turn_end.events)?;
        Ok(match turn_end.status {
            TurnEndStatus::Interrupted => DriverOutcome::Cancelled,
            TurnEndStatus::Completed | TurnEndStatus::Failed => DriverOutcome::TurnEnded,
        })
    }

    /// Act on the server's answer to one of the driver's own requests.
    fn resolve(
        &mut self,
        request: PendingRequest,
        outcome: Result<Value, RpcError>,
    ) -> Result<(), String> {
        match (request, outcome.map_err(|error| refusal(&error))) {
            (
                PendingRequest::Steer {
                    claim,
                    expected_turn_id,
                },
                Ok(result),
            ) => {
                // An answer that does not say the steer joined the turn it was sent into does not
                // confirm it: codex may have taken it, or not, or into another turn.
                // The sender hears so even when the capture cannot record why.
                match methods::TurnSteer::decode_response(&result) {
                    Ok(answer) if answer.turn_id == expected_turn_id.as_str() => {}
                    Ok(answer) => {
                        let reason = format!(
                            "codex answered for turn `{}`, not the turn `{expected_turn_id}` it \
                             was sent into",
                            answer.turn_id
                        );
                        self.write_steer_reply(&claim, &SteerReply::Unconfirmed { reason });
                        return Ok(());
                    }
                    Err(reason) => {
                        let reason = reason.to_string();
                        let recorded = self.record(&[translate::undecoded_message_event(
                            methods::TurnSteer::METHOD,
                            &reason,
                            &result,
                        )]);
                        self.write_steer_reply(&claim, &SteerReply::Unconfirmed { reason });
                        return recorded;
                    }
                }
                // Recorded before the sender hears of it, so a transcript read after a
                // successful `steer` always holds the instruction.
                if let Err(reason) = self.record(&[translate::accepted_steer_event(&claim.text)]) {
                    self.write_steer_reply(
                        &claim,
                        &SteerReply::Unconfirmed {
                            reason: format!(
                                "codex accepted it, but agentmux could not record it: {reason}"
                            ),
                        },
                    );
                    return Err(reason);
                }
                self.write_steer_reply(&claim, &SteerReply::Accepted);
                Ok(())
            }
            (PendingRequest::Steer { claim, .. }, Err(reason)) => {
                self.write_steer_reply(&claim, &SteerReply::Rejected { reason });
                Ok(())
            }
            (PendingRequest::Interrupt, Ok(_)) => Ok(()),
            (PendingRequest::Interrupt, Err(reason)) => {
                let note = self
                    .translator
                    .warning_event(&format!("codex refused to interrupt the turn: {reason}"));
                self.record(&[note])
            }
        }
    }

    /// Write the turn's closing message to the file a reply is recovered from.
    ///
    /// Written before the terminal event: a turn whose only answer is a plan has no reply in the
    /// capture, and a reader that sees the turn end must find the plan already there, or a
    /// follow-up started in between leaves it unrecoverable.
    /// When the file cannot be written, a plan is written into the capture as the reply instead,
    /// because nothing else would keep it; a reply is already there.
    /// Fails when neither could keep the plan, so the turn is not recorded as answered without
    /// it.
    /// Once kept, it is not kept again, so a plan written into the capture appears there once.
    fn save_closing_message(&mut self) -> Result<(), String> {
        if self.closing_message_saved {
            return Ok(());
        }
        let Some(text) = self.translator.closing_message() else {
            return Ok(());
        };
        if let Err(error) = std::fs::write(self.plan.last_message_path(), text) {
            eprintln!(
                "agentmux: cannot write the closing message to {}: {error}",
                self.plan.last_message_path().display()
            );
            if let Some(plan) = self.translator.uncaptured_plan().map(str::to_owned) {
                let kept = self.translator.agent_message_event(&plan);
                self.record(&[kept])?;
            }
        }
        self.closing_message_saved = true;
        Ok(())
    }

    /// Answer every steer sent but never answered: codex may have taken it, so neither delivery
    /// nor refusal can be claimed.
    fn mark_pending_steers_unconfirmed(&mut self, why: &str) {
        let pending = std::mem::take(&mut self.pending);
        for request in pending.into_values() {
            if let PendingRequest::Steer { claim, .. } = request {
                self.write_steer_reply(
                    &claim,
                    &SteerReply::Unconfirmed {
                        reason: why.to_owned(),
                    },
                );
            }
        }
    }

    fn write_steer_reply(&self, claim: &ClaimedSteer, reply: &SteerReply) {
        if let Err(error) = self.mailbox.write_reply(claim, reply) {
            eprintln!("agentmux: cannot answer a steered message: {error}");
        }
    }
}

/// What the server said when it refused a request, as the transcript and the caller are told.
fn refusal(error: &RpcError) -> String {
    if error.message.is_empty() {
        "the server gave no reason".to_owned()
    } else {
        error.message.clone()
    }
}

struct Server {
    child: Child,
    to_server: PipeWriter,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    start_time_secs: u64,
}

/// Start `codex app-server` for the plan, in the turn's working directory.
///
/// Its stderr is the driver's own, which is the turn's `stderr.log`.
/// The environment is the driver's, which the launcher built; under a subscription it is
/// scrubbed again here, because this is where the process that could spend money is made.
/// It stays in the driver's process group, so a driver the store has to signal takes the server
/// with it.
/// Returns the server and the write end of its input, a pipe of the driver's own so the driver
/// can hold the input open through a second handle.
fn spawn_server(plan: &DriverPlan) -> std::io::Result<Server> {
    let mut env: std::collections::BTreeMap<String, String> = crate::host_env();
    if plan.billing == Billing::Subscription {
        scrub_billing_env(&mut env);
    }
    let spawn = |program: &str| -> std::io::Result<Server> {
        let (input, to_server) = std::io::pipe()?;
        let child = std::process::Command::new(program)
            .args(&plan.settings.args)
            .current_dir(&plan.cwd)
            .env_clear()
            .envs(&env)
            .stdin(Stdio::from(input))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(Server { child, to_server })
    };
    let program = plan.settings.program.as_str();
    match spawn(program) {
        // An npm-installed CLI on Windows is a `.cmd` shim, which `Command` does not find by its
        // bare name.
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            spawn(&format!("{program}.cmd")).map_err(|_| error)
        }
        spawned => spawned,
    }
}

/// Close the server's input, wait for it to leave, and stop whatever it left running.
///
/// The commands a turn runs are the server's descendants, and codex's sandbox starts each in a
/// session of its own, which no process-group signal reaches and which outlives the server as an
/// orphan.
/// So what runs below the driver is read while `input` still holds the server open, and read again
/// until nothing is left; `recorded_descendants` is what [`DescendantMonitor`] recorded while the
/// turn ran, which reaches a command whose server crashed before then.
/// On Linux the driver adopts the server's orphans (see [`adopt_orphans`]), so such a command, or
/// one a command started while it was being stopped, is still found below it.
/// Each process is asked to stop and then made to, recognised by its pid and start time so a pid
/// reused meanwhile is left alone.
/// Elsewhere an orphan neither reading saw is out of reach: one a crashed server's command
/// started after the last record.
/// Once the polite signal's grace is over, every read for [`KILL_GRACE`] longer kills what it
/// finds, so a process forked during the sweep is still caught.
fn stop_server(child: &mut Child, input: PipeWriter, recorded_descendants: Vec<ProcessIdentity>) {
    let driver_pid = std::process::id();
    let mut known_descendants = recorded_descendants;
    add_unseen(&mut known_descendants, descendants(driver_pid));
    // The server is stopped and waited for through `child`; once waited for, its pid is free
    // for another process, which a start time to the second cannot tell apart.
    let server_pid = child.id();
    known_descendants.retain(|process| process.pid != server_pid);
    drop(input);
    let deadline = Instant::now() + SERVER_EXIT_GRACE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let polite_until = Instant::now() + SWEEP_GRACE;
    let forceful_until = polite_until + KILL_GRACE;
    let mut signalled_processes: Vec<ProcessIdentity> = Vec::new();
    loop {
        let mut processes_to_check = descendants(driver_pid);
        add_unseen(
            &mut processes_to_check,
            known_descendants
                .iter()
                .chain(&signalled_processes)
                .copied(),
        );
        if !signal_live_processes(&processes_to_check, None) {
            return;
        }
        let unsignalled_processes: Vec<ProcessIdentity> = processes_to_check
            .iter()
            .filter(|process| !signalled_processes.contains(process))
            .copied()
            .collect();
        let now = Instant::now();
        if now < polite_until {
            signal_live_processes(&unsignalled_processes, Some(sysinfo::Signal::Term));
        } else {
            // Every pass from here kills all it finds, so what was forked since the last read is
            // killed by the next one.
            signal_live_processes(&processes_to_check, Some(sysinfo::Signal::Kill));
            if now >= forceful_until {
                return;
            }
        }
        signalled_processes.extend(unsignalled_processes);
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Add to `known` each of `processes` it does not hold yet.
fn add_unseen(
    known: &mut Vec<ProcessIdentity>,
    processes: impl IntoIterator<Item = ProcessIdentity>,
) {
    for process in processes {
        if !known.contains(&process) {
            known.push(process);
        }
    }
}

/// Make the driver the parent of whatever its descendants orphan, so [`stop_server`] can still
/// find a command whose server exited before it.
///
/// Linux only, where the kernel offers it; elsewhere an orphan goes to the system, and nothing
/// below the driver says it was there.
/// Returns whether the driver now adopts.
fn adopt_orphans() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    match nix::sys::prctl::set_child_subreaper(true) {
        Ok(()) => return true,
        Err(error) => {
            eprintln!("agentmux: cannot adopt what codex app-server orphans: {error}");
        }
    }
    false
}

/// Watches what runs below the driver while the turn runs.
///
/// Where the driver cannot adopt, it records the driver's descendants every few ticks, so the
/// sweep in [`stop_server`] can still stop a command whose server crashed and left it to the
/// system.
/// Where it adopts, such a command stays below the driver, and the watch instead waits for each
/// adopted orphan as it exits.
/// An exited process stays in the process table until its parent waits for it, and a long turn
/// whose commands keep forking short-lived helpers would otherwise fill that table with them,
/// until the consultation can start no process at all.
/// The server is never waited for here, so its exit status stays where `Child` reads it.
/// Stopped before the sweep: an orphan nobody waited for keeps its pid, so the pid the sweep read
/// cannot pass to another process before it is signalled.
struct DescendantMonitor {
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<Vec<ProcessIdentity>>,
}

impl DescendantMonitor {
    /// How many ticks pass between two records of the driver's descendants.
    const DESCENDANT_SNAPSHOT_TICKS: u32 = 10;

    fn start(server_pid: u32, adopting: bool) -> Self {
        let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stopping);
        let driver_pid = std::process::id();
        let thread = std::thread::spawn(move || {
            let mut recorded_descendants: Vec<ProcessIdentity> = Vec::new();
            let mut ticks = 0_u32;
            while !flag.load(std::sync::atomic::Ordering::Acquire) {
                if adopting {
                    reap_exited_orphans(server_pid);
                } else if ticks.is_multiple_of(Self::DESCENDANT_SNAPSHOT_TICKS) {
                    add_unseen(&mut recorded_descendants, descendants(driver_pid));
                }
                ticks = ticks.wrapping_add(1);
                std::thread::sleep(POLL_INTERVAL);
            }
            recorded_descendants
        });
        Self { stopping, thread }
    }

    /// Stop watching, and return every descendant it recorded.
    fn stop(self) -> Vec<ProcessIdentity> {
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        self.thread.join().unwrap_or_default()
    }
}

/// Wait for every child of the driver's that has exited, except the server.
///
/// An exited child is looked at before it is waited for, so the server's own exit is left
/// alone; an orphan behind an exited server is left for the system, which waits for it once the
/// driver, which leaves soon after its server, has gone.
fn reap_exited_orphans(server_pid: u32) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use nix::sys::wait::{Id, WaitPidFlag, waitid, waitpid};

        let Ok(server_pid) = i32::try_from(server_pid).map(nix::unistd::Pid::from_raw) else {
            return;
        };
        let exited = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
        while let Ok(status) = waitid(Id::All, exited)
            && let Some(pid) = status.pid()
            && pid != server_pid
        {
            let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG));
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = server_pid;
}

/// On Windows, put the driver into a job object that kills everything still in it when the
/// driver exits, so what the server leaves running goes with it.
///
/// Every process the driver or its descendants start joins the job, and none is ever looked up
/// by its parent: Windows keeps the parent pid a process was created with after that parent has
/// gone, and a pid of ours that reuses it would claim someone else's process.
/// A process codex deliberately starts outside its own job is let go, as codex intends.
/// The handle is never closed by hand, so the job outlives everything the driver does and ends
/// with its process.
/// A driver that cannot make the job says so on stderr, and stops only the server itself.
fn contain_descendants() {
    #[cfg(windows)]
    {
        let mut limits = win32job::ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close().limit_breakaway_ok();
        let contained = win32job::Job::create_with_limit_info(&limits)
            .and_then(|job| job.assign_current_process().map(|()| job.into_handle()));
        if let Err(error) = contained {
            eprintln!("agentmux: cannot hold what codex app-server starts in a job: {error}");
        }
    }
}

/// A fresh read of the process table, with each process's parent and start time: of `pids`, or
/// of every process when `pids` is `None`.
///
/// Threads are left out: Linux lists each as a child of its process, and a signal to one is a
/// signal to the whole process — the driver's own included.
fn process_table(pids: Option<&[sysinfo::Pid]>) -> sysinfo::System {
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        pids.map_or(
            sysinfo::ProcessesToUpdate::All,
            sysinfo::ProcessesToUpdate::Some,
        ),
        true,
        sysinfo::ProcessRefreshKind::nothing().without_tasks(),
    );
    system
}

/// Every process below `root`, with the start time that tells it apart from a later holder of
/// its pid.
///
/// A process counts as a child only if it started no earlier than its parent, so a pid reused
/// since cannot claim an older process.
/// Always empty on Windows, where the job object of [`contain_descendants`] stops them instead
/// and a recorded parent pid proves nothing.
fn descendants(root: u32) -> Vec<ProcessIdentity> {
    if cfg!(windows) {
        return Vec::new();
    }
    let system = process_table(None);
    let Some(root_started) = system
        .process(sysinfo::Pid::from_u32(root))
        .map(sysinfo::Process::start_time)
    else {
        return Vec::new();
    };
    let mut found: Vec<ProcessIdentity> = Vec::new();
    let mut parents = vec![ProcessIdentity {
        pid: root,
        start_time_secs: root_started,
    }];
    while let Some(parent) = parents.pop() {
        for (pid, process) in system.processes() {
            let pid = pid.as_u32();
            let start_time_secs = process.start_time();
            if process.parent().map(sysinfo::Pid::as_u32) == Some(parent.pid)
                && start_time_secs >= parent.start_time_secs
                && pid != root
                && !found.iter().any(|known| known.pid == pid)
            {
                let identity = ProcessIdentity {
                    pid,
                    start_time_secs,
                };
                found.push(identity);
                parents.push(identity);
            }
        }
    }
    found
}

/// Returns whether any process of `tree` is still alive, and sends `signal`, when given, to each
/// live one.
///
/// A process counts as alive while its pid still has the start time recorded for it and it has
/// not exited.
///
/// A pid whose start time no longer matches belongs to someone else and is left alone, and a
/// platform without the polite signal gets the forceful one.
fn signal_live_processes(tree: &[ProcessIdentity], signal: Option<sysinfo::Signal>) -> bool {
    if tree.is_empty() {
        return false;
    }
    let pids: Vec<sysinfo::Pid> = tree
        .iter()
        .map(|process| sysinfo::Pid::from_u32(process.pid))
        .collect();
    let system = process_table(Some(&pids));
    let mut any = false;
    for &ProcessIdentity {
        pid,
        start_time_secs,
    } in tree
    {
        let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) else {
            continue;
        };
        let exited = matches!(
            process.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        );
        if process.start_time() != start_time_secs || exited {
            continue;
        }
        any = true;
        if let Some(signal) = signal
            && process.kill_with(signal).is_none()
        {
            process.kill();
        }
    }
    any
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use googletest::prelude::*;

    use super::confirm_launch;
    use crate::launch::Launched;
    use crate::run::layout::TurnDir;

    /// Record `pid` as the turn's child, whole, as the store does once the spawn returns.
    fn record_launch(turn: &TurnDir, pid: u32) -> Result<()> {
        let record = serde_json::json!({
            "launched": Launched {
                pid,
                process_group: Some(pid),
                started: None,
            },
            "started_at": "2026-10-19T12:00:00Z",
        });
        crate::atomic_file::write_whole(&turn.launch(), record.to_string().as_bytes()).or_fail()
    }

    /// A driver goes on only when its turn's launch record names it: a record naming another
    /// process, one that cannot be parsed, and one that never appears all stop it.
    #[gtest]
    fn a_driver_runs_only_the_turn_its_launch_record_names() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let turn = TurnDir {
            root: dir.path().to_path_buf(),
        };
        let wait = Duration::from_millis(50);

        let missing = confirm_launch(&turn, wait);
        assert_that!(missing, err(contains_substring("no launch record")));

        record_launch(&turn, std::process::id().wrapping_add(1))?;
        let other = confirm_launch(&turn, wait);
        assert_that!(other, err(contains_substring("not as this driver")));

        std::fs::write(turn.launch(), "{").or_fail()?;
        let unreadable = confirm_launch(&turn, wait);
        assert_that!(unreadable, err(contains_substring("cannot read")));

        record_launch(&turn, std::process::id())?;
        assert_that!(confirm_launch(&turn, wait), ok(()));

        // A record written after the driver first looks is still found.
        std::fs::remove_file(turn.launch()).or_fail()?;
        let recorder = std::thread::spawn({
            let turn = turn.clone();
            move || {
                std::thread::sleep(Duration::from_millis(100));
                record_launch(&turn, std::process::id())
            }
        });
        assert_that!(confirm_launch(&turn, Duration::from_secs(10)), ok(()));
        recorder.join().ok().or_fail()??;
        Ok(())
    }
}
