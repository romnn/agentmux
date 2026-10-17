//! Stopping a running consultation.

use std::fs::File;
use std::time::Duration;

use crate::launch::{Launched, Liveness};

use super::RunStore;
use super::error::RunError;
use super::id::RunId;
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
    pub(super) const INTERRUPT_GRACE: Duration =
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
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, and [`RunError::Io`]
    /// when the cancellation cannot be recorded.
    pub async fn cancel(&self, run_id: &RunId) -> Result<RunStatus, RunError> {
        let meta = self.meta(run_id)?;
        // Marked under the lock, which is released before anything slow: stopping can take
        // seconds, and a `status` in the meantime should not wait on it.
        let marked = {
            let run = self.lock(run_id)?;
            let newest = self.turn_files(run_id)?.into_iter().last();
            match newest {
                None => None,
                Some(newest) => {
                    let cancelling = !run.fold_turn(&meta, &newest)?.turn.outcome.is_terminal();
                    if cancelling {
                        File::create(newest.dir.cancelling()).map_err(|source| RunError::Io {
                            context: format!("marking turn {} as cancelling", newest.index),
                            source,
                        })?;
                    }
                    Some((newest, cancelling))
                }
            }
        };
        let Some((newest, cancelling)) = marked else {
            return self.status(run_id);
        };
        if let Some(record) = newest.launch.as_ref() {
            // A driven turn sees the marker and interrupts itself over the protocol, which lets
            // the server close the turn and save it; it is given the chance before any signal.
            // A driver whose turn has already ended may still be stopping what its server left
            // running, and is given the same chance to finish.
            let driven = read_json::<InvocationRecord>(&newest.dir.invocation())?
                .is_some_and(|invocation| invocation.mode == LaunchMode::AppServer);
            if driven {
                self.wait_for_departure(record.launched, Self::INTERRUPT_GRACE)
                    .await;
            }
            self.stop(record.launched).await;
        }
        if cancelling {
            // Frozen at the complete records that exist now, which is exactly what a later fold
            // reads back, and under the lock, so no fold that publishes can read past it first.
            let _run = self.lock(run_id)?;
            let (_, events_len) = read_complete_records(&newest.dir.events(), None)?;
            write_json_once(&newest.dir.exit(), &ExitRecord::Cancelled { events_len })?;
        }
        self.status(run_id)
    }

    /// Ask a child to stop, then make it, and wait until it has gone.
    ///
    /// A child that has already gone costs one look.
    /// A child that cannot be observed — on a platform that answers [`Liveness::Unknown`] — is
    /// not waited for, because nothing would ever end the wait.
    pub(super) async fn stop(&self, launched: Launched) {
        self.launcher.terminate(launched);
        if self
            .wait_for_departure(launched, Self::TERMINATE_GRACE)
            .await
        {
            return;
        }
        self.launcher.kill(launched);
        self.wait_for_departure(launched, Self::KILL_GRACE).await;
    }

    /// Whether the child left within `grace`.
    pub(super) async fn wait_for_departure(&self, launched: Launched, grace: Duration) -> bool {
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
