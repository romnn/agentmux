//! How long a consultation is kept, and taking it away.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::RunStore;
use super::error::RunError;
use super::id::RunId;
use super::records::{ExitRecord, Meta, TurnFiles};
use super::settle;

/// How long a finished consultation is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// Swept once the machine's retention window has passed since its delegate last wrote
    /// anything: [`Config::sweep_after`](crate::config::Config::sweep_after), a week unless the
    /// machine file sets `retention`.
    #[default]
    Ttl,
    /// Never swept, so a follow-up can arrive at any time.
    ///
    /// There is no long-lived child process behind this.
    /// Both CLIs resume a finished session from their own on-disk state, so a kept consultation is
    /// a retained *record*, not a parked process: nothing leaks if agentmux is restarted, killed
    /// or upgraded, and a follow-up works tomorrow as well as in five minutes.
    /// Remove these with `agentmux prune`.
    UntilReleased,
}

impl RunStore {
    /// Whether any recorded child of a run is still there.
    ///
    /// A turn is terminal the moment its stream says so, and the child behind it can outlive
    /// that by a little — Codex writes its closing message file afterwards — or, when it was
    /// told to stop and would not, by more.
    /// Deleting a run out from under a live child would leave it writing into unlinked files,
    /// billing, and unreachable.
    ///
    /// A cancelled turn's child is asked about too: its record says the capture is settled, not
    /// that the child left, and one that outlived its kill is still running.
    /// A child whose identity cannot be checked counts as alive, because deleting under it is
    /// the mistake that cannot be undone.
    ///
    /// A turn's supervisor is asked about too, because a settled marker does not mean it has
    /// finished publishing; and a supervisor that left while spawning its delegate, recording
    /// none, may have left one running that nothing can name.
    pub(super) fn a_child_is_alive(&self, run_id: &RunId) -> Result<bool, RunError> {
        Ok(self
            .turn_files(run_id)?
            .iter()
            .any(|turn| self.turn_may_be_alive(turn)))
    }

    /// Whether one turn's delegate or supervisor may still be running, by the rule
    /// [`RunStore::a_child_is_alive`] states.
    pub(super) fn turn_may_be_alive(&self, turn: &TurnFiles) -> bool {
        let delegate_may_run = match &turn.exit {
            None | Some(ExitRecord::Cancelled { .. }) => turn
                .launch
                .as_ref()
                .is_some_and(|record| !self.child_has_gone(record.launched)),
            Some(ExitRecord::Exited { .. } | ExitRecord::NotStarted { .. }) => false,
        };
        let unrecorded_start = turn.start_is_uncertain();
        let supervisor_runs = turn
            .supervisor
            .as_ref()
            .is_some_and(|supervisor| !self.child_has_gone(supervisor.launched));
        delegate_may_run || unrecorded_start || supervisor_runs
    }

    /// Delete consultations whose delegate last wrote anything more than `after` ago.
    ///
    /// `after` is [`Config::sweep_after`](crate::config::Config::sweep_after) everywhere but in
    /// tests.
    ///
    /// Runs once at server start.
    /// There is no timer and no daemon.
    ///
    /// A run is removed only under its lock, so a follow-up claimed between the check and the
    /// deletion cannot lose its capture files, and a run with a child still alive is never
    /// swept, however old: a long review that outlived its window is exactly the one worth
    /// keeping.
    /// A run that never got a turn — a start that failed after the directory was made — is
    /// swept like a finished one.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] when the run directory cannot be listed.
    pub fn sweep(&self, after: Duration) -> Result<usize, RunError> {
        // The clock runs from the last thing the delegate wrote, not from the start: a
        // consultation that ran for longer than the window is not stale the moment it ends.
        // File times alone decide it, so a young run costs a few stats and no fold.
        let expired = |run_id: &RunId, meta: &Meta| {
            if meta.retention == Retention::UntilReleased {
                return false;
            }
            let last = self
                .turn_dirs(run_id)
                .filter_map(|(_, dir)| settle::last_write(&dir))
                .max()
                .map_or(meta.created_at, DateTime::<Utc>::from)
                .max(meta.created_at);
            Utc::now()
                .signed_duration_since(last)
                .to_std()
                .is_ok_and(|age| age >= after)
        };
        let mut removed = 0;
        for run_id in self.run_ids()? {
            if !self.meta(&run_id).is_ok_and(|meta| expired(&run_id, &meta)) {
                continue;
            }
            let Ok(run) = self.lock(&run_id) else {
                continue;
            };
            // Asked again under the lock: a follow-up or a `keep` that landed between the first
            // look and the lock makes the run young or kept again.
            let Ok(meta) = self.meta(&run_id) else {
                continue;
            };
            if !expired(&run_id, &meta) {
                continue;
            }
            let finished = match run.fold_newest_turn(&meta) {
                Ok(folded) => folded.is_none_or(|folded| folded.turn.outcome.is_terminal()),
                Err(_) => continue,
            };
            if !finished || self.a_child_is_alive(&run_id).unwrap_or(true) {
                continue;
            }
            if std::fs::remove_dir_all(self.run_dir(&run_id)).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Delete one consultation outright, whatever its retention.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, and
    /// [`RunError::StillRunning`] while its delegate is: deleting the record of a running child
    /// would leave it running, billing, and unreachable by anything.
    pub fn remove(&self, run_id: &RunId) -> Result<(), RunError> {
        let dir = self.run_dir(run_id);
        if !dir.is_dir() {
            return Err(RunError::NotFound(run_id.clone()));
        }
        let run = self.lock(run_id)?;
        let meta = self.meta(run_id)?;
        if run
            .fold_newest_turn(&meta)?
            .is_some_and(|folded| !folded.turn.outcome.is_terminal())
            || self.a_child_is_alive(run_id)?
        {
            return Err(RunError::StillRunning {
                run_id: run_id.clone(),
            });
        }
        std::fs::remove_dir_all(&dir).map_err(RunError::io(format!("removing {run_id}")))
    }
}
