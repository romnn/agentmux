//! Stopping a running consultation.

use std::fs::File;
use std::time::Duration;

use crate::launch::{Launched, Liveness};
use crate::transcript::{FailureKind, Outcome};

use super::RunStore;
use super::error::RunError;
use super::id::RunId;
use super::layout::TurnDir;
use super::records::{
    ExitRecord, InvocationRecord, LaunchMode, read_complete_records, read_json, write_json_once,
};
use super::status::RunStatus;

impl RunStore {
    /// How long a cancelled child is given to leave after being asked.
    ///
    /// Both CLIs exit on `SIGTERM` well inside this; what needs the time is a tool subprocess
    /// they are waiting on.
    pub(super) const TERMINATE_GRACE: Duration = Duration::from_secs(3);

    /// How long a killed child is given to actually disappear.
    pub(super) const KILL_GRACE: Duration = Duration::from_secs(2);

    /// How often a cancellation checks whether the child has gone.
    pub(super) const STOP_POLL: Duration = Duration::from_millis(100);

    /// How long a cancelled turn driven over the app-server is given to interrupt itself and
    /// leave, before it is signalled like any other child.
    ///
    /// The driver takes at most [`crate::appserver::STOP_BUDGET`] to interrupt the turn, let the
    /// server exit and stop what it left running, so this outlasts all of it.
    pub(super) const DRIVER_EXIT_GRACE: Duration =
        crate::appserver::STOP_BUDGET.saturating_add(Duration::from_secs(5));

    /// Stop a running consultation.
    ///
    /// Everything collected so far is kept.
    /// The child is asked to stop, then made to, and the cancellation is recorded once it has
    /// gone, with the capture as it stood at that moment: the footer this publishes is the last
    /// thing the transcript will say, and nothing the child could still write is part of it.
    /// Whatever the child wrote before it went is folded in, so a delegate that finished its
    /// turn in the moment before the signal landed is reported as having finished.
    ///
    /// The intent is marked under the lock before anything is signalled, so a concurrent reader
    /// that sees the child go first records the same cancellation rather than a death.
    /// A child that survives both signals is still stopped by the next `cancel`, which stops
    /// whatever is still running whether or not the turn is already recorded.
    ///
    /// A consultation whose newest turn failed because the model was overloaded is not running,
    /// but agentmux may be about to resume it; cancelling it stops that resume, which then never
    /// starts, and the turn stays failed.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation,
    /// [`RunError::UncertainStart`] when the newest turn's supervisor died while starting a
    /// delegate it never recorded, and [`RunError::Io`] when the cancellation cannot be recorded.
    pub async fn cancel(&self, run_id: &RunId) -> Result<RunStatus, RunError> {
        let meta = self.meta(run_id)?;
        // Marked under the lock, which is released before anything slow: stopping can take
        // seconds, and a `status` in the meantime should not wait on it.
        let marked = {
            let run = self.lock(run_id)?;
            let newest = self.turn_files(run_id)?.into_iter().last();
            match newest {
                None => None,
                // A turn whose supervisor died while spawning may have a delegate nothing names,
                // so nothing could be stopped, and a cancellation recorded over it would read as
                // the end of a turn that may still be running and billing.
                Some(newest) if newest.start_is_uncertain() => {
                    return Err(RunError::UncertainStart {
                        run_id: run_id.clone(),
                        turn: newest.index.saturating_add(1),
                        dir: newest.dir.root.clone(),
                    });
                }
                Some(newest) => {
                    let outcome = run.fold_turn(&meta, &newest)?.turn.outcome;
                    let cancelling = !outcome.is_terminal();
                    if cancelling {
                        File::create(newest.dir.cancelling()).map_err(|source| RunError::Io {
                            context: format!("marking turn {} as cancelling", newest.index),
                            source,
                        })?;
                    } else if matches!(
                        outcome,
                        Outcome::Failed {
                            kind: FailureKind::Overloaded,
                            ..
                        }
                    ) {
                        // Its supervisor may be waiting to resume it, and claims the resume under
                        // this lock only while this marker is absent; marked whether or not the
                        // resume is decided yet, so one decided after this still never starts.
                        File::create(newest.dir.auto_resume_cancelled()).map_err(|source| {
                            RunError::Io {
                                context: format!(
                                    "stopping the automatic resume after turn {}",
                                    newest.index
                                ),
                                source,
                            }
                        })?;
                    }
                    Some((newest, cancelling))
                }
            }
        };
        let Some((newest, cancelling)) = marked else {
            return self.status(run_id);
        };
        // A supervised turn whose delegate is not recorded yet has nothing to stop: its
        // supervisor starts nothing once it sees the marker, under the same lock.
        // A supervisor that is running stops its delegate too, the same way, and either stop is
        // checked against the delegate's identity, so the second finds nothing to do.
        if let Some(record) = newest.launch.as_ref() {
            self.stop_delegate(&newest.dir, record.launched).await?;
        }
        if cancelling {
            self.record_cancellation(run_id, &newest.dir)?;
        }
        self.status(run_id)
    }

    /// Stop a turn's delegate, giving a driven turn the chance to interrupt itself first.
    ///
    /// A driven turn sees the `cancelling` marker and interrupts itself over the protocol, which
    /// lets the server close the turn and save it; it is given the chance before any signal.
    /// A driver whose turn has already ended may still be stopping what its server left running,
    /// and is given the same chance to finish.
    pub(super) async fn stop_delegate(
        &self,
        dir: &TurnDir,
        launched: Launched,
    ) -> Result<(), RunError> {
        let uses_app_server = read_json::<InvocationRecord>(&dir.invocation())?
            .is_some_and(|invocation| invocation.launch_mode == LaunchMode::AppServer);
        if uses_app_server {
            self.wait_for_delegate_exit(launched, Self::DRIVER_EXIT_GRACE)
                .await;
        }
        self.stop(launched).await;
        Ok(())
    }

    /// Record a turn's cancellation and settle it, whether or not its delegate left.
    ///
    /// Frozen at the complete records that exist now, which is exactly what a later fold reads
    /// back, and under the lock, so no fold that publishes can read past it first.
    /// A record another caller wrote first is adopted.
    pub(super) fn record_cancellation(
        &self,
        run_id: &RunId,
        dir: &TurnDir,
    ) -> Result<(), RunError> {
        let meta = self.meta(run_id)?;
        let run = self.lock(run_id)?;
        let (_, events_len) = read_complete_records(&dir.events(), None)?;
        write_json_once(&dir.exit(), &ExitRecord::Cancelled { events_len })?;
        run.fold_and_publish(&meta)?;
        Ok(())
    }

    /// Ask a child to stop, then make it, and wait until it has gone.
    ///
    /// A child that has already gone costs one look.
    /// A child that cannot be observed — on a platform that answers [`Liveness::Unknown`] — is
    /// not waited for, because nothing would ever end the wait.
    pub(super) async fn stop(&self, launched: Launched) {
        self.launcher.terminate(launched);
        if self
            .wait_for_delegate_exit(launched, Self::TERMINATE_GRACE)
            .await
        {
            return;
        }
        self.launcher.kill(launched);
        self.wait_for_delegate_exit(launched, Self::KILL_GRACE)
            .await;
    }

    /// Whether the child left within `grace`.
    pub(super) async fn wait_for_delegate_exit(&self, launched: Launched, grace: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if self.launcher.reap(launched).is_some()
                || self.launcher.liveness(launched) != Liveness::Alive
            {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Self::STOP_POLL).await;
        }
    }
}
