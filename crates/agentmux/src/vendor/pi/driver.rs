//! The driver: one pi turn over `pi --mode rpc`, so it can be steered while it runs.
//!
//! Launched as `agentmux pi-rpc <plan>` in pi's place, as [`crate::appserver`] is for Codex, and a
//! process like it, as [`crate::driver`] describes.
//!
//! ```text
//! driver                                     pi --mode rpc
//!   get_state                            ->  the session id, which a resume must match
//!   get_available_models, set_model      ->  the one model named `provider/id`
//!   set_thinking_level, get_state        ->  the effort, when one was named, read back
//!   prompt (the question)                ->
//!                                        <-  agent_start: steering opens
//!   steer (from the mailbox, one by one) ->
//!                                        <-  message_start (user): the oldest steer was taken
//!   abort (when `cancelling`)            ->
//!                                        <-  agent_settled: the turn is over
//!   close stdin, wait for exit, stop what it left running
//! ```
//!
//! Every event pi writes goes into the capture as it arrives, except its answers to the driver's
//! commands; [`super::fold`] reads it.
//!
//! The driver is the only source of pi's queued messages: `--no-extensions` keeps out every
//! extension that could add one.
//! So every user message pi starts after the question is the steer that has waited longest, and
//! is recorded and answered as accepted then.
//! A steer pi has not taken when the turn settles never will be, and is rejected; pi would only
//! hold it for a later prompt that never comes.
//!
//! Closing pi's input aborts a running turn, so it stays open until the turn has settled or been
//! cancelled.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read as _, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::delegate::{Billing, Effort, ModelId, SessionRef};
use crate::driver::mailbox::{ClaimedSteer, Mailbox, SteerReply};
use crate::driver::{
    DescendantMonitor, DriverOutcome, DriverTimeouts, LAUNCH_RECORD_WAIT, POLL_INTERVAL, Server,
    adopt_orphans, confirm_launch, contain_descendants, keep_left_running, spawn_server,
    stop_server, write_server,
};
use crate::run::layout::TurnDir;

/// The subcommand of the `agentmux` executable that runs [`drive`].
///
/// Hidden from its help: nothing but the run store launches it.
pub const SUBCOMMAND: &str = "pi-rpc";

/// Everything the driver needs to run one pi turn, written into the turn directory before the
/// launch.
///
/// The question is not here: it arrives on the driver's stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiPlan {
    /// The executable to run, `pi`.
    pub program: String,
    /// Its arguments, to which the driver adds where the session is kept.
    pub args: Vec<String>,
    /// The model, as `provider/id`.
    pub model: ModelId,
    /// The thinking level, or pi's own when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Where pi saves the session, and the id a resume must find in it.
    pub session: PiSession,
    /// The directory the delegate reads the project from.
    pub cwd: PathBuf,
    /// The pricing the turn may run on; under [`Billing::Subscription`] the driver scrubs pi's
    /// environment again.
    pub billing: Billing,
    /// The turn's directory, which holds every file the driver reads or writes besides its
    /// capture.
    pub turn_dir: PathBuf,
}

/// The session one pi turn saves into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiSession {
    /// The session file, the same for every turn of a consultation.
    pub file: PathBuf,
    /// The session a follow-up continues, which the file must hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_id: Option<SessionRef>,
}

impl PiPlan {
    /// Read a plan the run store wrote.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or is not a plan.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    fn turn(&self) -> TurnDir {
        TurnDir {
            root: self.turn_dir.clone(),
        }
    }

    /// pi's arguments with the session file and, as its session directory, the file's own.
    fn command_args(&self) -> Vec<String> {
        let file = &self.session.file;
        let dir = file.parent().unwrap_or(file);
        let mut args = self.args.clone();
        for (flag, path) in [("--session-dir", dir), ("--session", file)] {
            args.push(flag.to_owned());
            args.push(path.to_string_lossy().into_owned());
        }
        args
    }
}

/// Run the turn a plan describes, as the `pi-rpc` subcommand.
///
/// The question is read from stdin and the capture is written to stdout, which the launcher has
/// pointed at the turn's files; a turn that breaks says why on stderr.
/// Nothing is started until the turn's launch record names this process.
/// Returns the process's exit status.
#[must_use]
pub fn drive(plan_path: &Path) -> i32 {
    let plan = match PiPlan::read(plan_path) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!(
                "agentmux: cannot read the pi plan at {}: {err}",
                plan_path.display()
            );
            return 2;
        }
    };
    if let Err(reason) = confirm_launch(&plan.turn(), LAUNCH_RECORD_WAIT) {
        eprintln!("agentmux: {reason}, so pi was not started");
        return 2;
    }
    let mut question = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut question) {
        eprintln!("agentmux: cannot read the question: {err}");
        return 2;
    }

    let adopting = adopt_orphans();
    contain_descendants();
    let Server {
        mut child,
        to_server,
    } = match spawn_server(&plan.program, &plan.command_args(), &plan.cwd, plan.billing) {
        Ok(spawned) => spawned,
        Err(err) => {
            eprintln!(
                "agentmux: cannot run `{}`: {err}. Is it installed and on PATH?",
                plan.program
            );
            return 1;
        }
    };
    let descendant_monitor =
        (!cfg!(windows)).then(|| DescendantMonitor::start(child.id(), adopting));
    // pi's input stays open through a second handle until `stop_server` has read what pi left
    // running.
    let (Some(from_server), Ok(input)) = (child.stdout.take(), to_server.try_clone()) else {
        let left_running = stop_server(
            &mut child,
            to_server,
            descendant_monitor
                .map(DescendantMonitor::stop)
                .unwrap_or_default(),
        );
        keep_left_running(&plan.turn(), left_running);
        eprintln!("agentmux: pi did not offer stdio");
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
    let left_running = stop_server(&mut child, input, recorded_descendants);
    if outcome != DriverOutcome::Cancelled {
        keep_left_running(&plan.turn(), left_running);
    }
    if let DriverOutcome::Failed(reason) = &outcome {
        eprintln!("agentmux: {reason}");
    }
    outcome.exit_code()
}

/// Run one turn against a pi already connected through `to_server` and `from_server`.
///
/// This is all of [`drive`] but the process: a test drives it against an in-process pi.
/// `server_exited` says whether pi has gone, which its output alone cannot say while something
/// it started still holds that output open.
pub fn drive_with(
    plan: &PiPlan,
    question: &str,
    capture_writer: &mut dyn Write,
    to_server: impl Write + Send + 'static,
    from_server: impl BufRead + Send + 'static,
    server_exited: &mut dyn FnMut() -> bool,
    timeouts: DriverTimeouts,
) -> DriverOutcome {
    let (inbox, events) = channel();
    std::thread::spawn(move || read_server(from_server, &inbox));
    let (outbox, queue) = channel::<String>();
    std::thread::spawn(move || write_server(to_server, &queue));

    let mut driver = TurnDriver {
        plan,
        capture_writer,
        outbox,
        events,
        server_exited,
        timeouts,
        mailbox: Mailbox::new(plan.turn().steer_dir()),
        next_id: 0,
        cancellation_observed_at: None,
        session_id: None,
        run_started: false,
        question_echoed: false,
        settled: None,
        in_flight: None,
        queued: VecDeque::new(),
    };
    let ended = match driver.set_up(question) {
        Ok(prompt_id) => driver.follow(&prompt_id),
        Err(SetupExit::Cancelled) => Ok(DriverOutcome::Cancelled),
        Err(SetupExit::Failed(reason)) => Err(reason),
    };
    // Closed however the turn ended, a pi that died included, and the turn's own failure is the
    // one reported.
    let closed = driver.close();
    let outcome = ended
        .and_then(|outcome| closed.map(|()| outcome))
        .unwrap_or_else(DriverOutcome::Failed);
    driver.answer_unsettled_steers(&outcome);
    outcome
}

/// Hand every line pi writes to the driver, then say why it stopped.
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
                let line = line.trim_end_matches(['\n', '\r']).to_owned();
                if inbox.send(Event::Line(line)).is_err() {
                    return;
                }
            }
            Err(err) => {
                let _ = inbox.send(Event::OutputClosed(format!(": {err}")));
                return;
            }
        }
    }
}

/// What the reading thread hands over.
enum Event {
    Line(String),
    /// pi's stdout ended, with what the read said about why.
    OutputClosed(String),
}

/// Why setting a turn up stopped.
enum SetupExit {
    /// The caller cancelled before the turn started.
    Cancelled,
    /// pi refused, went away, or the capture could not be written.
    Failed(String),
}

/// How pi settled the turn.
enum Settled {
    /// On its own, with the `agent_settled` line the capture ends in.
    Naturally(String),
    /// Aborted for the cancellation, whose end the store records.
    Aborted,
}

/// The steer sent to pi and not answered yet.
struct InFlight {
    id: String,
    claim: ClaimedSteer,
}

struct TurnDriver<'a> {
    plan: &'a PiPlan,
    capture_writer: &'a mut dyn Write,
    outbox: Sender<String>,
    events: Receiver<Event>,
    server_exited: &'a mut dyn FnMut() -> bool,
    timeouts: DriverTimeouts,
    mailbox: Mailbox,
    next_id: u64,
    /// When the driver first saw the caller's cancellation.
    cancellation_observed_at: Option<Instant>,
    /// The session pi runs, as `get_state` named it.
    session_id: Option<String>,
    /// Whether pi started the turn's run, which is when it takes a steer.
    run_started: bool,
    /// Whether pi has started the question's own user message.
    question_echoed: bool,
    /// How pi settled the turn, once it did.
    settled: Option<Settled>,
    in_flight: Option<InFlight>,
    /// Steers pi queued and has not taken yet, oldest first.
    queued: VecDeque<ClaimedSteer>,
}

impl TurnDriver<'_> {
    /// Append one line to the capture, flushed at once so a `tail` sees it.
    fn record(&mut self, line: &str) -> Result<(), String> {
        writeln!(self.capture_writer, "{line}")
            .and_then(|()| self.capture_writer.flush())
            .map_err(|err| format!("cannot write the event capture: {err}"))
    }

    /// Send `command` with an id of its own, which is returned.
    fn send(&mut self, mut command: Value) -> Result<String, String> {
        self.next_id += 1;
        let id = format!("agentmux-{}", self.next_id);
        if let Some(fields) = command.as_object_mut() {
            fields.insert("id".to_owned(), Value::String(id.clone()));
        }
        self.outbox
            .send(command.to_string())
            .map_err(|_| "pi closed its input".to_owned())?;
        Ok(id)
    }

    /// Whether the caller has cancelled, remembering when the driver first saw it.
    fn notice_cancellation(&mut self) -> bool {
        if self.cancellation_observed_at.is_none() && self.plan.turn().cancelling().exists() {
            self.cancellation_observed_at = Some(Instant::now());
        }
        self.cancellation_observed_at.is_some()
    }

    /// Whether the cancellation's grace is over.
    fn cancellation_grace_over(&self) -> bool {
        self.cancellation_observed_at
            .is_some_and(|at| at.elapsed() >= self.timeouts.interrupt_completion)
    }

    /// The next line from pi, waiting up to `wait`, or why pi has gone.
    ///
    /// pi counts as gone once its output has ended, or once it has exited and said nothing for
    /// `wait`.
    fn receive(&mut self, wait: Duration) -> Result<Option<Value>, String> {
        match self.events.recv_timeout(wait) {
            Ok(Event::Line(line)) => {
                // A line that is no JSON is recorded as it came, for the fold to count.
                let value = serde_json::from_str::<Value>(&line).unwrap_or(Value::Null);
                if text(&value, "type") == "response" {
                    return Ok(Some(value));
                }
                self.handle_event(&line, &value)?;
                Ok(None)
            }
            Ok(Event::OutputClosed(why)) => Err(format!("pi exited before the turn settled{why}")),
            Err(RecvTimeoutError::Timeout) if (self.server_exited)() => {
                Err("pi exited before the turn settled".to_owned())
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err("pi exited before the turn settled".to_owned())
            }
        }
    }

    /// Record one event of pi's, and act on what it says about the turn.
    fn handle_event(&mut self, line: &str, value: &Value) -> Result<(), String> {
        let kind = text(value, "type");
        let role = value.pointer("/message/role").and_then(Value::as_str);
        match kind {
            "agent_start" => self.run_started = true,
            // A run the cancellation aborted ends as the store records it; one that ended on its
            // own ends as pi says.
            "agent_settled"
                if value.get("aborted") == Some(&Value::Bool(true))
                    && self.cancellation_observed_at.is_some() =>
            {
                self.settled = Some(Settled::Aborted);
                return Ok(());
            }
            "agent_settled" => {
                self.settled = Some(Settled::Naturally(line.to_owned()));
                return Ok(());
            }
            _ => {}
        }
        self.record(line)?;
        match (kind, role) {
            ("message_start", Some("user")) if !self.question_echoed => {
                self.question_echoed = true;
            }
            ("message_start", Some("user")) => self.deliver_oldest_steer()?,
            _ => {}
        }
        Ok(())
    }

    /// Record the steer that has waited longest as taken, then tell its sender.
    fn deliver_oldest_steer(&mut self) -> Result<(), String> {
        let claim = match self.queued.pop_front() {
            Some(claim) => claim,
            // pi can take a steer before it has answered the command that sent it.
            None => match self.in_flight.take() {
                Some(InFlight { claim, .. }) => claim,
                None => return Ok(()),
            },
        };
        let steer = json!({"type": "agentmux.steer", "text": claim.text}).to_string();
        if let Err(reason) = self.record(&steer) {
            self.write_steer_reply(
                &claim,
                &SteerReply::Unconfirmed {
                    reason: format!("pi took it, but agentmux could not record it: {reason}"),
                },
            );
            return Err(reason);
        }
        self.write_steer_reply(&claim, &SteerReply::Accepted);
        Ok(())
    }

    /// Answer a response to the steer in flight: pi queued it, or refused it.
    fn resolve_steer(&mut self, response: &Value) {
        let Some(in_flight) = self
            .in_flight
            .take_if(|in_flight| text(response, "id") == in_flight.id)
        else {
            return;
        };
        if response.get("success") == Some(&Value::Bool(true)) {
            self.queued.push_back(in_flight.claim);
        } else {
            let reason = refusal(response);
            self.write_steer_reply(&in_flight.claim, &SteerReply::Rejected { reason });
        }
    }

    /// Send a setup command and wait for pi's answer, recording whatever pi says meanwhile.
    ///
    /// Nothing is sent once the caller has cancelled, and a cancellation ends the wait.
    /// Returns the answer's `data`.
    fn call(&mut self, command: Value) -> Result<Value, SetupExit> {
        let name = text(&command, "type").to_owned();
        if self.notice_cancellation() {
            return Err(SetupExit::Cancelled);
        }
        let id = self.send(command).map_err(SetupExit::Failed)?;
        let deadline = Instant::now() + self.timeouts.setup_response;
        loop {
            if self.notice_cancellation() {
                return Err(SetupExit::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SetupExit::Failed(format!(
                    "pi did not answer `{name}` within {:?}",
                    self.timeouts.setup_response
                )));
            }
            let Some(response) = self
                .receive(remaining.min(POLL_INTERVAL))
                .map_err(SetupExit::Failed)?
            else {
                continue;
            };
            if text(&response, "id") != id {
                continue;
            }
            if response.get("success") != Some(&Value::Bool(true)) {
                return Err(SetupExit::Failed(format!(
                    "pi refused `{name}`: {}",
                    refusal(&response)
                )));
            }
            return Ok(response.get("data").cloned().unwrap_or(Value::Null));
        }
    }

    /// Check the session, choose the model and the effort, and send the question.
    ///
    /// Returns the question's command id, whose answer [`Self::follow`] reads.
    fn set_up(&mut self, question: &str) -> Result<String, SetupExit> {
        let state = self.call(json!({"type": "get_state"}))?;
        let session_id = text(&state, "sessionId").to_owned();
        if let Some(expected) = &self.plan.session.expect_id
            && expected.as_str() != session_id
        {
            return Err(SetupExit::Failed(format!(
                "pi opened the session `{session_id}` from {}, not `{expected}`, which this \
                 follow-up continues",
                self.plan.session.file.display()
            )));
        }
        self.session_id = Some(session_id);

        let available = self.call(json!({"type": "get_available_models"}))?;
        let models = available
            .get("models")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let named = |model: &Value| format!("{}/{}", text(model, "provider"), text(model, "id"));
        let wanted = self.plan.model.as_str();
        let matching: Vec<&Value> = models
            .iter()
            .filter(|model| named(model) == wanted)
            .collect();
        let [model] = matching.as_slice() else {
            let offered: Vec<String> = models.iter().map(named).collect();
            return Err(SetupExit::Failed(format!(
                "`{wanted}` names {} of the models pi offers, which are: {}",
                matching.len(),
                offered.join(", ")
            )));
        };
        self.call(json!({
            "type": "set_model",
            "provider": text(model, "provider"),
            "modelId": text(model, "id"),
        }))?;

        if let Some(effort) = &self.plan.effort {
            self.call(json!({"type": "set_thinking_level", "level": effort.as_str()}))?;
            let state = self.call(json!({"type": "get_state"}))?;
            let effective = text(&state, "thinkingLevel");
            if effective != effort.as_str() {
                let warning = json!({
                    "type": "agentmux.warning",
                    "message": format!(
                        "pi runs this turn at thinking level `{effective}`, not the `{effort}` \
                         it was asked for"
                    ),
                });
                self.record(&warning.to_string())
                    .map_err(SetupExit::Failed)?;
            }
        }
        if self.notice_cancellation() {
            return Err(SetupExit::Cancelled);
        }
        self.send(json!({"type": "prompt", "message": question}))
            .map_err(SetupExit::Failed)
    }

    /// Follow the turn from its question until it settles, steering it and honouring a
    /// cancellation.
    ///
    /// The question's answer, `prompt_id`'s, fails the turn unless pi started a run for it, and is
    /// waited for as long as a setup answer is.
    /// A cancellation aborts the run once pi has said it started; pi takes its commands without
    /// waiting for the one before, so an earlier `abort` could find it idle and leave the run to
    /// start anyway.
    fn follow(&mut self, prompt_id: &str) -> Result<DriverOutcome, String> {
        let prompt_deadline = Instant::now() + self.timeouts.setup_response;
        let mut run_confirmed = false;
        let mut abort_sent = false;
        let mut last_control_poll_at = Instant::now();
        loop {
            if let Some(response) = self.receive(POLL_INTERVAL)? {
                if text(&response, "id") == prompt_id {
                    started(&response)?;
                    run_confirmed = true;
                } else {
                    self.resolve_steer(&response);
                }
            }
            match self.settled {
                Some(Settled::Naturally(_)) => return Ok(DriverOutcome::TurnEnded),
                Some(Settled::Aborted) => return Ok(DriverOutcome::Cancelled),
                None => {}
            }
            // The files are looked at on a clock rather than per event.
            if last_control_poll_at.elapsed() < POLL_INTERVAL {
                continue;
            }
            last_control_poll_at = Instant::now();
            if self.notice_cancellation() {
                if self.cancellation_grace_over() {
                    return Ok(DriverOutcome::Cancelled);
                }
                if run_confirmed && !abort_sent {
                    abort_sent = true;
                    self.send(json!({"type": "abort"}))?;
                }
                continue;
            }
            if !run_confirmed && Instant::now() >= prompt_deadline {
                return Err(format!(
                    "pi did not answer `prompt` within {:?}",
                    self.timeouts.setup_response
                ));
            }
            if self.run_started
                && self.in_flight.is_none()
                && let Some(claim) = self.mailbox.claim_next()
            {
                match self.send(json!({"type": "steer", "message": claim.text})) {
                    Ok(id) => self.in_flight = Some(InFlight { id, claim }),
                    Err(reason) => self.write_steer_reply(&claim, &SteerReply::Rejected { reason }),
                }
            }
        }
    }

    /// Name the session in the capture if pi saved it, then record the turn's end, which the fold
    /// stops at.
    ///
    /// Read from the file's header once the turn is over, because pi appends to the file after it
    /// reports a message, so a turn that saved nothing advertises nothing to resume.
    /// A cancelled turn names it too, which is how the store sees a follow-up kept its session.
    fn close(&mut self) -> Result<(), String> {
        let mut header = String::new();
        let saved = std::fs::File::open(&self.plan.session.file)
            .and_then(|file| BufReader::new(file).read_line(&mut header))
            .is_ok_and(|_| {
                serde_json::from_str::<Value>(&header).is_ok_and(|header| {
                    text(&header, "type") == "session"
                        && Some(text(&header, "id")) == self.session_id.as_deref()
                })
            });
        if let (true, Some(id)) = (saved, self.session_id.clone()) {
            self.record(&json!({"type": "session", "id": id}).to_string())?;
        }
        if let Some(Settled::Naturally(line)) = &self.settled {
            let line = line.clone();
            self.record(&line)?;
        }
        Ok(())
    }

    /// Answer every steer pi was sent and was not seen to take, and every request still waiting in
    /// the mailbox, which is rejected.
    ///
    /// A steer sent is rejected only once pi settled the turn without taking it; a turn that ended
    /// without `agent_settled` may have had it taken unseen, so it is unconfirmed.
    fn answer_unsettled_steers(&mut self, outcome: &DriverOutcome) {
        let why = match (&self.settled, outcome) {
            (Some(_), _) => "the turn settled before pi took it",
            (None, DriverOutcome::Failed(_)) => "the turn broke before pi was seen to take it",
            (None, _) => "the consultation was cancelled before pi was seen to take it",
        };
        let reason = why.to_owned();
        let sent_reply = if self.settled.is_some() {
            SteerReply::Rejected { reason }
        } else {
            SteerReply::Unconfirmed { reason }
        };
        let sent: Vec<ClaimedSteer> = self
            .in_flight
            .take()
            .map(|in_flight| in_flight.claim)
            .into_iter()
            .chain(self.queued.drain(..))
            .collect();
        for claim in sent {
            self.write_steer_reply(&claim, &sent_reply);
        }
        for claim in self.mailbox.claim_all() {
            self.write_steer_reply(
                &claim,
                &SteerReply::Rejected {
                    reason: why.to_owned(),
                },
            );
        }
    }

    fn write_steer_reply(&self, claim: &ClaimedSteer, reply: &SteerReply) {
        if let Err(err) = self.mailbox.write_reply(claim, reply) {
            eprintln!("agentmux: cannot answer a steered message: {err}");
        }
    }
}

/// The string at `key` of `value`, or nothing.
fn text<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Fail the turn unless pi's answer to the question says it started a run.
fn started(response: &Value) -> Result<(), String> {
    if response.get("success") != Some(&Value::Bool(true)) {
        return Err(format!("pi refused the question: {}", refusal(response)));
    }
    match response
        .pointer("/data/disposition")
        .and_then(Value::as_str)
    {
        Some("started") => Ok(()),
        disposition => Err(format!(
            "pi did not start a run for the question: it answered `{}`",
            disposition.unwrap_or("no disposition")
        )),
    }
}

/// What pi said when it refused a command.
fn refusal(response: &Value) -> String {
    match text(response, "error") {
        "" => "pi gave no reason".to_owned(),
        error => error.to_owned(),
    }
}
