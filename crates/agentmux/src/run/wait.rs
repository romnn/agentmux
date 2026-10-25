//! Waiting for a turn to settle.
//!
//! Changes when what a wait returns on changes.
//!
//! A wait returns on a turn's `settled.json`, never on a transcript that merely looks finished:
//! the marker is written after the final transcript, by whoever settles the turn, so a waiter
//! that finds it finds the bytes it vouches for.
//! It is read from disk on every poll rather than signalled, which works the same on every
//! platform and whether or not the turn's supervisor is still there to signal anything.
//! Where nothing is left to settle the turn — no supervisor, or one that died — the waiter settles
//! it itself, through the same [`RunStore::finalize_turn`] every other party uses.

use std::time::Duration;

use serde::Serialize;

use crate::transcript::turn_number;

use super::RunStore;
use super::error::RunError;
use super::id::RunId;
use super::records::SettledTurn;
use super::status::{TranscriptPage, page_of};

/// One turn of one consultation, chosen once.
///
/// A wait holds on to the turn it was asked about, so a follow-up that begins while it waits is
/// never handed out in its place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnRef {
    run_id: RunId,
    #[serde(rename = "turn", serialize_with = "turn_number")]
    turn_index: u32,
}

impl TurnRef {
    /// The consultation.
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// The turn's zero-based index.
    #[must_use]
    pub fn turn_index(&self) -> u32 {
        self.turn_index
    }

    /// The turn's one-based number, as the transcript's headings show it.
    #[must_use]
    pub fn turn_number(&self) -> u32 {
        self.turn_index.saturating_add(1)
    }
}

/// When a wait on several turns returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaitFor {
    /// Once every one has settled.
    #[default]
    All,
    /// Once any one has; among several settled at once, the first asked for.
    Any,
}

/// What a wait returned with.
#[derive(Debug, Clone)]
pub struct Waited {
    /// The settled turns it returns, in the order they were asked for: every one under
    /// [`WaitFor::All`], and the one it returned on under [`WaitFor::Any`].
    /// On a timeout, every one that had settled by then.
    pub settled: Vec<SettledTurn>,
    /// The turns that had not settled, in the order they were asked for.
    pub pending: Vec<TurnRef>,
    /// Whether the deadline passed first.
    pub timed_out: bool,
}

/// First interval between looks for a settlement.
const MIN_POLL: Duration = Duration::from_millis(100);

/// Longest interval a wait backs off to, which bounds how late it returns after a settlement.
const MAX_POLL: Duration = Duration::from_millis(500);

impl RunStore {
    /// Choose the turn a wait is about: `turn`, counted from 1, or the newest turn there is now.
    ///
    /// Decided once, from which records are on disk and without taking the lock, so it never
    /// waits on another process: a turn whose launch is recorded a moment later is simply not
    /// the one chosen.
    /// No record is read, so a damaged one — in another turn, or another consultation of the
    /// same wait — stops nothing here; a turn's settlement is validated when the wait looks at
    /// that turn.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, and
    /// [`RunError::NoSuchTurn`] when it has no turn `turn`; a turn that has not begun yet is not
    /// one to wait for.
    pub fn pin_turn(&self, run_id: &RunId, turn: Option<u32>) -> Result<TurnRef, RunError> {
        self.meta(run_id)?;
        let turns = u32::try_from(
            self.turn_dirs(run_id)
                .take_while(|(_, dir)| dir.holds_a_record())
                .count(),
        )
        .unwrap_or(u32::MAX);
        let number = turn.unwrap_or(turns);
        if number == 0 || number > turns {
            return Err(RunError::NoSuchTurn {
                run_id: run_id.clone(),
                turn: number.max(1),
                turns,
            });
        }
        Ok(TurnRef {
            run_id: run_id.clone(),
            turn_index: number - 1,
        })
    }

    /// Wait until `target` has settled, however long that takes.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when the consultation is removed underneath the wait,
    /// [`RunError::UncertainStart`] when its turn can never settle, and
    /// [`RunError::InvalidSettlement`] when its marker does not hold.
    pub async fn wait_for_settlement(&self, target: &TurnRef) -> Result<SettledTurn, RunError> {
        let mut interval = MIN_POLL;
        loop {
            if let Some(settled) = self.look_for_settlement(target)? {
                return Ok(settled);
            }
            tokio::time::sleep(interval).await;
            interval = (interval * 2).min(MAX_POLL);
        }
    }

    /// Wait until `targets` have settled, as `until` says, or until `deadline`.
    ///
    /// Each look reads the markers; a turn without one is settled here only when nothing else
    /// will — it has a record already, or its delegate has gone and no live supervisor is about
    /// to — so a running turn's growing capture is not folded on every look.
    /// Targets are looked at in the order given, and under [`WaitFor::Any`] the first one found
    /// settled ends the wait before any later one is looked at, so a later target's failure
    /// cannot take the place of a settlement already found.
    /// The interval backs off from a tenth of a second to half of one, so a settlement is seen
    /// within half a second whenever it happens.
    ///
    /// # Errors
    ///
    /// As [`RunStore::wait_for_settlement`].
    pub async fn wait_for(
        &self,
        targets: &[TurnRef],
        until: WaitFor,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Waited, RunError> {
        struct Watched<'a> {
            target: &'a TurnRef,
            settled: Option<SettledTurn>,
        }
        let mut watched: Vec<Watched<'_>> = targets
            .iter()
            .map(|target| Watched {
                target,
                settled: None,
            })
            .collect();
        let mut interval = MIN_POLL;
        loop {
            let mut found_one = false;
            for entry in &mut watched {
                if entry.settled.is_none() {
                    entry.settled = self.look_for_settlement(entry.target)?;
                }
                found_one |= entry.settled.is_some();
                if found_one && until == WaitFor::Any {
                    break;
                }
            }
            let done = match until {
                WaitFor::All => watched.iter().all(|entry| entry.settled.is_some()),
                WaitFor::Any => found_one,
            };
            let now = tokio::time::Instant::now();
            let timed_out = !done && deadline.is_some_and(|deadline| now >= deadline);
            if done || timed_out {
                let mut settled = Vec::new();
                let mut pending = Vec::new();
                for entry in watched {
                    match entry.settled {
                        Some(turn) => settled.push(turn),
                        None => pending.push(entry.target.clone()),
                    }
                }
                return Ok(Waited {
                    settled,
                    pending,
                    timed_out,
                });
            }
            let sleep = deadline.map_or(interval, |deadline| interval.min(deadline - now));
            tokio::time::sleep(sleep).await;
            interval = (interval * 2).min(MAX_POLL);
        }
    }

    /// The turn's settlement if it has one now, settling it first when nothing else will.
    ///
    /// One attempt, which never waits for the run's lock: while another process holds it, the
    /// turn is not settled yet as far as this look can tell, and the next look tries again.
    fn look_for_settlement(&self, target: &TurnRef) -> Result<Option<SettledTurn>, RunError> {
        let settled = self.read_settled(&target.run_id, target.turn_index)?;
        if settled.as_ref().is_some_and(SettledTurn::is_published) {
            return Ok(settled);
        }
        let files = self.read_turn_files(
            &target.run_id,
            target.turn_index,
            self.turn_dir(&target.run_id, target.turn_index),
        )?;
        // A marker whose transcript is missing is settled already and only needs it written
        // again; otherwise a live supervisor settles its own turn, with the exit status only it
        // holds.
        let left_to_this_waiter = settled.is_some()
            || files.exit.is_some()
            || (!self.supervisor_will_settle(files.supervisor.as_ref())
                && files
                    .launch
                    .as_ref()
                    .is_none_or(|record| self.child_has_gone(record.launched)));
        if left_to_this_waiter {
            self.try_finalize_turn(&target.run_id, target.turn_index)
        } else {
            Ok(None)
        }
    }
}

impl SettledTurn {
    /// Whether the published transcript still holds everything this settlement commits.
    ///
    /// It is a cache a fold writes again, so one that has gone or been cut short is repaired by
    /// finalizing the turn, which keeps the marker; [`SettledTurn::page`] refuses it.
    #[must_use]
    pub fn is_published(&self) -> bool {
        std::fs::metadata(self.transcript_path())
            .is_ok_and(|metadata| metadata.len() >= self.transcript_end())
    }

    /// A page of the transcript a settlement vouches for, which ends where the settled turn does
    /// however much a later turn has appended since.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] when the transcript cannot be read, and
    /// [`RunError::InvalidSettlement`] when it no longer holds what the settlement committed.
    pub fn page(&self, offset: u64, max_bytes: usize) -> Result<TranscriptPage, RunError> {
        let path = self.transcript_path();
        let mut bytes = std::fs::read(path).map_err(RunError::io(format!(
            "reading the transcript at {}",
            path.display()
        )))?;
        let end = usize::try_from(self.transcript_end()).unwrap_or(usize::MAX);
        let invalid = |reason: &str| RunError::InvalidSettlement {
            path: self.marker_path().to_path_buf(),
            reason: reason.to_owned(),
        };
        if bytes.len() < end {
            return Err(invalid("the transcript is shorter than it commits"));
        }
        bytes.truncate(end);
        let text = String::from_utf8(bytes)
            .map_err(|_| invalid("the transcript does not end where it commits on a character"))?;
        Ok(page_of(&text, offset, max_bytes))
    }
}
