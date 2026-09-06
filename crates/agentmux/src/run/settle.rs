//! Deciding what happened to a delegate that has stopped speaking.
//!
//! Changes when process semantics or a vendor's exit behaviour changes.
//!
//! [`super`] owns where a consultation lives on disk; this module owns what its capture files
//! mean once the child is gone.
//! Those have different reasons to change, which is why they are different files: a new directory
//! layout does not touch this code, and a CLI that starts reporting failure differently does not
//! touch the store.
//!
//! Terminal state is decided in one order, and the order is the point:
//!
//! 1. a terminal event in the stream, which is authoritative because it survives everything else;
//! 2. `exit.json`, when the child was reaped;
//! 3. process liveness, as a last resort.

use std::time::SystemTime;

use super::{
    ExitRecord, InvocationRecord, LaunchRecord, Meta, RunState, RunStore, TurnDir,
    read_complete_records, read_json, tail_of_file, write_json,
};
use crate::delegate::Vendor;
use crate::launch::{ExitStatus, Liveness};
use crate::stream::{self, Fold};
use crate::transcript::{FailureKind, Message, Outcome, Transcript, Turn};

/// Recover a report the event stream lost but the CLI wrote out anyway.
///
/// Codex's `--output-last-message` file is written by the codex process rather than by the agent,
/// so it survives cases the event stream does not.
/// This only fires when the turn has finished and the stream yielded no report at all — a stream
/// whose shape drifted, or a closing event that never landed.
/// It is the difference between a caller seeing an empty transcript and a caller seeing the answer
/// with a note about where it came from.
///
/// The recovered text lands in [`Turn::recovered`] rather than in [`Turn::messages`], because it
/// becomes readable strictly after the terminal event and so must render after the footer.
/// Codex writes the file about 228 ms after `turn.completed` reaches stdout, which is wider than
/// any poll interval, so a transcript really can be rendered complete before this text exists.
fn recover_from_last_message(dir: &TurnDir, turn: &mut Turn) {
    if !turn.outcome.is_terminal() || turn.messages.iter().any(Message::is_report_content) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(dir.last_message()) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    turn.recovered = Some(text);
}

impl RunStore {
    /// Rebuild a consultation from its capture files.
    ///
    /// This is the only place a `Transcript` comes from.
    /// Nothing is cached in memory between calls, which is what makes an agentmux restart
    /// mid-review a non-event.
    pub(super) fn fold_run(&self, meta: &Meta) -> RunState {
        let vendor = meta.delegate.vendor();
        let mut transcript = Transcript::default();
        let mut session = None;
        let mut newest_turn_index = 0;
        let mut newest_launch = None;
        let mut last_activity = None;

        for index in 0.. {
            let dir = self.turn_dir(&meta.run_id, index);
            if !dir.root.is_dir() {
                break;
            }
            newest_turn_index = index;

            let question = std::fs::read_to_string(dir.question()).unwrap_or_default();
            let events = read_complete_records(&dir.events());
            if let Ok(modified) = std::fs::metadata(dir.events()).and_then(|m| m.modified()) {
                last_activity = Some(last_activity.map_or(modified, |seen: SystemTime| {
                    if modified > seen { modified } else { seen }
                }));
            }

            let mut fold = stream::fold(vendor, index, &question, &events);
            let launch = read_json::<LaunchRecord>(&dir.launch());

            // A recorded cancellation outranks a terminal event that arrived afterwards.
            // A child can ignore or outrun SIGTERM and still emit `turn.completed`, and letting
            // that replace the cancellation would insert its last message above a footer already
            // handed out and re-advertise the consultation as resumable.
            let cancelled = read_json::<ExitRecord>(&dir.exit()).is_some_and(|exit| exit.cancelled);

            // One observation of the child, made once and reused, because two observations of a
            // moving target disagree.
            //
            // Reaping comes first: a finished child that agentmux still parents stays a zombie
            // until it is waited on, and a zombie keeps its process group, so asking about
            // liveness first would report it alive forever and the consultation would poll
            // without end.
            //
            // Then, if it is gone, the capture is read again.
            // The first read happened before this observation and a child can flush its terminal
            // event in between, so settling on the older read would record a failure that the
            // next call re-folds as a success, moving bytes a `tail` cursor had already passed.
            // A child observed gone has written everything it will write, so this read is
            // final.
            let departure = launch.as_ref().map(|record| {
                let status = self.launcher.reap(record.launched);
                (status, self.launcher.liveness(record.launched))
            });
            if !fold.turn.outcome.is_terminal() && matches!(departure, Some((_, Liveness::Gone))) {
                let settled = read_complete_records(&dir.events());
                if settled.len() != events.len() {
                    fold = stream::fold(vendor, index, &question, &settled);
                }
            }

            let Fold {
                mut turn,
                session: turn_session,
            } = fold;

            // Both CLIs accept a resume against a session they no longer hold and answer from an
            // empty context rather than failing, so the only evidence is the identifier they hand
            // back.
            // A turn that asked to continue one conversation and opened another is a fresh
            // consultation wearing a follow-up's clothes.
            if let Some(record) = read_json::<InvocationRecord>(&dir.invocation())
                && let Some(asked_for) = record.resumed_from
                && let Some(opened) = turn_session.as_ref()
                && *opened != asked_for
            {
                turn.broke_continuity = true;
            }

            if turn_session.is_some() {
                session = turn_session;
            }

            if cancelled {
                turn.outcome = Outcome::Cancelled;
            } else if !turn.outcome.is_terminal() {
                Self::settle_unfinished(&dir, launch.as_ref(), departure, &mut turn, vendor);
            }
            recover_from_last_message(&dir, &mut turn);
            if launch.is_some() {
                newest_launch = launch;
            }
            transcript.turns.push(turn);
        }

        RunState {
            transcript,
            session,
            newest_turn_index,
            newest_launch,
            last_activity,
        }
    }

    /// Decide the outcome of a turn whose stream carries no terminal event.
    ///
    /// The stream comes first precisely because it survives everything else; only when it is
    /// silent do `exit.json` and process liveness get a say.
    fn settle_unfinished(
        dir: &TurnDir,
        launch: Option<&LaunchRecord>,
        departure: Option<(Option<ExitStatus>, Liveness)>,
        turn: &mut Turn,
        vendor: Vendor,
    ) {
        if let Some(exit) = read_json::<ExitRecord>(&dir.exit()) {
            if exit.cancelled {
                turn.outcome = Outcome::Cancelled;
                return;
            }
            if let Some(status) = exit.status {
                turn.outcome = Self::died_without_saying_so(dir, status, vendor);
                return;
            }
        }

        let Some(_launch) = launch else {
            // No launch record and no terminal event: the spawn itself never completed.
            turn.outcome = Outcome::Failed {
                kind: FailureKind::LaunchFailed,
                detail: "agentmux recorded no child process for this turn".to_owned(),
            };
            return;
        };

        match departure {
            // Alive: nothing to settle.
            // Unknown: this platform cannot answer without the child handle, and guessing "dead"
            // would abandon a live review.
            None | Some((_, Liveness::Alive | Liveness::Unknown)) => {}
            Some((status, Liveness::Gone)) => {
                // Record the exit so the next fold does not have to observe the process again.
                let _ = write_json(
                    &dir.exit(),
                    &ExitRecord {
                        status,
                        cancelled: false,
                    },
                );
                tracing::warn!(
                    ?status,
                    "delegate exited without a completion event in its stream"
                );
                turn.outcome = Self::died_without_saying_so(
                    dir,
                    status.unwrap_or(ExitStatus {
                        code: None,
                        signal: None,
                    }),
                    vendor,
                );
            }
        }
    }

    /// Build a failure for a child that exited without a terminal event in its stream.
    ///
    /// The delegate's stderr is the only remaining evidence, so its tail goes into the detail —
    /// this is the difference between "it failed" and a message a caller can act on.
    fn died_without_saying_so(dir: &TurnDir, status: ExitStatus, vendor: Vendor) -> Outcome {
        if status.is_success() {
            // Exit 0 with no terminal event means the CLI's output format changed under us.
            return Outcome::Failed {
                kind: FailureKind::Unclassified,
                detail: format!(
                    "the {vendor} CLI exited successfully but its event stream carried no \
                     completion event. Its output format may have changed; the raw stream is at {}",
                    dir.events().display()
                ),
            };
        }
        let stderr = tail_of_file(&dir.stderr(), 2000);
        let detail = if stderr.trim().is_empty() {
            format!("the {vendor} CLI exited with {status:?} and wrote nothing to stderr")
        } else {
            format!("the {vendor} CLI exited with {status:?}: {}", stderr.trim())
        };
        let kind = match stream::classify(&detail) {
            FailureKind::Unclassified => FailureKind::LaunchFailed,
            classified => classified,
        };
        Outcome::Failed { kind, detail }
    }
}
