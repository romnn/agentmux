//! A Claude turn's input, held by the turn's supervisor: the question, and then each steer,
//! written to Claude's stdin as stream-json lines.
//!
//! The supervisor makes the pipe before it starts Claude, keeps the write end, and steps this from
//! its watch loop, which looks for a cancellation and for Claude's exit between steps:
//!
//! 1. A line the writer finished with is collected, and a steer it could not write is rejected.
//! 2. The lines Claude has completed in its capture since the last step are read, as far as the
//!    capture reached when this step began, and a `result` among them closes the input.
//! 3. While the input is open and nothing is being written, the oldest waiting request is claimed
//!    and handed to the writer.
//!
//! The writer is a thread of its own, which writes one line at a time, so a Claude that stops
//! reading its input never keeps the supervisor from seeing a cancellation or Claude's exit.
//! Closing the input drops the channel to the writer, which drops the pipe once the line in
//! progress is through, and Claude reads the end of its input.
//!
//! Nothing here decides whether Claude took a steer.
//! Claude's echo of it in the capture does, which `steer` reads from the turn's fold, so a steer
//! is reported taken exactly when the transcript shows it.

use std::io::{Read as _, Seek as _, Write as _};
use std::path::PathBuf;
use std::sync::mpsc;

use serde_json::Value;
use uuid::Uuid;

use crate::driver::mailbox::{ClaimedSteer, Mailbox, SteerReply};
use crate::vendor::claude::fold::InputBoundary;

use super::RunStore;
use super::id::RunId;
use super::layout::TurnDir;

/// Why a request still waiting when Claude reports its result is rejected.
pub(super) const FINISHING: &str = "Claude has reported its result and takes nothing more";

/// Why a request still waiting when the turn is cancelled is rejected.
pub(super) const CANCELLING: &str = "the turn is being cancelled";

/// Why a request still waiting when Claude has exited is rejected.
pub(super) const EXITED: &str = "Claude has exited";

/// The input of one Claude turn, while its supervisor watches the turn.
pub(super) struct ClaudeInput {
    mailbox: Mailbox,
    capture: PathBuf,
    /// How far the capture has been read, which is always the end of a line.
    read_to: u64,
    /// Hands lines to the writer, until the input is closed.
    lines: Option<mpsc::Sender<Vec<u8>>>,
    /// What the writer made of each line, in order.
    written: mpsc::Receiver<std::io::Result<()>>,
    /// What the line the writer has now is, if it has one.
    writing: Option<Writing>,
}

/// What a line handed to the writer is.
enum Writing {
    Question,
    Steer(ClaimedSteer),
}

impl ClaudeInput {
    /// Start writing `question` to Claude's input under `question_id`, through a writer thread
    /// that holds `pipe`, Claude's stdin.
    pub(super) fn start(
        pipe: std::io::PipeWriter,
        question: &str,
        question_id: Uuid,
        turn: &TurnDir,
    ) -> Self {
        let (lines, to_write) = mpsc::channel::<Vec<u8>>();
        let (report, written) = mpsc::channel();
        std::thread::spawn(move || {
            let mut pipe = pipe;
            for line in to_write {
                if report.send(pipe.write_all(&line)).is_err() {
                    break;
                }
            }
        });
        let mut input = Self {
            mailbox: turn.mailbox(),
            capture: turn.events(),
            read_to: 0,
            lines: Some(lines),
            written,
            writing: None,
        };
        input.hand_over(input_line(question_id, question, None), Writing::Question);
        input
    }

    /// One step of serving the input, as the module says.
    ///
    /// A failure is logged rather than returned: the supervisor's work is settling the turn,
    /// and the input failing must not stop it.
    pub(super) fn step(&mut self, store: &RunStore, run_id: &RunId) {
        self.collect_written();
        if self.lines.is_none() {
            return;
        }
        match self.result_written() {
            Ok(true) => return self.close(store, run_id, FINISHING),
            Ok(false) => {}
            Err(err) => tracing::warn!(?err, "cannot read the turn's capture"),
        }
        if self.writing.is_some() {
            return;
        }
        let Some(claim) = self.mailbox.claim_next() else {
            return;
        };
        match claim.uuid() {
            Some(id) => self.hand_over(
                input_line(id, &claim.text, Some("next")),
                Writing::Steer(claim),
            ),
            None => self.reject(&claim, "its id is not a UUID"),
        }
    }

    /// Close the input, and reject every request still waiting because of `why`.
    ///
    /// A steer whose write has failed already, as one does once Claude has gone, is rejected for
    /// that first.
    /// Rejected under the run's lock, which `steer` submits under after checking that the turn
    /// takes input, so a request is either swept here or never submitted.
    pub(super) fn close(&mut self, store: &RunStore, run_id: &RunId, why: &str) {
        self.collect_written();
        self.lines = None;
        let run = store.lock(run_id);
        if let Err(err) = &run {
            tracing::warn!(?err, "rejecting waiting steers without the run's lock");
        }
        for claim in self.mailbox.claim_all() {
            self.reject(&claim, why);
        }
        drop(run);
    }

    /// Give the writer `line`, which is `writing`.
    fn hand_over(&mut self, line: Vec<u8>, writing: Writing) {
        let handed = self
            .lines
            .as_ref()
            .is_some_and(|lines| lines.send(line).is_ok());
        match writing {
            _ if handed => self.writing = Some(writing),
            Writing::Steer(claim) => self.reject(&claim, "Claude's input has closed"),
            Writing::Question => tracing::warn!("the question could not be handed to the writer"),
        }
    }

    /// Take in what the writer made of the line it had, rejecting a steer it could not write.
    fn collect_written(&mut self) {
        while let Ok(written) = self.written.try_recv() {
            match (self.writing.take(), written) {
                (_, Ok(())) | (None, Err(_)) => {}
                (Some(Writing::Steer(claim)), Err(err)) => {
                    self.reject(
                        &claim,
                        &format!("writing it to Claude's input failed: {err}"),
                    );
                }
                (Some(Writing::Question), Err(err)) => {
                    tracing::warn!(?err, "the question could not be written to Claude's input");
                }
            }
        }
    }

    /// Whether the lines Claude completed in its capture since the last look hold a `result`,
    /// read as far as the capture reached when this look began, so a capture that never stops
    /// growing cannot hold up a step.
    fn result_written(&mut self) -> std::io::Result<bool> {
        let len = match std::fs::metadata(&self.capture) {
            Ok(metadata) => metadata.len(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        let Some(unread) = len.checked_sub(self.read_to).filter(|unread| *unread > 0) else {
            return Ok(false);
        };
        let mut file = std::fs::File::open(&self.capture)?;
        file.seek(std::io::SeekFrom::Start(self.read_to))?;
        let mut bytes = Vec::new();
        file.take(unread).read_to_end(&mut bytes)?;
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(false);
        };
        let complete = bytes.get(..end).unwrap_or_default();
        self.read_to = self
            .read_to
            .saturating_add(u64::try_from(end).unwrap_or(u64::MAX))
            .saturating_add(1);
        Ok(complete.split(|byte| *byte == b'\n').any(|line| {
            serde_json::from_slice::<Value>(line)
                .is_ok_and(|line| InputBoundary::of(&line) == Some(InputBoundary::Closes))
        }))
    }

    /// Answer a claimed request as never delivered, because of `why`.
    fn reject(&self, claim: &ClaimedSteer, why: &str) {
        let rejected = SteerReply::Rejected {
            reason: why.to_owned(),
        };
        self.mailbox
            .write_reply(claim, &rejected)
            .inspect_err(|err| tracing::warn!(?err, "cannot reject a steer"))
            .ok();
    }
}

/// One input line: a user message with `text`, sent under `id`, at `priority` when one is given.
///
/// A steer goes `next`, which Claude takes once the tool it is running has finished, before its
/// next model call; never `now`, which would end the CLI turn under way.
fn input_line(id: Uuid, text: &str, priority: Option<&str>) -> Vec<u8> {
    let mut line = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "parent_tool_use_id": null,
        "uuid": id.hyphenated().to_string(),
    });
    if let (Some(priority), Some(fields)) = (priority, line.as_object_mut()) {
        fields.insert("priority".to_owned(), priority.into());
    }
    let mut bytes = line.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use googletest::prelude::*;

    use super::{ClaudeInput, EXITED, Writing, input_line};
    use crate::driver::mailbox::SteerReply;
    use crate::run::layout::TurnDir;
    use crate::run::{RunId, RunStore};
    use crate::testing::ScriptedLauncher;

    /// A line larger than a pipe's buffer, which Claude does not read, blocks only the writer:
    /// handing it over returns at once, as the supervisor's watch loop needs to go on looking for
    /// a cancellation.
    #[gtest]
    fn a_line_claude_does_not_read_never_blocks_the_supervisor() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let turn = TurnDir {
            root: dir.path().to_path_buf(),
        };
        let (_claude_stdin, pipe) = std::io::pipe().or_fail()?;

        let began = Instant::now();
        let mut input = ClaudeInput::start(pipe, &"q".repeat(4 << 20), uuid::Uuid::new_v4(), &turn);
        input.collect_written();
        assert_that!(began.elapsed() < Duration::from_secs(1), eq(true));
        assert_that!(matches!(input.writing, Some(Writing::Question)), eq(true));
        Ok(())
    }

    /// A steer whose write failed, as one does once Claude has gone, is rejected for that when the
    /// input closes, though nothing collected the failure before.
    #[gtest]
    fn a_steer_whose_write_failed_is_rejected_for_it_when_the_input_closes() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let store = RunStore::open(
            dir.path(),
            Arc::new(ScriptedLauncher::default()),
            BTreeMap::new(),
        )
        .or_fail()?;
        let run_id = RunId::generate();
        let turn = store.turn_dir(&run_id, 0);
        std::fs::create_dir_all(&turn.root).or_fail()?;
        let steer = turn.mailbox().submit("Narrow it to auth.").or_fail()?;
        let claim = turn.mailbox().claim_next().or_fail()?;

        // The writer has reported the failure, and nothing has collected it.
        let (report, written) = mpsc::channel();
        report
            .send(Err(std::io::ErrorKind::BrokenPipe.into()))
            .or_fail()?;
        let mut input = ClaudeInput {
            mailbox: turn.mailbox(),
            capture: turn.events(),
            read_to: 0,
            lines: None,
            written,
            writing: Some(Writing::Steer(claim)),
        };

        input.close(&store, &run_id, EXITED);
        assert_that!(
            turn.mailbox().reply(&steer).or_fail()?,
            some(matches_pattern!(SteerReply::Rejected {
                reason: contains_substring("writing it to Claude's input failed"),
            }))
        );
        Ok(())
    }

    /// A steer's line says `next`, and carries the id it was submitted under, hyphenated as
    /// Claude echoes it.
    #[gtest]
    fn a_steer_goes_next_under_its_own_id() -> Result<()> {
        let id = uuid::Uuid::new_v4();
        let line: serde_json::Value =
            serde_json::from_slice(&input_line(id, "narrow it", Some("next"))).or_fail()?;
        assert_that!(line.get("priority"), some(eq(&serde_json::json!("next"))));
        assert_that!(
            line.get("uuid"),
            some(eq(&serde_json::json!(id.hyphenated().to_string())))
        );
        Ok(())
    }
}
