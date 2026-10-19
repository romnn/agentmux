//! Steering an instruction into a running turn.

use std::time::Duration;

use crate::appserver::mailbox::{Letter, Mailbox, Reply};
use crate::delegate::{Delegate, Isolation};
use crate::launch::{Launched, Liveness};
use crate::transcript::Outcome;

use super::RunStore;
use super::error::{Idle, RunError, SteerRefused, Unsteerable};
use super::id::RunId;
use super::layout::TurnDir;
use super::records::LaunchMode;
use super::status::RunStatus;

impl RunStore {
    /// How long a steered message may wait for the turn's driver to pick it up.
    ///
    /// The driver looks every tenth of a second, so this is only ever reached by a driver that has
    /// stopped looking.
    pub(super) const STEER_PICKUP: Duration = Duration::from_secs(15);

    /// How long a picked-up message may wait for the delegate CLI to confirm it.
    pub(super) const STEER_CONFIRM: Duration = Duration::from_secs(30);

    /// How often a steer looks for its reply.
    pub(super) const STEER_POLL: Duration = Duration::from_millis(100);

    /// Add an instruction to a consultation's running turn, for the delegate to see before its
    /// next model call.
    ///
    /// Only a turn driven over `codex app-server` takes one; the message travels through the
    /// turn's mailbox to its driver, which hands it to the server as `turn/steer` and records it
    /// in the capture once the server has accepted it.
    /// Returns once the delegate CLI has accepted the message, so a transcript read after a
    /// successful steer holds it, as a `### user (steer)` section.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, and [`RunError::Steer`]
    /// when the message is empty, no turn is running, the running turn cannot be steered, the
    /// delegate CLI refused the message, the turn ended before the message was picked up, or the
    /// CLI did not confirm it in time.
    pub async fn steer(&self, run_id: &RunId, message: &str) -> Result<RunStatus, RunError> {
        if message.trim().is_empty() {
            return Err(SteerRefused::Empty.into());
        }
        let Submitted {
            mailbox,
            letter,
            launched,
        } = self.submit_steer(run_id, message)?;
        let submitted = tokio::time::Instant::now();
        let mut claimed_at: Option<tokio::time::Instant> = None;
        loop {
            if let Some(reply) = mailbox
                .reply(&letter)
                .map_err(RunError::io("reading the reply to a steered message"))?
            {
                return match reply {
                    Reply::Accepted => self.status(run_id),
                    Reply::Rejected { reason } => Err(SteerRefused::Rejected {
                        run_id: run_id.clone(),
                        detail: reason,
                    }
                    .into()),
                    Reply::Unconfirmed { reason } => Err(SteerRefused::Unconfirmed {
                        run_id: run_id.clone(),
                        why: reason,
                    }
                    .into()),
                };
            }
            if claimed_at.is_none() && mailbox.is_claimed(&letter) {
                claimed_at = Some(tokio::time::Instant::now());
            }
            let gone = self.launcher.reap(launched).is_some()
                || self.launcher.liveness(launched) == Liveness::Gone;
            match claimed_at {
                // Claimed and then never answered: the driver went, or the CLI is silent.
                Some(at) if gone || at.elapsed() >= Self::STEER_CONFIRM => {
                    return Err(SteerRefused::Unconfirmed {
                        run_id: run_id.clone(),
                        why: if gone {
                            "the turn's driver left without answering".to_owned()
                        } else {
                            format!(
                                "the delegate did not confirm it within {}s",
                                Self::STEER_CONFIRM.as_secs()
                            )
                        },
                    }
                    .into());
                }
                None if gone || submitted.elapsed() >= Self::STEER_PICKUP => {
                    // Withdrawn by creating the decision file the driver claims with, so a
                    // message that was not taken back is one the driver has and will answer.
                    if mailbox
                        .withdraw(&letter)
                        .map_err(RunError::io("withdrawing a steered message"))?
                    {
                        return Err(SteerRefused::Withdrawn {
                            run_id: run_id.clone(),
                            why: if gone {
                                "the turn ended before its driver picked it up".to_owned()
                            } else {
                                format!(
                                    "the turn's driver did not pick it up within {}s",
                                    Self::STEER_PICKUP.as_secs()
                                )
                            },
                        }
                        .into());
                    }
                    claimed_at = Some(tokio::time::Instant::now());
                }
                Some(_) | None => {}
            }
            tokio::time::sleep(Self::STEER_POLL).await;
        }
    }

    /// Put `message` in the mailbox of the consultation's running turn, when that turn takes a
    /// steer now; otherwise say why not.
    ///
    /// Submitted under the run's lock, so the turn cannot end and its run be removed between the
    /// check and the write, which would leave a mailbox behind in a run that is gone.
    fn submit_steer(&self, run_id: &RunId, message: &str) -> Result<Submitted, RunError> {
        let meta = self.meta(run_id)?;
        let not_running = |state| SteerRefused::NotRunning {
            run_id: run_id.clone(),
            state,
        };
        let run = self.lock(run_id)?;
        let Some(newest) = self.turn_files(run_id)?.into_iter().last() else {
            return Err(not_running(Idle::NotLaunched).into());
        };
        let folded = run.fold_turn(&meta, &newest)?;
        let outcome = folded.turn.outcome;
        if outcome.is_terminal() {
            return Err(not_running(Idle::Ended(outcome.state())).into());
        }
        if folded.mode != LaunchMode::AppServer {
            return Err(SteerRefused::NotSteerable {
                run_id: run_id.clone(),
                reason: why_not_steerable(&meta.delegate),
            }
            .into());
        }
        if newest.dir.cancelling().exists() {
            return Err(not_running(Idle::Cancelling).into());
        }
        if !Self::accepts_steers(&outcome, folded.mode, folded.turn_started, &newest.dir) {
            return Err(SteerRefused::Starting {
                run_id: run_id.clone(),
            }
            .into());
        }
        let Some(launched) = newest.launch.as_ref().map(|record| record.launched) else {
            return Err(not_running(Idle::NotLaunched).into());
        };
        let mailbox = newest.dir.mailbox();
        let letter = mailbox
            .submit(message)
            .map_err(RunError::io("submitting the steered message"))?;
        Ok(Submitted {
            mailbox,
            letter,
            launched,
        })
    }

    /// Whether the newest turn takes a steer: it runs, over the app-server, its driver is
    /// following the turn, and nobody has asked it to stop.
    ///
    /// The one answer both `status` and `steer` give, so a turn advertised as steerable is one
    /// `steer` will not refuse.
    /// `started` is whether the turn's capture says the turn began, which the driver writes once
    /// it knows the turn and is about to serve its mailbox: until then a steer would only wait
    /// there, for as long as opening a long thread or starting the turn takes.
    pub(super) fn accepts_steers(
        outcome: &Outcome,
        mode: LaunchMode,
        started: bool,
        turn: &TurnDir,
    ) -> bool {
        !outcome.is_terminal()
            && mode == LaunchMode::AppServer
            && started
            && !turn.cancelling().exists()
    }
}

/// A steered message in its turn's mailbox, and the driver that is to answer it.
struct Submitted {
    mailbox: Mailbox,
    letter: Letter,
    launched: Launched,
}

/// Why a turn of this delegate that was not driven over the app-server offers no way in.
pub(super) fn why_not_steerable(delegate: &Delegate) -> Unsteerable {
    match delegate {
        Delegate::Claude { .. } => Unsteerable::ClaudePrint,
        Delegate::Codex {
            isolation: Some(Isolation::Inherit),
            ..
        } => Unsteerable::CodexExec,
        Delegate::Codex { .. } => Unsteerable::CodexIsolated,
    }
}
