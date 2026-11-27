//! Steering an instruction into a running turn.

use std::time::Duration;

use crate::appserver::mailbox::{Mailbox, SteerId, SteerReply};
use crate::delegate::{Delegate, Isolation};
use crate::launch::{Launched, Liveness};
use crate::stream::TurnInput;
use crate::transcript::Outcome;

use super::error::{NotRunningReason, RunError, SteerError, UnsteerableReason};
use super::id::RunId;
use super::layout::TurnDir;
use super::records::LaunchMode;
use super::status::RunStatus;
use super::{NewestTurn, RunStore};

impl RunStore {
    /// How long a steered message may wait for the turn's driver to pick it up.
    ///
    /// The driver looks every tenth of a second, so this is only ever reached by a driver that has
    /// stopped looking.
    pub(super) const STEER_PICKUP_TIMEOUT: Duration = Duration::from_secs(15);

    /// How long a picked-up message may wait for the delegate CLI to confirm it.
    pub(super) const STEER_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

    /// How often a steer looks for its reply.
    pub(super) const STEER_POLL_INTERVAL: Duration = Duration::from_millis(100);

    /// Add an instruction to a consultation's running turn, for the delegate to see before its
    /// next model call.
    ///
    /// A turn driven over `codex app-server` takes one, and so does a Claude turn reading
    /// stream-json input until it has reported its result.
    /// The message travels through the turn's mailbox: to the Codex turn's driver, which hands it
    /// to the server as `turn/steer` and records it in the capture once the server has accepted
    /// it, or to the Claude turn's supervisor, which writes it to Claude's input, where Claude
    /// echoes it into the capture as it takes it.
    /// Returns once the capture holds the message, so a transcript read after a successful steer
    /// holds it, as a `### user (steer)` section.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, and [`RunError::Steer`]
    /// when the message is empty, no turn is running, the running turn cannot be steered, the
    /// delegate CLI refused the message, the turn ended before the message was picked up, or the
    /// CLI did not confirm it in time.
    pub async fn steer(&self, run_id: &RunId, message: &str) -> Result<RunStatus, RunError> {
        if message.trim().is_empty() {
            return Err(SteerError::Empty.into());
        }
        let SubmittedSteer {
            mailbox,
            steer_id,
            server,
            turn_index,
            launch_mode,
        } = self.submit_steer(run_id, message)?;
        let claude = launch_mode == LaunchMode::ClaudeStreamInput;
        let server_name = if claude { "supervisor" } else { "driver" };
        let submitted = tokio::time::Instant::now();
        let mut claimed_at: Option<tokio::time::Instant> = None;
        loop {
            if let Some(reply) = mailbox
                .reply(&steer_id)
                .map_err(RunError::io("reading the reply to a steered message"))?
            {
                return match reply {
                    SteerReply::Accepted => self.status(run_id),
                    SteerReply::Rejected { reason } => Err(SteerError::Rejected {
                        run_id: run_id.clone(),
                        detail: reason,
                    }
                    .into()),
                    SteerReply::Unconfirmed { reason } => Err(SteerError::Unconfirmed {
                        run_id: run_id.clone(),
                        why: reason,
                    }
                    .into()),
                };
            }
            // A Claude turn's supervisor answers only a message it could not write: whether
            // Claude took one is what the turn's capture says.
            let ended = match (claude, steer_id.uuid()) {
                (true, Some(id)) => match self.claude_progress(run_id, turn_index, id)? {
                    ClaudeProgress::Took => return self.status(run_id),
                    ClaudeProgress::Waiting => false,
                    ClaudeProgress::TurnEnded => true,
                },
                (true, None) | (false, _) => false,
            };
            if claimed_at.is_none() && mailbox.is_claimed(&steer_id) {
                claimed_at = Some(tokio::time::Instant::now());
            }
            let gone = self.child_has_gone(server);
            let waited = Self::STEER_CONFIRM_TIMEOUT.as_secs();
            match claimed_at {
                // Written to Claude, which the turn's frozen capture shows never took it.
                Some(_) if ended => {
                    return Err(SteerError::Unconfirmed {
                        run_id: run_id.clone(),
                        why: "the turn ended before Claude took it".to_owned(),
                    }
                    .into());
                }
                // Claimed and then never answered: the driver went, or the CLI is silent.
                // Claude can still take a message its supervisor wrote before it went.
                Some(at) if (gone && !claude) || at.elapsed() >= Self::STEER_CONFIRM_TIMEOUT => {
                    return Err(SteerError::Unconfirmed {
                        run_id: run_id.clone(),
                        why: if gone && !claude {
                            "the turn's driver left without answering".to_owned()
                        } else if claude {
                            format!(
                                "Claude has not taken it within {waited}s, which it does only \
                                 between its model calls, and it may still arrive"
                            )
                        } else {
                            format!("the delegate did not confirm it within {waited}s")
                        },
                    }
                    .into());
                }
                None if gone || ended || submitted.elapsed() >= Self::STEER_PICKUP_TIMEOUT => {
                    // Withdrawn by creating the decision file the driver claims with, so a
                    // message that was not taken back is one the driver has and will answer.
                    if mailbox
                        .withdraw(&steer_id)
                        .map_err(RunError::io("withdrawing a steered message"))?
                    {
                        return Err(SteerError::Withdrawn {
                            run_id: run_id.clone(),
                            why: if gone || ended {
                                format!("the turn ended before its {server_name} picked it up")
                            } else {
                                format!(
                                    "the turn's {server_name} did not pick it up within {}s",
                                    Self::STEER_PICKUP_TIMEOUT.as_secs()
                                )
                            },
                        }
                        .into());
                    }
                    claimed_at = Some(tokio::time::Instant::now());
                }
                Some(_) | None => {}
            }
            tokio::time::sleep(Self::STEER_POLL_INTERVAL).await;
        }
    }

    /// Whether Claude has taken the steer `id` into turn `turn_index`, as a fold of the turn
    /// under the run's lock says, which reads no further than a settled turn's frozen capture.
    ///
    /// What the transcript shows and what `steer` reports are then the same fact.
    fn claude_progress(
        &self,
        run_id: &RunId,
        turn_index: u32,
        id: uuid::Uuid,
    ) -> Result<ClaudeProgress, RunError> {
        let meta = self.meta(run_id)?;
        let run = self.lock(run_id)?;
        let Some(files) = self
            .turn_files(run_id)?
            .into_iter()
            .find(|files| files.index == turn_index)
        else {
            return Ok(ClaudeProgress::TurnEnded);
        };
        let folded = run.fold_turn(&meta, &files)?;
        Ok(if folded.steers_taken.contains(&id) {
            ClaudeProgress::Took
        } else if folded.turn.outcome.is_terminal() {
            ClaudeProgress::TurnEnded
        } else {
            ClaudeProgress::Waiting
        })
    }

    /// Put `message` in the mailbox of the consultation's running turn, when that turn takes a
    /// steer now; otherwise say why not.
    ///
    /// Submitted under the run's lock, so the turn cannot end and its run be removed between the
    /// check and the write, which would leave a mailbox behind in a run that is gone.
    /// A Claude turn's supervisor closes its input under the same lock, so a message submitted
    /// here is one it will write or reject.
    fn submit_steer(&self, run_id: &RunId, message: &str) -> Result<SubmittedSteer, RunError> {
        let meta = self.meta(run_id)?;
        let not_running = |state| SteerError::NotRunning {
            run_id: run_id.clone(),
            state,
        };
        let run = self.lock(run_id)?;
        let Some(files) = self.turn_files(run_id)?.into_iter().last() else {
            return Err(not_running(NotRunningReason::NotLaunched).into());
        };
        let folded = run.fold_turn(&meta, &files)?;
        let newest = NewestTurn::of(&files, &folded);
        let server = match self.steer_server(&folded.turn.outcome, &newest, &files.dir) {
            Ok(server) => server,
            Err(Refusal::NotRunning(state)) => return Err(not_running(state).into()),
            Err(Refusal::ReadsQuestionOnce) => {
                // The running turn's own isolation, which a follow-up may have changed.
                let current = self.current_delegate(&meta, Some(files.index))?;
                return Err(SteerError::NotSteerable {
                    run_id: run_id.clone(),
                    reason: why_not_steerable(&current),
                }
                .into());
            }
            Err(Refusal::ClaudeInputLost) => {
                return Err(SteerError::NotSteerable {
                    run_id: run_id.clone(),
                    reason: UnsteerableReason::ClaudeInputLost,
                }
                .into());
            }
            Err(Refusal::Starting) => {
                return Err(SteerError::Starting {
                    run_id: run_id.clone(),
                }
                .into());
            }
        };
        let mailbox = files.dir.mailbox();
        let steer_id = mailbox
            .submit(message)
            .map_err(RunError::io("submitting the steered message"))?;
        Ok(SubmittedSteer {
            mailbox,
            steer_id,
            server,
            turn_index: files.index,
            launch_mode: newest.launch_mode,
        })
    }

    /// The process that serves the newest turn's mailbox, when the turn takes a steer now, or why
    /// it takes none, from one fold of it.
    ///
    /// The one answer both `status` and `steer` give, so a turn advertised as steerable is one
    /// `steer` will not refuse.
    /// The turn must still run, with nobody asking it to stop, and take input as its capture says:
    /// a Codex turn's driver writes the turn's start once it knows the turn and is about to serve
    /// its mailbox, which it then does, and until then a steer would only wait there, for as long
    /// as opening a long thread or starting the turn takes; a Claude turn takes one from its
    /// `system`/`init` until its first `result`, while its supervisor, which serves its mailbox
    /// and holds its input, and Claude itself are both positively alive.
    pub(super) fn steer_server(
        &self,
        outcome: &Outcome,
        newest: &NewestTurn,
        dir: &TurnDir,
    ) -> Result<Launched, Refusal> {
        if outcome.is_terminal() {
            return Err(Refusal::NotRunning(NotRunningReason::Ended(
                outcome.state(),
            )));
        }
        if newest.launch_mode == LaunchMode::DirectCli {
            return Err(Refusal::ReadsQuestionOnce);
        }
        if dir.cancelling().exists() {
            return Err(Refusal::NotRunning(NotRunningReason::Cancelling));
        }
        match newest.input {
            TurnInput::Pending => return Err(Refusal::Starting),
            TurnInput::Closed => return Err(Refusal::NotRunning(NotRunningReason::Finishing)),
            TurnInput::Open => {}
        }
        let server = match newest.launch_mode {
            LaunchMode::DirectCli | LaunchMode::AppServer => newest.delegate,
            LaunchMode::ClaudeStreamInput if !self.is_alive(newest.supervisor) => {
                return Err(Refusal::ClaudeInputLost);
            }
            LaunchMode::ClaudeStreamInput if !self.is_alive(newest.delegate) => {
                return Err(Refusal::NotRunning(NotRunningReason::Finishing));
            }
            LaunchMode::ClaudeStreamInput => newest.supervisor,
        };
        server.ok_or(Refusal::NotRunning(NotRunningReason::NotLaunched))
    }

    /// Whether a recorded process is positively alive, rather than gone or unknown.
    fn is_alive(&self, launched: Option<Launched>) -> bool {
        launched.is_some_and(|launched| self.launcher.liveness(launched) == Liveness::Alive)
    }
}

/// A steered message in its turn's mailbox, and the process that is to answer it.
struct SubmittedSteer {
    mailbox: Mailbox,
    steer_id: SteerId,
    /// The process that serves the mailbox: a Codex turn's driver, or a Claude turn's
    /// supervisor.
    server: Launched,
    turn_index: u32,
    launch_mode: LaunchMode,
}

/// Why a running turn takes no steer now.
pub(super) enum Refusal {
    NotRunning(NotRunningReason),
    /// Its delegate was launched to read its question once.
    ReadsQuestionOnce,
    /// It does not take input yet.
    Starting,
    /// Its supervisor, which holds Claude's input, has gone or cannot be checked on.
    ClaudeInputLost,
}

/// How far a steer into a Claude turn has got, as the turn's capture says.
enum ClaudeProgress {
    /// Claude echoed it where it took it.
    Took,
    /// Not yet, and the turn runs on.
    Waiting,
    /// Never: the turn ended without it.
    TurnEnded,
}

/// Why a turn of this delegate that was launched to read its question once offers no way in.
pub(super) fn why_not_steerable(delegate: &Delegate) -> UnsteerableReason {
    match delegate {
        Delegate::Claude { .. } => UnsteerableReason::ClaudePrint,
        Delegate::Codex {
            isolation: Some(Isolation::Inherit),
            ..
        } => UnsteerableReason::CodexExec,
        Delegate::Codex { .. } => UnsteerableReason::CodexIsolated,
    }
}
