//! Resuming a consultation automatically after its model was overloaded.
//!
//! Changes when what agentmux does about a transient vendor failure changes.
//!
//! A long turn can fail because the vendor's model is at capacity for the moment, hours into the
//! work.
//! The session is intact, and a follow-up saying "continue" carries on from where it stopped, but
//! a caller that looks in once a day would lose a night to it.
//! So the turn's supervisor resumes the consultation itself:
//!
//! 1. As the newest turn's settlement is committed, whoever commits it decides from the turn's
//!    own records whether agentmux resumes it, and writes `auto-resume.json` before
//!    `settled.json`, so every reader that finds the settlement finds the decision too.
//! 2. The turn's supervisor, which is still there because it settles the turn, waits until the
//!    resume is due, looking for a `cancel` and for a follow-up that took the next turn.
//! 3. Under the run's lock it checks that the turn is still the newest, the consultation can
//!    still be continued and nobody stopped it, and launches the next turn with
//!    [`CONTINUE`], supervised and driven as the turn it continues was.
//!
//! The resumed turn has a supervisor of its own, and a resumed turn that fails overloaded is
//! resumed the same way, waiting longer each time, until a turn ends otherwise or the
//! [`AutoResumePolicy`] runs out of attempts.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::delegate::Isolation;
use crate::transcript::{FailureKind, Outcome, ResumeAttempt};

use super::error::RunError;
use super::id::RunId;
use super::lock::LockedRun;
use super::records::{
    InvocationRecord, SupervisorPlan, SupervisorRecord, read_json, write_json_once,
};
use super::spawn::{Inherited, NextTurn, Resolution};
use super::{RunState, RunStore, is_this_process};

/// The question an automatic resume asks: the follow-up a caller would send by hand.
pub const CONTINUE: &str = "continue";

/// How often a supervisor waiting to resume looks for a `cancel` or a follow-up.
const RESUME_POLL: Duration = Duration::from_secs(1);

/// How agentmux resumes a consultation whose turn failed because the model was overloaded.
///
/// The delay before each automatic resume doubles along an unbroken run of turns that failed
/// overloaded, from [`AutoResumePolicy::first_delay`] up to
/// [`AutoResumePolicy::longest_delay`], and after [`AutoResumePolicy::attempts`] automatic
/// resumes that all failed that way agentmux stops.
/// A turn that ends any other way ends the run, and the next overload starts from the first
/// delay again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoResumePolicy {
    /// How long agentmux waits before the first automatic resume of a run.
    pub first_delay: Duration,
    /// The longest it waits before any one automatic resume.
    pub longest_delay: Duration,
    /// How many automatic resumes in a row may fail overloaded before agentmux stops resuming.
    pub attempts: u32,
}

impl AutoResumePolicy {
    /// The policy every store starts with, which `docs/adr/0027` explains: thirty seconds,
    /// doubling up to fifteen minutes, twelve attempts.
    pub const DEFAULT: Self = Self {
        first_delay: Duration::from_secs(30),
        longest_delay: Duration::from_mins(15),
        attempts: 12,
    };

    /// How long to wait before automatic resume `attempt`, counted from 1.
    #[must_use]
    pub fn delay_before(&self, attempt: u32) -> Duration {
        let factor = 2_u32
            .checked_pow(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.first_delay
            .checked_mul(factor)
            .unwrap_or(self.longest_delay)
            .min(self.longest_delay)
    }
}

impl Default for AutoResumePolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a turn's supervisor needs to resume the consultation the way the turn ran, carried in
/// its plan because the supervisor's own environment is the delegate's, not the host's.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AutoResumePlan {
    pub(super) policy: AutoResumePolicy,
    /// The agentmux executable that supervises the resumed turn, as it supervised this one.
    pub(super) supervisor: PathBuf,
    /// The agentmux executable that drives a Codex turn over `codex app-server`, when the store
    /// that launched this turn drives them.
    pub(super) codex_driver: Option<PathBuf>,
}

/// `auto-resume.json`: what was decided as a turn that failed overloaded settled.
///
/// Written once, before the turn's `settled.json`, and never rewritten; what happened since is
/// read from the files around it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum AutoResumeRecord {
    /// The turn's supervisor resumes the consultation at `due_at`.
    Scheduled {
        attempt: u32,
        attempts: u32,
        due_at: DateTime<Utc>,
    },
    /// The turn was the last of `attempts` automatic resumes that all failed overloaded.
    Exhausted { attempts: u32 },
}

/// What agentmux does, or did, about a turn that failed because the model was overloaded.
///
/// Every turn number is one-based, as the transcript's headings count turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AutoResume {
    /// agentmux sends [`CONTINUE`] as turn `turn` at `due_at`, unless the consultation is
    /// cancelled or followed up first.
    Scheduled {
        /// The turn the resume starts.
        turn: u32,
        /// Which automatic resume in a row it is.
        attempt: u32,
        /// How many agentmux makes in a row before it stops.
        attempts: u32,
        /// When the turn's supervisor resumes the consultation.
        due_at: DateTime<Utc>,
    },
    /// agentmux sent [`CONTINUE`] as turn `turn`.
    Resumed {
        /// The turn the resume started.
        turn: u32,
        /// Which automatic resume in a row it was.
        attempt: u32,
        /// How many agentmux makes in a row before it stops.
        attempts: u32,
    },
    /// A `cancel` stopped the resume before it started.
    Cancelled {
        /// Which automatic resume in a row it would have been.
        attempt: u32,
        /// How many agentmux makes in a row before it stops.
        attempts: u32,
    },
    /// The resume was due, but the supervisor that was to send it has gone without sending it.
    Abandoned {
        /// When it was due.
        due_at: DateTime<Utc>,
        /// What that supervisor printed, which says why when anything does.
        supervisor_log: PathBuf,
    },
    /// The turn was the last of `attempts` automatic resumes that all failed overloaded, so
    /// agentmux stopped resuming.
    Exhausted {
        /// How many it made.
        attempts: u32,
    },
}

/// How a supervisor's wait to resume ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Resumption {
    /// Nothing was scheduled.
    NotScheduled,
    /// A `cancel` stopped it.
    Cancelled,
    /// A follow-up took the next turn first.
    Superseded,
    /// The next turn is launched.
    Resumed,
}

impl RunStore {
    /// What agentmux does, or did, about turn `turn`, counted from 1, failing because the model
    /// was overloaded, or `None` when it does nothing.
    ///
    /// Read from the decision recorded as the turn settled, and from what happened around it
    /// since: the next turn, a `cancel`, and whether the supervisor that was to resume is still
    /// there.
    /// A follow-up a caller started in the resume's place leaves nothing to say here.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] or [`RunError::CorruptRecord`] when a record there cannot be
    /// read.
    pub fn auto_resume(&self, run_id: &RunId, turn: u32) -> Result<Option<AutoResume>, RunError> {
        let Some(turn_index) = turn.checked_sub(1) else {
            return Ok(None);
        };
        self.auto_resume_at_index(run_id, turn_index)
    }

    /// [`RunStore::auto_resume`] of the turn with zero-based `turn_index`.
    pub(super) fn auto_resume_at_index(
        &self,
        run_id: &RunId,
        turn_index: u32,
    ) -> Result<Option<AutoResume>, RunError> {
        let dir = self.turn_dir(run_id, turn_index);
        let Some(record) = read_json::<AutoResumeRecord>(&dir.auto_resume())? else {
            return Ok(None);
        };
        let (attempt, attempts, due_at) = match record {
            AutoResumeRecord::Exhausted { attempts } => {
                return Ok(Some(AutoResume::Exhausted { attempts }));
            }
            AutoResumeRecord::Scheduled {
                attempt,
                attempts,
                due_at,
            } => (attempt, attempts, due_at),
        };
        let next_index = turn_index.saturating_add(1);
        let turn = next_index.saturating_add(1);
        let next = self.turn_dir(run_id, next_index);
        if next.holds_a_record() {
            let automatic = read_json::<InvocationRecord>(&next.invocation())?
                .is_some_and(|record| record.auto_resume.is_some());
            return Ok(automatic.then_some(AutoResume::Resumed {
                turn,
                attempt,
                attempts,
            }));
        }
        if dir.auto_resume_cancelled().exists() {
            return Ok(Some(AutoResume::Cancelled { attempt, attempts }));
        }
        let supervised = read_json::<SupervisorRecord>(&dir.supervisor())?
            .is_some_and(|supervisor| !self.child_has_gone(supervisor.launched));
        Ok(Some(if supervised {
            AutoResume::Scheduled {
                turn,
                attempt,
                attempts,
                due_at,
            }
        } else {
            AutoResume::Abandoned {
                due_at,
                supervisor_log: dir.supervisor_log(),
            }
        }))
    }

    /// Resume the consultation once the turn `plan` supervised is due to be, unless something
    /// overtook it, and say how the wait ended.
    ///
    /// Called by the turn's supervisor once the turn has settled, so the decision recorded with
    /// the settlement is already on disk.
    /// The checks that decide whether to resume are made again under the run's lock, which the
    /// launch is made under too, so a `cancel` or a follow-up either comes first and is seen, or
    /// comes after and finds the resumed turn.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotResumable`] when the consultation can no longer be continued, and
    /// whatever reading the run or launching the turn fails with.
    pub(super) async fn resume_when_due(
        &self,
        plan: &SupervisorPlan,
    ) -> Result<Resumption, RunError> {
        let dir = self.turn_dir(&plan.run_id, plan.turn_index);
        let next_index = plan.turn_index.saturating_add(1);
        let Some(AutoResumeRecord::Scheduled {
            attempt,
            attempts,
            due_at,
        }) = read_json::<AutoResumeRecord>(&dir.auto_resume())?
        else {
            return Ok(Resumption::NotScheduled);
        };

        // Wait until it is due, giving up early for a `cancel` or a follow-up.
        loop {
            if dir.auto_resume_cancelled().exists() {
                return Ok(Resumption::Cancelled);
            }
            if self.turn_dir(&plan.run_id, next_index).holds_a_record() {
                return Ok(Resumption::Superseded);
            }
            let Ok(remaining) = (due_at - Utc::now()).to_std() else {
                break;
            };
            if remaining.is_zero() {
                break;
            }
            tokio::time::sleep(remaining.min(RESUME_POLL)).await;
        }

        // Check and claim under one hold of the lock.
        let meta = self.meta(&plan.run_id)?;
        let run = self.lock(&plan.run_id)?;
        if dir.auto_resume_cancelled().exists() {
            return Ok(Resumption::Cancelled);
        }
        let state = run.fold_and_publish(&meta)?.state;
        if state.newest.map(|newest| newest.index) != Some(plan.turn_index) {
            return Ok(Resumption::Superseded);
        }
        let session = Self::resumable_session(&plan.run_id, &state)?;
        let mut delegate = self.current_delegate(&meta, Some(plan.turn_index))?;
        if delegate.isolation().is_none() {
            delegate = delegate.with_isolation(Isolation::Isolated);
        }
        let next = NextTurn {
            delegate: &delegate,
            index: next_index,
            question: CONTINUE,
            resume: Some(&session),
        };
        run.launch_turn(
            &meta,
            &next,
            Resolution::Inherited(Inherited {
                billing: plan.producer.billing,
                attempt: ResumeAttempt { attempt, attempts },
            }),
        )?;
        Ok(Resumption::Resumed)
    }
}

impl LockedRun<'_> {
    /// Decide, as the newest turn's settlement is about to be committed, whether agentmux
    /// resumes the consultation after it, and record the decision in the turn's directory.
    ///
    /// Only a turn that failed overloaded, whose plan carries an [`AutoResumePlan`] and which
    /// nobody asked to cancel, qualifies.
    /// Its own launch record says how many automatic resumes in a row came before it, so a run
    /// that has used up its attempts is recorded as exhausted.
    /// Otherwise the resume is scheduled only while the turn's supervisor is there to make it
    /// and the consultation can be continued at all.
    /// Whoever commits the settlement decides, from the same records, so the decision does not
    /// depend on who that is; one already there is kept.
    pub(super) fn schedule_auto_resume(
        &self,
        state: &RunState,
        turn_index: u32,
        outcome: &Outcome,
    ) -> Result<(), RunError> {
        if !matches!(
            outcome,
            Outcome::Failed {
                kind: FailureKind::Overloaded,
                ..
            }
        ) {
            return Ok(());
        }
        let dir = self.store.turn_dir(&self.run_id, turn_index);
        // A turn somebody asked to cancel is not continued, however it ended.
        if dir.cancelling().exists() {
            return Ok(());
        }
        let Some(SupervisorPlan {
            auto_resume: Some(plan),
            ..
        }) = read_json::<SupervisorPlan>(&dir.supervisor_plan_path())?
        else {
            return Ok(());
        };
        let before = read_json::<InvocationRecord>(&dir.invocation())?
            .and_then(|record| record.auto_resume)
            .map_or(0, |resume| resume.attempt);
        let attempt = before.saturating_add(1);
        if attempt > plan.policy.attempts {
            write_json_once(
                &dir.auto_resume(),
                &AutoResumeRecord::Exhausted { attempts: before },
            )?;
            return Ok(());
        }
        let supervised = read_json::<SupervisorRecord>(&dir.supervisor())?.is_some_and(|record| {
            is_this_process(record.launched) || !self.store.child_has_gone(record.launched)
        });
        if !supervised || RunStore::resumable_session(&self.run_id, state).is_err() {
            return Ok(());
        }
        let delay = TimeDelta::from_std(plan.policy.delay_before(attempt)).unwrap_or_default();
        write_json_once(
            &dir.auto_resume(),
            &AutoResumeRecord::Scheduled {
                attempt,
                attempts: plan.policy.attempts,
                due_at: Utc::now() + delay,
            },
        )?;
        tracing::info!(
            run_id = %self.run_id,
            turn = turn_index,
            attempt,
            %delay,
            "the model was overloaded; resuming automatically",
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use googletest::prelude::*;

    use super::AutoResumePolicy;

    /// The default waits thirty seconds, doubles after each overloaded attempt, never waits more
    /// than fifteen minutes, and allows twelve attempts.
    #[gtest]
    fn the_default_delay_doubles_up_to_its_ceiling() {
        let policy = AutoResumePolicy::DEFAULT;
        let delays: Vec<Duration> = (1..=8)
            .map(|attempt| policy.delay_before(attempt))
            .collect();
        expect_that!(
            delays,
            elements_are![
                eq(&Duration::from_secs(30)),
                eq(&Duration::from_mins(1)),
                eq(&Duration::from_mins(2)),
                eq(&Duration::from_mins(4)),
                eq(&Duration::from_mins(8)),
                eq(&Duration::from_mins(15)),
                eq(&Duration::from_mins(15)),
                eq(&Duration::from_mins(15)),
            ]
        );
        // An attempt far past any streak still waits the ceiling rather than overflowing.
        expect_that!(policy.delay_before(u32::MAX), eq(Duration::from_mins(15)));
        expect_that!(policy.attempts, eq(12));
    }
}
