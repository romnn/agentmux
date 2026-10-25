//! What agentmux writes about a consultation and its turns, and how it reads and writes it.
//!
//! Every record is written whole, so a reader never sees a partial one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::delegate::{Billing, Delegate, ModelId, SessionRef};
use crate::launch::{ExitStatus, Launched};
use crate::transcript::{Outcome, ResumeAttempt, turn_index_of_number, turn_number};

use super::SettledBy;
use super::auto_resume::AutoResumePlan;
use super::error::RunError;
use super::id::RunId;
use super::layout::TurnDir;
use super::retention::Retention;

/// How a turn's delegate was launched.
///
/// Recorded per turn, because the mode is decided at each launch: a consultation begun from a
/// store that runs every Codex turn over `exec` can be followed up by one that drives it over the
/// app-server, and the thread carries over either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    /// The delegate CLI was run with the question on stdin: `claude --print` or `codex exec`.
    ///
    /// Nothing can reach the turn once it has started.
    #[default]
    #[serde(rename = "exec")]
    DirectCli,
    /// The turn is driven over `codex app-server` by [`crate::appserver`], and can be steered.
    AppServer,
}

/// What agentmux recorded about a consultation when it began.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    /// The consultation's id.
    pub run_id: RunId,
    /// Who is being consulted.
    pub delegate: Delegate,
    /// Where the delegate is reading from.
    pub cwd: PathBuf,
    /// How long the consultation is kept.
    pub retention: Retention,
    /// When `start` was called.
    pub created_at: DateTime<Utc>,
    /// Extra environment the caller asked for, reapplied on every follow-up.
    ///
    /// Recorded so a later turn runs under the same conditions as the first; a follow-up that
    /// quietly dropped it would answer from a differently configured delegate.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The model the caller named, when the machine's configuration rewrote it to another.
    ///
    /// `delegate` names what ran, as it does for the account.
    /// The identifier that was asked for is kept beside it rather than thrown away, because a
    /// caller that pinned a model and reads a different one back has to be able to tell a rule of
    /// its own machine from agentmux ignoring the pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewritten_from: Option<ModelId>,
    /// The pricing the consultation may run on, decided when it started and asked for again by
    /// every follow-up.
    ///
    /// Pinned like the account, so a follow-up never runs on a pricing its first turn was not
    /// allowed: a checkout that selected the account, or a caller that asked for a subscription,
    /// keeps the consultation on one even after the files change.
    /// `None` only for a consultation recorded before agentmux kept it, whose account decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
    /// Whether every Codex turn runs with `codex exec`, because the caller asked or the store that
    /// started the consultation drives none, kept for the follow-ups.
    #[serde(
        rename = "codex_exec",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub force_codex_exec: bool,
}

/// The turn's delegate, recorded once it is running.
///
/// Under a supervisor this names the delegate the supervisor started, not the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct LaunchRecord {
    pub(super) launched: Launched,
    pub(super) started_at: DateTime<Utc>,
}

/// The `agentmux supervise` process responsible for a turn, recorded by the caller that launched
/// it before the run's lock is released.
///
/// Its own identity, apart from the delegate's in [`LaunchRecord`], because the two processes
/// outlive each other in both directions: a supervisor publishes after its delegate has gone,
/// and a delegate keeps running when its supervisor crashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SupervisorRecord {
    /// The launch this supervisor was started for, which it checks against its plan before
    /// starting anything.
    pub(super) launch_id: String,
    pub(super) launched: Launched,
    pub(super) started_at: DateTime<Utc>,
    pub(super) phase: SupervisorPhase,
}

/// How far a supervisor got towards starting its delegate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SupervisorPhase {
    /// Recorded by its caller; nothing has been spawned.
    Prepared,
    /// The supervisor is about to spawn the delegate, under the run's lock.
    ///
    /// A supervisor found gone in this phase with no [`LaunchRecord`] may have started a
    /// delegate it never recorded, so its turn is neither settled nor replaced.
    Spawning,
}

/// What a supervisor is to run, written into the turn directory before it is launched.
///
/// Carries no environment: the supervisor is launched with the delegate's environment, already
/// resolved, and hands it on unchanged, so no credential is ever written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SupervisorPlan {
    /// The run store's root, absolute.
    pub(super) store_root: PathBuf,
    /// The consultation.
    pub(super) run_id: RunId,
    /// The turn's zero-based position, written as the one-based number its directory shows.
    #[serde(
        rename = "turn",
        serialize_with = "turn_number",
        deserialize_with = "turn_index_of_number"
    )]
    pub(super) turn_index: u32,
    /// The launch this plan belongs to, which the [`SupervisorRecord`] must name.
    pub(super) launch_id: String,
    /// What to run as the delegate.
    pub(super) producer: ProducerCommand,
    /// How the turn is resumed automatically should its model be overloaded, or `None` when it
    /// is not.
    pub(super) auto_resume: Option<AutoResumePlan>,
}

/// The delegate a supervisor starts, as the initiating process prepared it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ProducerCommand {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) billing: Billing,
}

/// How a turn's child ended, as far as agentmux could tell.
///
/// Written once and never rewritten: whichever caller observes the departure first settles the
/// turn, and a second observation adopts that record rather than deriving its own.
/// Everything the settlement is derived from is in the record, so a later fold reads the record
/// and not the files it was derived from, which a stray descendant of the child may still be
/// writing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ExitRecord {
    /// The child was reaped, or observed gone.
    Exited {
        /// Absent when agentmux was not the child's parent by the time it ended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ExitStatus>,
        /// How much of the capture existed once the child had gone.
        events_len: u64,
        /// The end of what the child wrote to stderr, which is all the evidence a silent exit
        /// leaves.
        #[serde(default)]
        stderr_tail: String,
    },
    /// A caller stopped it.
    Cancelled {
        /// How much of the capture existed once the child had gone, or as it stood when the
        /// child could not be made to go.
        events_len: u64,
    },
    /// Its supervisor never started it: the spawn failed, or the supervisor left before trying.
    NotStarted {
        /// Why, in the words of whoever found out.
        reason: String,
    },
}

impl ExitRecord {
    /// How much of the capture is part of the consultation; nothing past it is read.
    pub(super) fn events_len(&self) -> u64 {
        match self {
            Self::Exited { events_len, .. } | Self::Cancelled { events_len } => *events_len,
            Self::NotStarted { .. } => 0,
        }
    }
}

/// One turn's directory with the records that decide whether it is a turn at all, and whether
/// it has settled.
#[derive(Debug)]
pub(super) struct TurnFiles {
    pub(super) index: u32,
    pub(super) dir: TurnDir,
    pub(super) launch: Option<LaunchRecord>,
    pub(super) exit: Option<ExitRecord>,
    pub(super) supervisor: Option<SupervisorRecord>,
    pub(super) settled: Option<SettledRecord>,
}

impl TurnFiles {
    /// Whether the directory is a turn rather than a claim.
    pub(super) fn is_recorded(&self) -> bool {
        self.launch.is_some() || self.exit.is_some() || self.supervisor.is_some()
    }

    /// Whether the turn's supervisor left while spawning its delegate without recording it, so a
    /// delegate may be running that nothing names.
    ///
    /// Read under the run's lock, this needs no process lookup: a supervisor spawns and records
    /// its delegate under one hold of the lock, so one seen in that phase with no launch record
    /// has died there.
    /// Only a record that the spawn failed resolves it; a cancellation cannot, because nothing it
    /// could stop is known.
    pub(super) fn start_is_uncertain(&self) -> bool {
        self.launch.is_none()
            && !matches!(self.exit, Some(ExitRecord::NotStarted { .. }))
            && self
                .supervisor
                .as_ref()
                .is_some_and(|supervisor| supervisor.phase == SupervisorPhase::Spawning)
    }
}

/// `settled.json` as it is written: the commitment that a turn's transcript is published and
/// final.
///
/// Read through [`super::SettledTurn`], which validates it against the files it vouches for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct SettledRecord {
    /// Exactly [`SettledRecord::VERSION`].
    pub(super) version: u32,
    /// The turn's zero-based position, written as the one-based number its directory shows.
    #[serde(
        rename = "turn",
        serialize_with = "turn_number",
        deserialize_with = "turn_index_of_number"
    )]
    pub(super) turn_index: u32,
    pub(super) outcome: Outcome,
    pub(super) settled_by: SettledBy,
    /// The complete-record prefix of `events.jsonl` the turn was folded from.
    pub(super) events_len: u64,
    /// The length of `recovered.md` the turn rendered, or `None` when it recovered no reply.
    pub(super) recovered_bytes: Option<u64>,
    /// Where the turn's section starts in `transcript.md`.
    pub(super) transcript_start: u64,
    /// Where it ends, exclusive: everything before this offset is published and never changes.
    pub(super) transcript_end: u64,
}

impl SettledRecord {
    /// The one schema version this agentmux reads and writes.
    pub(super) const VERSION: u32 = 2;
}

/// A turn whose end is committed: its transcript is published through
/// [`SettledTurn::transcript_end`], and nothing before that offset will change.
///
/// Read from the turn's `settled.json`, which is written last, after the transcript it describes,
/// and only once the turn's delegate has gone or a cancellation froze it.
/// Built only by validating that record against the files it vouches for, so a value of this type
/// never names a running turn or bytes that are not on disk.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SettledTurn {
    run_id: RunId,
    #[serde(rename = "turn", serialize_with = "turn_number")]
    turn_index: u32,
    outcome: Outcome,
    settled_by: SettledBy,
    transcript_start: u64,
    transcript_end: u64,
    transcript_path: PathBuf,
    #[serde(rename = "settled_path")]
    marker_path: PathBuf,
}

impl SettledTurn {
    /// Check a marker against the inputs it froze, and keep it if it holds.
    ///
    /// `transcript_path` is the consultation's published transcript, which is a cache a fold
    /// writes again whenever it is missing, so its length is checked where a page of it is read,
    /// in [`SettledTurn::page`], rather than here.
    pub(super) fn validate(
        run_id: &RunId,
        dir: &TurnDir,
        index: u32,
        transcript_path: &Path,
        record: &SettledRecord,
    ) -> Result<Self, RunError> {
        let marker_path = dir.settled();
        let invalid = |reason: String| RunError::InvalidSettlement {
            path: marker_path.clone(),
            reason,
        };
        if record.version != SettledRecord::VERSION {
            return Err(invalid(format!(
                "it has version {}, and this agentmux reads only version {}",
                record.version,
                SettledRecord::VERSION
            )));
        }
        if record.turn_index != index {
            return Err(invalid(format!(
                "it names turn {}, but it is in the directory of turn {}",
                record.turn_index.saturating_add(1),
                index.saturating_add(1)
            )));
        }
        if !record.outcome.is_terminal() {
            return Err(invalid("its outcome is not terminal".to_owned()));
        }
        if record.transcript_start > record.transcript_end {
            return Err(invalid(format!(
                "its transcript range {}..{} is reversed",
                record.transcript_start, record.transcript_end
            )));
        }
        // Each length is a prefix of an input the marker froze, so the file it measures must hold
        // at least that much; a shorter one was truncated or replaced after the commitment.
        let committed = [
            (dir.events(), record.events_len),
            (dir.recovered(), record.recovered_bytes.unwrap_or(0)),
        ];
        for (path, committed_len) in committed {
            if committed_len == 0 {
                continue;
            }
            let on_disk = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
            if on_disk < committed_len {
                return Err(invalid(format!(
                    "it commits {committed_len} bytes of {}, which holds {on_disk}",
                    path.display()
                )));
            }
        }
        Ok(Self {
            run_id: run_id.clone(),
            turn_index: index,
            outcome: record.outcome.clone(),
            settled_by: record.settled_by,
            transcript_start: record.transcript_start,
            transcript_end: record.transcript_end,
            transcript_path: transcript_path.to_path_buf(),
            marker_path,
        })
    }

    /// The consultation.
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// The turn's zero-based position among the consultation's turns.
    #[must_use]
    pub(crate) fn turn_index(&self) -> u32 {
        self.turn_index
    }

    /// The turn's one-based number, as the transcript's headings, its directory and every front
    /// end show it.
    #[must_use]
    pub fn turn_number(&self) -> u32 {
        self.turn_index.saturating_add(1)
    }

    /// How the turn ended: completed, failed or cancelled, never running.
    #[must_use]
    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }

    /// What decided the outcome.
    #[must_use]
    pub fn settled_by(&self) -> SettledBy {
        self.settled_by
    }

    /// Where the turn's own section starts in the published transcript, in bytes.
    #[must_use]
    pub fn transcript_start(&self) -> u64 {
        self.transcript_start
    }

    /// Where a reader of this turn starts: the top for the first turn, which carries the
    /// consultation's header, and the turn's own section for a later one, as a follow-up's reply
    /// carries that turn alone.
    #[must_use]
    pub fn section_offset(&self) -> u64 {
        if self.turn_index == 0 {
            0
        } else {
            self.transcript_start
        }
    }

    /// Where the published transcript through this turn ends, exclusive, in bytes.
    ///
    /// A follow-up appends after this offset; nothing before it ever changes.
    #[must_use]
    pub fn transcript_end(&self) -> u64 {
        self.transcript_end
    }

    /// The consultation's published transcript.
    #[must_use]
    pub fn transcript_path(&self) -> &Path {
        &self.transcript_path
    }

    /// The turn's `settled.json`.
    #[must_use]
    pub fn marker_path(&self) -> &Path {
        &self.marker_path
    }
}

/// What was tried, recorded before the spawn so a failed launch still leaves evidence.
///
/// Environment *keys* only.
/// The values are credentials.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct InvocationRecord {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) env_keys: Vec<String>,
    pub(super) cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resumed_from: Option<SessionRef>,
    /// Where the delegate CLI saves its sessions, when this process could not write there as the
    /// turn was launched, which means the child could not either.
    ///
    /// Probed at every launch, because each turn inherits the confinement of whoever launched it:
    /// a consultation begun through an unconfined server can be continued from a sandboxed shell.
    /// A store that becomes writable later still holds nothing to resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) unsaved_session_store: Option<PathBuf>,
    /// How the delegate was run, which decides whether the turn can be steered.
    #[serde(rename = "mode", default)]
    pub(super) launch_mode: LaunchMode,
    /// The pricing the delegate was launched on; absent from a record older than agentmux
    /// keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) billing: Option<Billing>,
    /// The vendor's configuration directory, as the environment the child got resolves it:
    /// where Codex writes the rollout a turn's usage window is recovered from.
    /// Recorded rather than re-derived, because it depends on the host environment, the
    /// configuration and the request's environment as they were at launch, any of which can
    /// change afterwards; absent from a record older than agentmux keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) config_dir: Option<PathBuf>,
    /// The delegate CLI's version, as it printed it, for telling apart captures that different
    /// releases wrote; absent when the CLI could not say, and from a record older than agentmux
    /// keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cli_version: Option<String>,
    /// The version of agentmux that launched the turn, whose own capture format changes between
    /// releases too; absent from a record older than agentmux keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agentmux_version: Option<String>,
    /// The delegate this turn ran as: its isolation and permissions as held to the machine
    /// file's limits, which the consultation's next follow-up starts from.
    ///
    /// Kept with the turn rather than in the consultation's record, whose header must not change
    /// once rendered; absent from a record older than agentmux keeping it, whose turn ran as the
    /// consultation's record says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) delegate: Option<Delegate>,
    /// Which automatic resume this turn is, along an unbroken run of turns that failed
    /// overloaded, when agentmux rather than a caller sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) auto_resume: Option<ResumeAttempt>,
}

/// Write a record, replacing any there before, so that a reader never sees a partial one.
pub(super) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RunError> {
    let text = json_text(path, value)?;
    crate::atomic_file::write_whole(path, text.as_bytes())
        .map_err(RunError::io(format!("writing {}", path.display())))
}

/// Write a record once, and hand back whatever record is there afterwards.
///
/// When another caller got there first, its record is returned rather than overwritten, so two
/// observers of one departure settle on one story.
/// [`crate::atomic_file::create_once`] says how: on a filesystem without hard links a reader can
/// glimpse a partial record — loudly, as one that cannot be parsed, never quietly as one that is
/// not there.
pub(super) fn write_json_once<T: Serialize + for<'de> Deserialize<'de>>(
    path: &Path,
    value: &T,
) -> Result<T, RunError> {
    let text = json_text(path, value)?;
    crate::atomic_file::create_once(path, text.as_bytes())
        .map_err(RunError::io(format!("writing {}", path.display())))?;
    read_json(path)?.ok_or_else(|| RunError::Io {
        context: format!("reading back {}", path.display()),
        source: std::io::Error::from(std::io::ErrorKind::NotFound),
    })
}

/// A record's whole text, as it is written to `path`.
fn json_text<T: Serialize>(path: &Path, value: &T) -> Result<String, RunError> {
    serde_json::to_string_pretty(value).map_err(|source| RunError::Io {
        context: format!("serialising {}", path.display()),
        source: std::io::Error::other(source),
    })
}

/// Read the complete, newline-terminated records of a capture file.
///
/// A live child is appending, so the file routinely ends mid-line and often mid-character.
/// Decoding the whole file as UTF-8 fails on that trailing fragment, and treating the failure as
/// "no events" would drop every complete message before it: the rendered transcript would shrink,
/// an outstanding cursor would be clamped backwards, and the messages would reappear once the
/// child finished the character.
/// Cutting at the last newline first means only complete records are decoded, and a complete
/// record from either CLI is valid UTF-8.
///
/// A file that is not there yet is empty; a file that cannot be read is an error, because reading
/// it as empty would make the transcript shrink with the failure and grow back after it.
///
/// With a `limit`, only that many bytes of the file are considered at all: it is the length a
/// settled turn's record froze, and it is applied to the raw bytes so that it means the same
/// thing whether or not the file decodes cleanly.
/// Returns the records and how many raw bytes they span, which is the length a record freezes:
/// a later read with that limit yields exactly these records, whatever was appended since.
pub(super) fn read_complete_records(
    path: &Path,
    limit: Option<u64>,
) -> Result<(String, u64), RunError> {
    let mut bytes = read_if_present(path, std::fs::read)?.unwrap_or_default();
    if let Some(limit) = limit {
        bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    bytes.truncate(end);
    let text = String::from_utf8(bytes)
        .unwrap_or_else(|invalid| String::from_utf8_lossy(invalid.as_bytes()).into_owned());
    Ok((text, u64::try_from(end).unwrap_or(u64::MAX)))
}

/// Read a record agentmux wrote, telling a record that is not there from one that cannot be read.
///
/// Records are written whole, so a file that is there is complete; one that cannot be parsed
/// was damaged or edited, and reading it as absent would change the shape of the turn it
/// belongs to.
pub(super) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, RunError> {
    let Some(text) = read_if_present(path, std::fs::read_to_string)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| RunError::CorruptRecord {
            path: path.to_owned(),
            source,
        })
}

/// The last `max_bytes` of a text file, or nothing for a file that is not there.
///
/// A file that cannot be read is an error rather than empty: this is the evidence a settlement
/// is recorded from, and recording "wrote nothing" over a transient failure would freeze the
/// wrong story.
pub(super) fn tail_of_file(path: &Path, max_bytes: usize) -> Result<String, RunError> {
    let text = read_if_present(path, std::fs::read)?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    if text.len() <= max_bytes {
        return Ok(text);
    }
    let start = text.floor_char_boundary(text.len().saturating_sub(max_bytes));
    Ok(text.get(start..).unwrap_or_default().to_owned())
}

/// What `read` makes of the file at `path`, or `None` for a file that is not there.
///
/// Any other failure is an error, so a file that cannot be read is never taken for one that is
/// not there yet.
pub(super) fn read_if_present<'path, T>(
    path: &'path Path,
    read: impl FnOnce(&'path Path) -> std::io::Result<T>,
) -> Result<Option<T>, RunError> {
    match read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(RunError::Io {
            context: format!("reading {}", path.display()),
            source,
        }),
    }
}
