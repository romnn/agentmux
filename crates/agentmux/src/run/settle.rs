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
//! 2. `exit.json`, when the child was reaped, observed gone, or cancelled;
//! 3. process liveness, as a last resort — and only for a turn nothing has settled yet, so a
//!    settled turn is never observed again and a recycled pid cannot be mistaken for it.
//!
//! # Why a settled turn's fold input is frozen
//!
//! The rendered transcript is paged by byte offset, so once a turn's footer has been handed out
//! nothing may land above it.
//! A turn becomes terminal in exactly three ways, and each freezes what will be folded: a
//! terminal event ends the fold at that line; a child observed gone is recorded together with
//! how much capture existed and what its stderr said, and the fold reads neither past that
//! length nor from the live stderr again; and a cancellation records the capture length once
//! the child was asked and then made to stop, so nothing that could be written afterwards is read.
//! A cancellation record freezes the capture without proving the child left, which is why
//! removing a run still asks about a cancelled turn's child.
//! Whatever record settles a turn, the turn is rendered from exactly the capture length that
//! record froze, whichever caller wrote it.

use std::path::Path;
use std::time::SystemTime;

use codex_app_server_protocol::rollout::{RateLimitSnapshot, RateLimitWindow};

use super::layout::TurnDir;
use super::lock::LockedRun;
use super::records::{
    ExitRecord, InvocationRecord, LaunchMode, Meta, TurnFiles, read_complete_records, read_json,
    tail_of_file, write_json_once,
};
use super::{NewestTurn, RunError, RunState, RunStore, SettledBy};
use crate::delegate::{SessionRef, Vendor};
use crate::launch::{ExitStatus, Liveness};
use crate::stream::{self, Fold};
use crate::transcript::{FailureKind, Message, Outcome, RateLimit, Transcript, Turn};

/// How many bytes of a departed child's stderr are kept with its exit record.
const STDERR_TAIL: usize = 2000;

/// One turn, folded from its own files.
#[derive(Debug)]
pub(super) struct FoldedTurn {
    pub(super) turn: Turn,
    /// The session this turn announced, if it announced one.
    pub(super) session: Option<SessionRef>,
    /// The newest write to any of this turn's capture files.
    pub(super) last_activity: Option<SystemTime>,
    /// What decided the outcome, once something has.
    ///
    /// An outcome the record settled already quotes the stderr tail the record froze; a reader
    /// that would quote stderr again asks this rather than the disk, which may have gained a
    /// record since the fold.
    pub(super) settled_by: Option<SettledBy>,
    /// How the turn's delegate was launched.
    pub(super) launch_mode: LaunchMode,
    /// Whether the turn's capture says the turn itself began.
    pub(super) turn_started: bool,
    /// Where its usage window is still to be read from, when its stream carried none.
    pub(super) pending_rate_limit_lookup: Option<RateLimitLookup>,
}

/// A finished Codex turn whose usage window is still to be read from its session rollout.
///
/// Searched for outside the run's lock, by [`RunStore::resolve_rate_limit_lookups`]: the search
/// walks the vendor's whole session tree, and the transcript does not depend on what it finds.
#[derive(Debug)]
pub(super) struct RateLimitLookup {
    /// The turn's index.
    turn_index: u32,
    /// The turn's directory, which caches the answer.
    turn_dir: TurnDir,
    /// The `CODEX_HOME` the launch recorded, when it recorded one.
    codex_home: Option<std::path::PathBuf>,
    /// The thread whose rollout to read.
    thread_id: SessionRef,
    /// Whether the turn's delegate had gone before the search, so its rollout was final.
    delegate_has_exited: bool,
}

/// What one observation of a child that had gone established.
struct Departure {
    status: Option<ExitStatus>,
    events_len: u64,
    stderr_tail: String,
}

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
///
/// It is still append-only for the consultation, because a reply is only ever recovered into the
/// newest turn — checked under the run lock, at the moment it is kept — and once kept it is read
/// from beside the turn by every later fold.
/// A file that lands for an earlier turn after a later one exists is left where it is rather
/// than inserted above the turn that followed; one kept before then stays.
impl LockedRun<'_> {
    fn recover_from_last_message(
        &self,
        files: &TurnFiles,
        turn: &mut Turn,
    ) -> Result<(), RunError> {
        let dir = &files.dir;
        if !turn.outcome.is_terminal() || turn.messages.iter().any(Message::is_report_content) {
            return Ok(());
        }
        if let Some(kept) = read_text(&dir.recovered())? {
            turn.recovered = Some(kept);
            return Ok(());
        }
        let Some(text) = read_text(&dir.last_message())?.filter(|text| !text.trim().is_empty())
        else {
            return Ok(());
        };
        // Kept before it is rendered, so what a reader is handed cannot later go missing, and
        // only into the newest turn, which the lock makes a fact rather than a guess: no turn can
        // be claimed while it is held.
        // Text that cannot be kept is not rendered either; a note the turn never gains keeps
        // every page a prefix of the next, where a note that came and went would not.
        if self
            .store
            .turn_dir(&self.run_id, files.index.saturating_add(1))
            .root
            .exists()
        {
            return Ok(());
        }
        if let Err(err) = crate::atomic_file::write_whole(&dir.recovered(), text.as_bytes()) {
            tracing::warn!(
                ?err,
                path = %dir.recovered().display(),
                "cannot keep a recovered reply"
            );
            return Ok(());
        }
        turn.recovered = Some(text);
        Ok(())
    }
}

/// A text file's contents, or `None` for one that is not there.
fn read_text(path: &std::path::Path) -> Result<Option<String>, RunError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(RunError::Io {
            context: format!("reading {}", path.display()),
            source,
        }),
    }
}

/// Whether a turn that asked to continue a session actually did.
///
/// Both CLIs accept a resume against a session they no longer hold and answer from an empty
/// context rather than failing, so the only evidence is the identifier they hand back.
/// A turn that asked to continue one conversation and opened another is a fresh consultation
/// wearing a follow-up's clothes.
/// A resumed turn that announced no session at all is counted as drift: the field the evidence
/// lives in has moved, and continuity can no longer be checked.
fn note_continuity(asked_for: Option<&SessionRef>, opened: Option<&SessionRef>, turn: &mut Turn) {
    let Some(asked_for) = asked_for else {
        return;
    };
    match opened {
        Some(opened) if opened != asked_for => turn.broke_continuity = true,
        None if turn.outcome.is_terminal() => {
            turn.unrecognised.record("session.<not announced>");
        }
        _ => {}
    }
}

/// The window a Codex turn's usage is reported against.
///
/// An exhausted window is the one a request is refused against, and when several are, the one
/// that reopens last is the one that decides when a request can succeed again.
/// With none exhausted it is the one closest to closing, which is what the next question has to
/// expect.
/// A window that names no reopening time cannot answer either question, and is passed over.
fn select_rate_limit_window(snapshot: &RateLimitSnapshot) -> Option<&RateLimitWindow> {
    [&snapshot.primary, &snapshot.secondary]
        .into_iter()
        .flatten()
        .filter(|window| window.resets_at.is_some())
        .max_by(|left_window, right_window| {
            left_window
                .exhausted()
                .cmp(&right_window.exhausted())
                .then_with(|| {
                    if left_window.exhausted() {
                        left_window.resets_at.cmp(&right_window.resets_at)
                    } else {
                        left_window
                            .used_percent
                            .total_cmp(&right_window.used_percent)
                    }
                })
        })
}

/// The newest modification time among a turn's capture files.
pub(super) fn last_write(dir: &TurnDir) -> Option<SystemTime> {
    [dir.events(), dir.stderr()]
        .iter()
        .filter_map(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
        .max()
}

impl RunStore {
    /// Read every usage window the fold left to a rate-limit lookup, and remember each answer.
    ///
    /// The absence of one is remembered too, once the delegate has gone.
    /// Its rollout does not gain rate limits after that, so a fruitless search repeated on every
    /// `status` would walk the vendor's whole session tree for nothing.
    /// Before then the CLI may still be writing it, and a miss remembered early would hide the
    /// window for good.
    /// The first answer remembered is the one every caller adopts: a follow-up resuming the same
    /// thread appends to the same rollout, so two searches can disagree, and overwriting would let
    /// a later miss erase an earlier hit.
    pub(super) fn resolve_rate_limit_lookups(&self, meta: &Meta, state: &mut RunState) {
        for lookup in std::mem::take(&mut state.pending_rate_limit_lookups) {
            let found = self.codex_rate_limit(
                meta,
                lookup.codex_home.as_deref(),
                lookup.thread_id.as_str(),
            );
            let found = if found.is_some() || lookup.delegate_has_exited {
                write_json_once(&lookup.turn_dir.rate_limit(), &found).unwrap_or_else(|err| {
                    tracing::debug!(?err, "not remembering a turn's usage window");
                    found
                })
            } else {
                found
            };
            if let Some(turn) = state
                .transcript
                .turns
                .iter_mut()
                .find(|turn| turn.index == lookup.turn_index)
            {
                turn.rate_limit = found;
            }
        }
    }

    /// The usage window Codex recorded while running one turn.
    ///
    /// Its rollout files live under the `CODEX_HOME` the launch recorded.
    /// A turn launched before agentmux recorded it is searched where the account resolves now.
    fn codex_rate_limit(
        &self,
        meta: &Meta,
        codex_home: Option<&Path>,
        thread_id: &str,
    ) -> Option<RateLimit> {
        let codex_home = if let Some(recorded) = codex_home {
            recorded.to_path_buf()
        } else {
            let config = crate::config::Config::load(&self.host_env, &meta.cwd).ok()?;
            let env = meta.delegate.identity_env(&self.host_env, &config).ok()?;
            Vendor::Codex.config_dir(&env)?
        };

        // Only the account-wide bucket is recorded here; the per-model ones are not in a rollout.
        let snapshot = crate::quota::codex_rollout_rate_limits(&codex_home, thread_id)?;
        let resets_at = select_rate_limit_window(&snapshot)?
            .resets_at
            .and_then(chrono::DateTime::from_timestamp_secs)?;
        Some(RateLimit {
            resets_at,
            window: snapshot.limit_id,
        })
    }
}

impl LockedRun<'_> {
    /// Rebuild a consultation from its capture files.
    ///
    /// This is the only place a `Transcript` comes from.
    /// Nothing is cached in memory between calls, which is what makes an agentmux restart
    /// mid-review a non-event.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] when a capture file exists but cannot be read.
    /// A file that is not there yet is an empty capture; one that cannot be read is not, because
    /// treating it as empty would make the transcript shrink and grow with the failure.
    pub(super) fn fold_run(&self, meta: &Meta) -> Result<RunState, RunError> {
        let mut transcript = Transcript::default();
        let mut session = None;
        let mut newest = None;
        let mut last_activity: Option<SystemTime> = None;
        let mut pending_rate_limit_lookups = Vec::new();

        let all_files = self.store.turn_files(&self.run_id)?;
        for files in &all_files {
            let folded = self.fold_turn(meta, files)?;
            pending_rate_limit_lookups.extend(folded.pending_rate_limit_lookup);
            newest = Some(NewestTurn {
                index: files.index,
                settled_by: folded.settled_by,
                launch_mode: folded.launch_mode,
                turn_started: folded.turn_started,
                // Asked of the newest turn alone, below: it costs a process lookup.
                delegate_has_exited: false,
            });
            if folded.session.is_some() {
                session = folded.session;
            }
            if let Some(seen) = folded.last_activity {
                last_activity = Some(last_activity.map_or(seen, |known| known.max(seen)));
            }
            transcript.turns.push(folded.turn);
        }
        if let (Some(newest), Some(files)) = (newest.as_mut(), all_files.last()) {
            newest.delegate_has_exited = self.store.delegate_has_exited(files);
        }

        Ok(RunState {
            transcript,
            session,
            newest,
            last_activity,
            pending_rate_limit_lookups,
        })
    }

    /// The outcome of the run's newest turn, folded on its own, or `None` for a run with no turn.
    ///
    /// A run's outcome is its newest turn's, and a turn folds from its own files alone, so the
    /// callers that only need to know whether a consultation is over — the poll loop, `list`,
    /// the sweep — do not re-read every settled turn's capture to find out.
    pub(super) fn fold_newest_turn(&self, meta: &Meta) -> Result<Option<Turn>, RunError> {
        let turns = self.store.turn_files(&self.run_id)?;
        let Some(newest) = turns.last() else {
            return Ok(None);
        };
        Ok(Some(self.fold_turn(meta, newest)?.turn))
    }

    /// Read a finished Codex turn's remembered usage window, or say where to search for one.
    ///
    /// Codex says nothing about usage on the stream agentmux captures, but records it in the
    /// session rollout it writes anyway.
    /// Reading that afterwards is the only way a Codex consultation reports a usage window at
    /// all, and it costs neither a process nor a token.
    /// Only once the turn is over: a running turn's rollout is still being written, and the
    /// search is too expensive to repeat on every fold.
    /// A remembered answer is read here; a search is left to the caller, after the lock.
    fn cached_rate_limit_or_lookup(
        &self,
        files: &TurnFiles,
        turn: &mut Turn,
        thread_id: Option<&SessionRef>,
        codex_home: Option<&Path>,
    ) -> Option<RateLimitLookup> {
        let thread_id =
            thread_id.filter(|_| turn.rate_limit.is_none() && turn.outcome.is_terminal())?;
        match read_json::<Option<RateLimit>>(&files.dir.rate_limit()) {
            Ok(Some(remembered)) => {
                turn.rate_limit = remembered;
                None
            }
            Ok(None) | Err(_) => Some(RateLimitLookup {
                turn_index: files.index,
                turn_dir: files.dir.clone(),
                codex_home: codex_home.map(Path::to_path_buf),
                thread_id: thread_id.clone(),
                delegate_has_exited: self.store.delegate_has_exited(files),
            }),
        }
    }

    /// Fold one turn from its own files, settling it if its child has gone, and keeping a reply
    /// the CLI wrote out late when the turn is the newest.
    ///
    /// Reading a turn never depends on another turn's capture, which is what lets a caller that
    /// only needs the newest turn's state fold that one alone.
    pub(super) fn fold_turn(&self, meta: &Meta, files: &TurnFiles) -> Result<FoldedTurn, RunError> {
        let vendor = meta.delegate.vendor();
        let dir = &files.dir;
        let question = match std::fs::read_to_string(dir.question()) {
            Ok(question) => question,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(RunError::Io {
                    context: format!("reading {}", dir.question().display()),
                    source,
                });
            }
        };
        // A settled turn's capture is read only up to the length its record froze.
        let frozen_len = files.exit.as_ref().map(ExitRecord::events_len);
        let (events, mut folded_len) = read_complete_records(&dir.events(), frozen_len)?;
        let mut fold = stream::fold(vendor, files.index, &question, &events);

        // Reaping goes through the handle this process retained, whatever the turn's state: it
        // costs a lookup, cannot mistake another process for the child, and is what keeps a
        // finished child from lingering as a zombie.
        // A zombie keeps its process group, so asking about liveness first would report it alive
        // forever and the consultation would poll without end.
        let reaped = files
            .launch
            .as_ref()
            .and_then(|record| self.store.launcher.reap(record.launched));

        // One observation of the child, made only while nothing has settled the turn, so a
        // settled turn's pid is never asked about again.
        //
        // If it is gone, the capture is read again.
        // The first read happened before this observation and a child can flush its terminal
        // event in between, so settling on the older read would record a failure that the next
        // call re-folds as a success, moving bytes a `tail` cursor had already passed.
        // A child observed gone has written everything it will write, so this read is final, and
        // its length is what the record freezes.
        let mut departure = None;
        if !fold.turn.outcome.is_terminal()
            && files.exit.is_none()
            && let Some(record) = files.launch.as_ref()
        {
            let liveness = if reaped.is_some() {
                Liveness::Gone
            } else {
                self.store.launcher.liveness(record.launched)
            };
            if liveness == Liveness::Gone {
                let (settled, settled_len) = read_complete_records(&dir.events(), None)?;
                if settled.len() != events.len() {
                    fold = stream::fold(vendor, files.index, &question, &settled);
                }
                folded_len = settled_len;
                // The record freezes what was folded, by construction rather than by timing.
                departure = Some(Departure {
                    status: reaped,
                    events_len: settled_len,
                    stderr_tail: tail_of_file(&dir.stderr(), STDERR_TAIL)?,
                });
            }
        }

        // Settle, and then render from exactly what the record froze.
        // Another caller may have settled this turn first with a different reading, and the
        // record it wrote is the one every later fold reads, so this fold reads it too rather
        // than publishing the reading it happened to make.
        let record = if fold.turn.outcome.is_terminal() {
            None
        } else {
            RunStore::settle_unfinished(dir, files.exit.as_ref(), departure)?
        };
        if let Some(record) = &record
            && record.events_len() != folded_len
        {
            let (frozen, _) = read_complete_records(&dir.events(), Some(record.events_len()))?;
            fold = stream::fold(vendor, files.index, &question, &frozen);
        }

        let Fold {
            mut turn,
            session: turn_session,
            turn_started,
        } = fold;
        let settled_by = match &record {
            Some(record) => {
                turn.outcome = RunStore::outcome_of(dir, record, vendor);
                Some(SettledBy::ExitRecord)
            }
            None => turn
                .outcome
                .is_terminal()
                .then_some(SettledBy::TerminalEvent),
        };

        // What the launch recorded before the spawn: which session it asked to resume, and
        // whether the child was ever going to be able to save its own.
        let invocation = read_json::<InvocationRecord>(&dir.invocation())?.unwrap_or_default();
        turn.unsaved_session_store = invocation.unsaved_session_store;
        turn.billing = invocation.billing;
        let launch_mode = invocation.launch_mode;
        note_continuity(
            invocation.resumed_from.as_ref(),
            turn_session.as_ref(),
            &mut turn,
        );

        let pending_rate_limit_lookup = if vendor == Vendor::Codex {
            self.cached_rate_limit_or_lookup(
                files,
                &mut turn,
                turn_session.as_ref(),
                invocation.config_dir.as_deref(),
            )
        } else {
            None
        };

        self.recover_from_last_message(files, &mut turn)?;

        Ok(FoldedTurn {
            turn,
            session: turn_session,
            last_activity: last_write(dir),
            settled_by,
            launch_mode,
            turn_started,
            pending_rate_limit_lookup,
        })
    }
}

impl RunStore {
    /// The record that settles a turn whose stream carries no terminal event, if anything does.
    ///
    /// The stream comes first precisely because it survives everything else; only when it is
    /// silent do `exit.json` and process liveness get a say.
    fn settle_unfinished(
        dir: &TurnDir,
        exit: Option<&ExitRecord>,
        departure: Option<Departure>,
    ) -> Result<Option<ExitRecord>, RunError> {
        match (exit, departure) {
            (Some(record), _) => Ok(Some(record.clone())),
            // Alive, or unobservable: nothing to settle.
            // Guessing "dead" for a platform that cannot answer would abandon a live review.
            (None, None) => Ok(None),
            // Recorded once, so a second caller observing the same departure adopts this record
            // rather than settling on its own, possibly different, reading.
            (None, Some(departure)) => {
                // A child that went after a cancellation was asked for went because of it, and
                // is recorded as cancelled by whichever caller sees it go first.
                let record = if dir.cancelling().exists() {
                    ExitRecord::Cancelled {
                        events_len: departure.events_len,
                    }
                } else {
                    tracing::warn!(
                        status = ?departure.status,
                        "delegate exited without a completion event in its stream"
                    );
                    ExitRecord::Exited {
                        status: departure.status,
                        events_len: departure.events_len,
                        stderr_tail: departure.stderr_tail,
                    }
                };
                write_json_once(&dir.exit(), &record).map(Some)
            }
        }
    }

    /// What a settled turn's record says happened.
    fn outcome_of(dir: &TurnDir, record: &ExitRecord, vendor: Vendor) -> Outcome {
        match record {
            ExitRecord::Cancelled { .. } => Outcome::Cancelled,
            ExitRecord::Exited {
                status,
                stderr_tail,
                ..
            } => Self::died_without_saying_so(dir, *status, stderr_tail, vendor),
        }
    }

    /// Build a failure for a child that exited without a terminal event in its stream.
    ///
    /// The delegate's stderr is the only remaining evidence, so the tail recorded with its exit
    /// goes into the detail — this is the difference between "it failed" and a message a caller
    /// can act on.
    fn died_without_saying_so(
        dir: &TurnDir,
        status: Option<ExitStatus>,
        stderr_tail: &str,
        vendor: Vendor,
    ) -> Outcome {
        if status.is_some_and(|status| status.is_success()) {
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
        let how = match status {
            Some(status) => format!("exited with {status}"),
            // The child was reparented before it ended, which happens when agentmux was
            // restarted underneath it.
            None => {
                "exited while agentmux was not its parent, so its exit status is unknown".to_owned()
            }
        };
        let stderr = stderr_tail.trim();
        let detail = if stderr.is_empty() {
            format!("the {vendor} CLI {how} and wrote nothing to stderr")
        } else {
            format!("the {vendor} CLI {how}: {stderr}")
        };
        let kind = match stream::classify(&detail) {
            FailureKind::Unclassified => FailureKind::LaunchFailed,
            classified => classified,
        };
        Outcome::Failed { kind, detail }
    }
}

#[cfg(test)]
mod tests {
    use codex_app_server_protocol::rollout::{RateLimitSnapshot, RateLimitWindow};
    use googletest::prelude::*;

    use super::select_rate_limit_window;

    fn window(used_percent: f64, resets_at: Option<i64>) -> RateLimitWindow {
        RateLimitWindow {
            used_percent,
            window_minutes: None,
            resets_at,
        }
    }

    fn selected_reset_timestamp(
        primary: Option<RateLimitWindow>,
        secondary: Option<RateLimitWindow>,
    ) -> Option<i64> {
        let snapshot = RateLimitSnapshot {
            limit_id: None,
            primary,
            secondary,
        };
        select_rate_limit_window(&snapshot).and_then(|window| window.resets_at)
    }

    /// The window a refusal is reported against is the exhausted one, however full the other is,
    /// and with both exhausted it is the one that reopens last.
    #[gtest]
    fn an_exhausted_window_binds_whichever_slot_it_is_in() {
        // A five-hour window nearly full and a weekly one used up: waiting for the five-hour
        // window to reopen would only be refused again.
        assert_that!(
            selected_reset_timestamp(Some(window(90.0, Some(10))), Some(window(100.0, Some(500)))),
            some(eq(500))
        );
        assert_that!(
            selected_reset_timestamp(
                Some(window(100.0, Some(10))),
                Some(window(100.0, Some(500)))
            ),
            some(eq(500))
        );
        assert_that!(
            selected_reset_timestamp(Some(window(100.0, Some(10))), Some(window(40.0, Some(500)))),
            some(eq(10))
        );
    }

    /// With nothing exhausted, the window closest to closing is the one to expect, and a window
    /// with no reopening time cannot be reported.
    #[gtest]
    fn otherwise_the_fullest_window_with_a_reopening_time_binds() {
        assert_that!(
            selected_reset_timestamp(Some(window(20.0, Some(10))), Some(window(60.0, Some(500)))),
            some(eq(500))
        );
        assert_that!(
            selected_reset_timestamp(Some(window(20.0, Some(10))), Some(window(100.0, None))),
            some(eq(10))
        );
        assert_that!(selected_reset_timestamp(None, None), none());
    }
}
