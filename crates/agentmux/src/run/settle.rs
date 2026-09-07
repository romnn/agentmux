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
//! the child has gone, so nothing that could be written afterwards is read.
//! Whatever record settles a turn, the turn is rendered from exactly the capture length that
//! record froze, whichever caller wrote it.

use std::time::SystemTime;

use super::{
    ExitRecord, InvocationRecord, Meta, RunError, RunState, RunStore, TurnDir, TurnFiles,
    read_complete_records, read_json, tail_of_file, write_json, write_json_once,
};
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
impl RunStore {
    fn recover_from_last_message(
        &self,
        meta: &Meta,
        files: &TurnFiles,
        turn: &mut Turn,
        may_recover: bool,
    ) -> Result<(), RunError> {
        let dir = &files.dir;
        if !turn.outcome.is_terminal() || turn.messages.iter().any(Message::is_report_content) {
            return Ok(());
        }
        if let Some(kept) = read_text(&dir.recovered())? {
            turn.recovered = Some(kept);
            return Ok(());
        }
        if !may_recover {
            return Ok(());
        }
        let Some(text) = read_text(&dir.last_message())?.filter(|text| !text.trim().is_empty())
        else {
            return Ok(());
        };
        // Kept before it is rendered, so what a reader is handed cannot later go missing, and
        // kept under the lock so that a turn claimed in the meantime is seen: a fold that
        // decided this turn was the newest before that claim must not recover into it now.
        // Text that cannot be kept is not rendered either; a note the turn never gains keeps
        // every page a prefix of the next, where a note that came and went would not.
        let _lock = self.lock(&meta.run_id)?;
        if let Some(kept) = read_text(&dir.recovered())? {
            turn.recovered = Some(kept);
            return Ok(());
        }
        if self
            .turn_dir(&meta.run_id, files.index.saturating_add(1))
            .root
            .exists()
        {
            return Ok(());
        }
        let temp = dir
            .recovered()
            .with_extension(format!("md.{}.tmp", uuid::Uuid::new_v4().simple()));
        let kept =
            std::fs::write(&temp, &text).and_then(|()| std::fs::rename(&temp, dir.recovered()));
        if let Err(error) = kept {
            let _ = std::fs::remove_file(&temp);
            tracing::warn!(%error, path = %dir.recovered().display(), "cannot keep a recovered reply");
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
fn note_continuity(
    dir: &TurnDir,
    opened: Option<&SessionRef>,
    turn: &mut Turn,
) -> Result<(), RunError> {
    // The first turn never resumes anything, so its invocation record is not worth reading.
    if turn.index == 0 {
        return Ok(());
    }
    let Some(asked_for) =
        read_json::<InvocationRecord>(&dir.invocation())?.and_then(|record| record.resumed_from)
    else {
        return Ok(());
    };
    match opened {
        Some(opened) if *opened != asked_for => turn.broke_continuity = true,
        None if turn.outcome.is_terminal() => {
            turn.unrecognised.record("session.<not announced>");
        }
        _ => {}
    }
    Ok(())
}

/// The newest modification time among a turn's capture files.
pub(super) fn last_write(dir: &TurnDir) -> Option<SystemTime> {
    [dir.events(), dir.stderr()]
        .iter()
        .filter_map(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
        .max()
}

impl RunStore {
    /// The recovered usage window, searched for once and remembered.
    ///
    /// The absence of one is cached too.
    /// A finished turn's rollout does not gain rate limits later, so a fruitless search repeated
    /// on every `status` would walk the vendor's whole session tree for nothing.
    fn cached_codex_rate_limit(
        &self,
        dir: &TurnDir,
        meta: &Meta,
        thread_id: &str,
    ) -> Option<RateLimit> {
        if let Ok(Some(cached)) = read_json::<Option<RateLimit>>(&dir.rate_limit()) {
            return cached;
        }
        let found = self.codex_rate_limit(meta, thread_id);
        let _ = write_json(&dir.rate_limit(), &found);
        found
    }

    /// The usage window Codex recorded while running one turn.
    ///
    /// Its rollout files live under `CODEX_HOME`, which is found the way the delegate found it:
    /// from the environment the same account resolves to.
    fn codex_rate_limit(&self, meta: &Meta, thread_id: &str) -> Option<RateLimit> {
        let config = crate::config::Config::load(&self.host_env, &meta.cwd).ok()?;
        let env = meta.delegate.identity_env(&self.host_env, &config).ok()?;
        let codex_home = env
            .get("CODEX_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| crate::config::home_dir(&self.host_env).map(|home| home.join(".codex")))?;

        let limits = crate::quota::codex_rollout_rate_limits(&codex_home, thread_id)?;
        // Only the account-wide bucket is recorded here; the per-model ones are not in a rollout.
        let primary = limits.get("primary")?;
        let resets_at = primary
            .get("resets_at")
            .and_then(serde_json::Value::as_i64)
            .and_then(chrono::DateTime::from_timestamp_secs)?;
        Some(RateLimit {
            resets_at,
            window: limits
                .get("limit_id")
                .and_then(|id| id.as_str())
                .map(ToOwned::to_owned),
        })
    }

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
        let mut newest_turn_index = 0;
        let mut last_activity: Option<SystemTime> = None;

        let turns = self.turn_files(&meta.run_id)?;
        let newest_index = turns.last().map(|files| files.index);
        for files in turns {
            let folded = self.fold_turn(meta, &files, Some(files.index) == newest_index)?;
            newest_turn_index = files.index;
            if folded.session.is_some() {
                session = folded.session;
            }
            if let Some(seen) = folded.last_activity {
                last_activity = Some(last_activity.map_or(seen, |known| known.max(seen)));
            }
            transcript.turns.push(folded.turn);
        }

        Ok(RunState {
            transcript,
            session,
            newest_turn_index,
            last_activity,
        })
    }

    /// Fold one turn from its own files, settling it if its child has gone.
    ///
    /// Reading a turn never depends on another turn's capture, which is what lets a caller that
    /// only needs the newest turn's state fold that one alone.
    /// `may_recover` says whether this fold may keep a reply the CLI wrote out late, which only
    /// the newest turn may gain and which takes the run lock to do — so a caller that already
    /// holds the lock, or only needs the outcome, passes `false`.
    pub(super) fn fold_turn(
        &self,
        meta: &Meta,
        files: &TurnFiles,
        may_recover: bool,
    ) -> Result<FoldedTurn, RunError> {
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
            .and_then(|record| self.launcher.reap(record.launched));

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
                self.launcher.liveness(record.launched)
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
            Self::settle_unfinished(dir, files.exit.as_ref(), departure)?
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
        } = fold;
        if let Some(record) = &record {
            turn.outcome = Self::outcome_of(dir, record, vendor);
        }

        note_continuity(dir, turn_session.as_ref(), &mut turn)?;

        // Codex says nothing about usage on the stream agentmux captures, but records it in the
        // session rollout it writes anyway.
        // Reading that afterwards is the only way a Codex consultation reports a usage window at
        // all, and it costs neither a process nor a token.
        // Only once the turn is over: a running turn's rollout is still being written, and the
        // search is too expensive to repeat on every fold.
        if vendor == Vendor::Codex
            && turn.rate_limit.is_none()
            && turn.outcome.is_terminal()
            && let Some(thread) = turn_session.as_ref()
        {
            turn.rate_limit = self.cached_codex_rate_limit(dir, meta, thread.as_str());
        }

        self.recover_from_last_message(meta, files, &mut turn, may_recover)?;

        Ok(FoldedTurn {
            turn,
            session: turn_session,
            last_activity: last_write(dir),
        })
    }

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
