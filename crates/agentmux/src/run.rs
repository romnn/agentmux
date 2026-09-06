//! Run directories, ids, lifecycle, retention and resume state.
//!
//! Changes when storage changes.
//!
//! # The shape on disk
//!
//! ```text
//! runs/<run_id>/
//!   meta.json                  delegate, question, cwd, retention, created_at
//!   transcript.md              the rendered transcript, a cache of the fold below
//!   turns/0000/
//!     question.md              what was asked; also the child's stdin
//!     invocation.json          what was run, written before the spawn
//!     events.jsonl             the delegate's raw event stream, exactly as emitted
//!     stderr.log               the delegate's stderr
//!     last-message.md          Codex's closing message, written by the CLI itself
//!     launch.json              pid and process group, so a later call can check on it
//!     exit.json                exit status, written when the child is reaped
//! ```
//!
//! One directory per turn, because each turn is a separate child process.
//! Nothing ever appends to another turn's capture file, so the "the child owns the file
//! descriptor" invariant holds without coordination.
//!
//! # Where truth lives
//!
//! `events.jsonl` is the state; everything else is derived.
//! `transcript.md` is a cache, rewritten from the fold on every read, so an agentmux that died
//! mid-review loses nothing.
//! Terminal state is decided in this order:
//!
//! 1. a terminal event in the stream — authoritative, because it survives everything else;
//! 2. `exit.json`, when the child was reaped;
//! 3. process liveness, as a last resort.

mod settle;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::delegate::{Delegate, DelegateError, SessionRef, TurnPlan};
use crate::launch::{ExitStatus, LaunchError, LaunchSpec, Launched, Launcher};
use crate::transcript::{FailureKind, Outcome, Transcript, UnrecognisedEvents, Usage, render_turn};

/// Anything that can go wrong while running a consultation.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// No consultation with that id.
    #[error("no consultation with id {0}; call `list` to see recent ids")]
    NotFound(RunId),
    /// A run id was not of the accepted shape.
    #[error("{0}")]
    BadRunId(#[from] RunIdError),
    /// A delegate argument was rejected.
    #[error(transparent)]
    Delegate(#[from] DelegateError),
    /// A child could not be launched.
    #[error(transparent)]
    Launch(#[from] LaunchError),
    /// A run directory could not be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What agentmux was doing.
        context: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// A run's own metadata could not be parsed, so the run is unusable.
    #[error("consultation {run_id} has unreadable metadata: {source}")]
    Corrupt {
        /// Which run.
        run_id: RunId,
        /// The parse failure.
        #[source]
        source: serde_json::Error,
    },
    /// A follow-up was asked of a consultation that cannot take one.
    #[error("{0}")]
    NotResumable(#[from] NotResumable),
    /// Another caller claimed this turn first.
    #[error(
        "consultation {run_id} already has a turn {index} in flight, started by another caller. \
         Only one follow-up can be outstanding at a time; wait for it with `result`."
    )]
    TurnAlreadyClaimed {
        /// Which consultation.
        run_id: RunId,
        /// The turn index that was already claimed.
        index: u32,
    },

    /// The state directory could not be located.
    #[error("cannot locate a state directory; set AGENTMUX_STATE_DIR")]
    NoStateDir,
}

impl RunError {
    fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        move |source| Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// A run id was not of the accepted shape.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid consultation id {value:?}: ids are 1-64 characters of [0-9A-Za-z-]")]
pub struct RunIdError {
    /// What was supplied.
    pub value: String,
}

/// Why a consultation cannot take a follow-up.
///
/// Kept apart as variants because the caller does something different about each: wait, fix the
/// delegate arguments, or start over.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NotResumable {
    /// The newest turn has not finished, so there is nothing to continue yet.
    #[error(
        "consultation {run_id} is still running, so it has nothing to continue yet. Wait for it \
         with `result` and its `wait_seconds`, then ask the follow-up."
    )]
    StillRunning {
        /// Which consultation.
        run_id: RunId,
    },

    /// The delegate never announced a session, so there is nothing to reopen.
    #[error(
        "consultation {run_id} has no session to reopen: the delegate never announced one, which \
         usually means it failed before it started. Read its transcript for why, then start a new \
         consultation."
    )]
    NoSession {
        /// Which consultation.
        run_id: RunId,
    },

    /// The consultation failed without the delegate ever saying anything.
    #[error(
        "consultation {run_id} failed ({}) without the delegate saying anything, so there is no \
         conversation to continue. Fix what the failure names, then start a fresh consultation.",
        .kind.label()
    )]
    Failed {
        /// Which consultation.
        run_id: RunId,
        /// How it failed, which is what the caller has to fix.
        kind: FailureKind,
    },

    /// The consultation was cancelled, so the delegate was stopped part-way through a turn.
    #[error(
        "consultation {run_id} was cancelled, so its delegate was stopped part-way through a \
         turn. Everything collected before the cancellation is still readable with `result`, but \
         continuing that turn would resume a delegate mid-thought. Start a new consultation."
    )]
    Cancelled {
        /// Which consultation.
        run_id: RunId,
    },
}

/// The identity of a consultation.
///
/// A run id arrives from a language model and is used to open a directory.
/// Parsing is the only way to build one, so path traversal through a tool argument is not a bug
/// that was fixed; it is a state that cannot be constructed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId(String);

impl RunId {
    /// Longest id accepted.
    pub const MAX_LEN: usize = 64;

    /// Validate an id supplied by a caller.
    ///
    /// # Errors
    ///
    /// Returns [`RunIdError`] for anything outside `[0-9A-Za-z-]{1,64}`, which rejects `..`, `/`,
    /// `\`, absolute paths, NUL and every other control character by construction.
    pub fn parse(value: &str) -> Result<Self, RunIdError> {
        let ok = !value.is_empty()
            && value.len() <= Self::MAX_LEN
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(RunIdError {
                value: value.to_owned(),
            })
        }
    }

    /// Mint a fresh id: sortable by creation time, with a random tail against collisions.
    #[must_use]
    pub fn generate() -> Self {
        let stamp = Utc::now().format("%Y%m%dT%H%M%S");
        let tail = uuid::Uuid::new_v4().simple().to_string();
        Self(format!("{stamp}-{}", tail.get(..8).unwrap_or("00000000")))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunId {
    type Error = RunIdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<RunId> for String {
    fn from(value: RunId) -> Self {
        value.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How long a finished consultation is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// Swept once it is older than [`RunStore::TTL`].
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

/// What `start` was asked for.
#[derive(Debug, Clone)]
pub struct StartRequest {
    /// Who to consult, and on what terms.
    pub delegate: Delegate,
    /// The question, self-contained: the delegate starts with no conversation context.
    pub question: String,
    /// The directory the delegate reads the project from.
    pub cwd: PathBuf,
    /// How long to keep the consultation after it finishes.
    pub retention: Retention,
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
}

/// Where one turn's files live, and what agentmux knows about its child.
#[derive(Debug, Clone)]
struct TurnDir {
    root: PathBuf,
}

impl TurnDir {
    fn question(&self) -> PathBuf {
        self.root.join("question.md")
    }
    fn events(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }
    fn stderr(&self) -> PathBuf {
        self.root.join("stderr.log")
    }
    fn last_message(&self) -> PathBuf {
        self.root.join("last-message.md")
    }
    fn launch(&self) -> PathBuf {
        self.root.join("launch.json")
    }
    fn invocation(&self) -> PathBuf {
        self.root.join("invocation.json")
    }
    fn exit(&self) -> PathBuf {
        self.root.join("exit.json")
    }
}

/// A consultation as a caller sees it.
#[derive(Debug, Clone)]
pub struct RunStatus {
    /// The consultation's id.
    pub run_id: RunId,
    /// Who is being consulted.
    pub delegate: Delegate,
    /// The directory the delegate is reading the project from.
    ///
    /// Reported because getting it wrong is silent: a delegate pointed at the wrong worktree
    /// returns a confident review of code that was never under review.
    pub cwd: PathBuf,
    /// `running`, `completed`, `failed` or `cancelled`.
    pub outcome: Outcome,
    /// When `start` was called.
    pub created_at: DateTime<Utc>,
    /// How long the consultation has been going, or how long it took.
    pub elapsed: Duration,
    /// How many turns the consultation has.
    pub turns: u32,
    /// How many messages the transcript holds across every turn.
    ///
    /// Counts hook injections and agentmux's own notes alongside the delegate's replies, because
    /// a caller polling this wants to know that *something* arrived.
    pub message_count: usize,
    /// Token accounting so far.
    pub usage: Usage,
    /// Reported dollar cost so far, where the vendor reports one.
    pub cost_usd: Option<f64>,
    /// Event types the parser met but does not model.
    pub unrecognised: UnrecognisedEvents,
    /// Whether a hook reopened a finished turn, which makes the last message untrustworthy.
    pub reopened_by_hook: bool,
    /// An earlier turn that failed, when the newest turn is not the one that failed.
    ///
    /// Without this a follow-up onto a failed turn would report the consultation as `completed`
    /// and leave the failure visible only deep inside the transcript body.
    pub earlier_failure: Option<(u32, FailureKind)>,
    /// Whether any turn asked to continue a session and silently opened a new one instead.
    pub broke_continuity: bool,
    /// Where the rendered transcript is written.
    pub transcript_path: PathBuf,
    /// Where the delegate's raw event stream for the newest turn is written.
    pub events_path: PathBuf,
    /// Where the delegate's stderr for the newest turn is written.
    pub stderr_path: PathBuf,
    /// The size of the rendered transcript, which is also the cursor a `tail` would end at.
    pub transcript_bytes: u64,
    /// Whether a follow-up is possible: the delegate announced a resumable session.
    pub resumable: bool,
}

impl RunStatus {
    /// Whether nothing more can arrive.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.outcome.is_terminal()
    }
}

/// One line about a consultation, for `list`.
#[derive(Debug, Clone)]
pub struct RunSummary {
    /// The consultation's id.
    pub run_id: RunId,
    /// A one-line description of the delegate.
    pub delegate: String,
    /// `running`, `completed`, `failed` or `cancelled`.
    pub state: &'static str,
    /// When `start` was called.
    pub created_at: DateTime<Utc>,
    /// The first line of the question, truncated.
    pub question: String,
}

/// A slice of the rendered transcript.
#[derive(Debug, Clone)]
pub struct TranscriptPage {
    /// The text, from `offset`.
    pub text: String,
    /// Where this slice started.
    pub offset: u64,
    /// Where the next slice starts.
    /// Pass it back as the next `offset` or `cursor`.
    pub next_offset: u64,
    /// Total size of the rendered transcript right now.
    pub total_bytes: u64,
    /// Whether `next_offset` has reached the end of what exists so far.
    pub at_end: bool,
}

/// The run store: one directory per consultation under a state directory.
pub struct RunStore {
    root: PathBuf,
    launcher: Arc<dyn Launcher>,
    host_env: BTreeMap<String, String>,
}

impl std::fmt::Debug for RunStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl RunStore {
    /// How long a `ttl` consultation survives after it was created.
    pub const TTL: Duration = Duration::from_hours(24);

    /// Environment variable that overrides where run directories live.
    pub const STATE_DIR_ENV: &'static str = "AGENTMUX_STATE_DIR";

    /// First interval between polls, so a question answered in a second returns in about a second.
    const MIN_POLL: Duration = Duration::from_millis(250);

    /// Longest interval a wait backs off to.
    const MAX_POLL: Duration = Duration::from_secs(2);

    /// Open a store rooted at `root`, creating it if needed.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] when the directory cannot be created.
    pub fn open(
        root: impl Into<PathBuf>,
        launcher: Arc<dyn Launcher>,
        host_env: BTreeMap<String, String>,
    ) -> Result<Self, RunError> {
        let root = root.into();
        create_private_dir(&root)?;
        Ok(Self {
            root,
            launcher,
            host_env,
        })
    }

    /// The default state directory: `$AGENTMUX_STATE_DIR`, else the platform's own.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NoStateDir`] when the platform has no home directory.
    pub fn default_root(host_env: &BTreeMap<String, String>) -> Result<PathBuf, RunError> {
        if let Some(explicit) = host_env.get(Self::STATE_DIR_ENV).filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(explicit));
        }
        let dirs = directories::ProjectDirs::from("com", "romnn", "agentmux")
            .ok_or(RunError::NoStateDir)?;
        // `state_dir` exists only on Linux; `data_dir` is the right home elsewhere.
        Ok(dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_dir())
            .to_path_buf())
    }

    /// Where run directories live.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    fn run_dir(&self, run_id: &RunId) -> PathBuf {
        self.runs_dir().join(run_id.as_str())
    }

    fn turn_dir(&self, run_id: &RunId, index: u32) -> TurnDir {
        TurnDir {
            root: self
                .run_dir(run_id)
                .join("turns")
                .join(format!("{index:04}")),
        }
    }

    fn transcript_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join("transcript.md")
    }

    /// Begin a consultation.
    ///
    /// Returns as soon as the child is running, not when it answers.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the run directory cannot be created or the delegate cannot be
    /// launched.
    pub fn start(&self, request: &StartRequest) -> Result<RunStatus, RunError> {
        let run_id = RunId::generate();
        let dir = self.run_dir(&run_id);
        create_private_dir(&dir)?;

        let meta = Meta {
            run_id: run_id.clone(),
            delegate: request.delegate.clone(),
            cwd: request.cwd.clone(),
            retention: request.retention,
            created_at: Utc::now(),
        };
        write_json(&dir.join("meta.json"), &meta)?;

        self.launch_turn(&meta, 0, &request.question, None)?;
        self.status(&run_id)
    }

    /// Continue a consultation with another question, in the delegate's own session.
    ///
    /// The run id does not change: a consultation is a conversation.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, and
    /// [`RunError::NotResumable`] when the previous turn is still running, was cancelled, or never
    /// announced a session id.
    pub fn follow_up(&self, run_id: &RunId, question: &str) -> Result<RunStatus, RunError> {
        let meta = self.meta(run_id)?;
        let state = self.fold_run(&meta);

        let outcome = state.transcript.outcome();
        if !outcome.is_terminal() {
            return Err(NotResumable::StillRunning {
                run_id: run_id.clone(),
            }
            .into());
        }
        if outcome == Outcome::Cancelled {
            return Err(NotResumable::Cancelled {
                run_id: run_id.clone(),
            }
            .into());
        }
        // Whether there is a conversation to continue is a question about what the delegate
        // said, not about how the newest turn ended.
        if !state.transcript.has_delegate_content()
            && let Outcome::Failed { kind, .. } = outcome
        {
            return Err(NotResumable::Failed {
                run_id: run_id.clone(),
                kind,
            }
            .into());
        }
        let Some(session) = state.session else {
            return Err(NotResumable::NoSession {
                run_id: run_id.clone(),
            }
            .into());
        };

        let next = u32::try_from(state.transcript.turns.len()).unwrap_or(u32::MAX);
        self.launch_turn(&meta, next, question, Some(&session))?;
        self.status(run_id)
    }

    fn launch_turn(
        &self,
        meta: &Meta,
        index: u32,
        question: &str,
        resume: Option<&SessionRef>,
    ) -> Result<(), RunError> {
        let turn = self.turn_dir(&meta.run_id, index);
        // `create_dir` rather than `create_dir_all`: creating the directory *is* the claim on this
        // turn index, and it has to fail if someone else already made it.
        // Two callers that both fold a one-turn consultation both choose index 1, and with a
        // forgiving create they would both launch a paid child into the same capture file.
        reserve_turn_dir(&turn.root).map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                RunError::TurnAlreadyClaimed {
                    run_id: meta.run_id.clone(),
                    index,
                }
            } else {
                RunError::Io {
                    context: format!("reserving turn {index}"),
                    source,
                }
            }
        })?;
        std::fs::write(turn.question(), question)
            .map_err(RunError::io(format!("writing turn {index} question")))?;

        let last_message = turn.last_message();
        let plan = TurnPlan {
            question_path: &turn.question(),
            last_message_path: &last_message,
            resume,
        };
        let invocation = meta.delegate.invocation(&plan, &self.host_env)?;

        // Recorded before the spawn so a launch that fails still leaves evidence of what was
        // tried, and so a later fold can tell whether a resume opened the session it asked for.
        write_json(
            &turn.invocation(),
            &InvocationRecord {
                program: invocation.program.to_owned(),
                args: invocation.args.clone(),
                env_keys: invocation.env.keys().cloned().collect(),
                cwd: meta.cwd.clone(),
                resumed_from: resume.cloned(),
            },
        )?;

        // Under `agentmux mcp` this is the only visible record of what was spawned, and the
        // argument vector is the thing most worth seeing when a delegate misbehaves.
        // Environment values are deliberately absent: they carry credentials.
        tracing::info!(
            run_id = %meta.run_id,
            turn = index,
            program = invocation.program,
            args = ?invocation.args,
            cwd = %meta.cwd.display(),
            resumed = resume.is_some(),
            "launching delegate",
        );

        let launched = self.launcher.launch(&LaunchSpec {
            invocation: &invocation,
            cwd: &meta.cwd,
            question: &turn.question(),
            events: &turn.events(),
            stderr: &turn.stderr(),
        })?;
        write_json(
            &turn.launch(),
            &LaunchRecord {
                launched,
                started_at: Utc::now(),
            },
        )?;
        tracing::debug!(run_id = %meta.run_id, turn = index, pid = launched.pid, "delegate running");
        Ok(())
    }

    /// Everything currently known about a consultation.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation.
    pub fn status(&self, run_id: &RunId) -> Result<RunStatus, RunError> {
        let meta = self.meta(run_id)?;
        let state = self.fold_run(&meta);
        let rendered = self.write_transcript(&meta, &state)?;
        let newest = state.newest_turn_index;
        let turn = self.turn_dir(run_id, newest);
        let outcome = state.transcript.outcome();

        Ok(RunStatus {
            run_id: run_id.clone(),
            delegate: meta.delegate.clone(),
            cwd: meta.cwd.clone(),
            elapsed: elapsed_since(meta.created_at, &outcome, state.last_activity),
            created_at: meta.created_at,
            turns: u32::try_from(state.transcript.turns.len()).unwrap_or(u32::MAX),
            message_count: state.transcript.messages().count(),
            usage: state.transcript.usage(),
            cost_usd: state.transcript.cost_usd(),
            unrecognised: state.transcript.unrecognised(),
            reopened_by_hook: state.transcript.was_reopened_by_hook(),
            earlier_failure: state.transcript.earlier_failure(),
            broke_continuity: state
                .transcript
                .turns
                .iter()
                .any(|turn| turn.broke_continuity),
            transcript_path: self.transcript_path(run_id),
            events_path: turn.events(),
            stderr_path: turn.stderr(),
            transcript_bytes: rendered,
            // Advertised only when a follow-up would actually work, because `next_steps` turns
            // this into a recommendation and a caller that takes it pays for the turn.
            // The test is evidence: a session was announced, the delegate said something worth
            // continuing, and the newest turn is finished and was not interrupted mid-thought.
            resumable: state.session.is_some()
                && outcome.is_terminal()
                && outcome != Outcome::Cancelled
                && state.transcript.has_delegate_content(),
            outcome,
        })
    }

    /// A slice of the rendered transcript, starting at `offset`.
    ///
    /// The rendered transcript is append-only with respect to the events folded into it, so an
    /// offset stays valid for the life of the consultation.
    /// `tail` and `result` share this coordinate system: a caller can poll with one and page with
    /// the other.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation.
    pub fn read_transcript(
        &self,
        run_id: &RunId,
        offset: u64,
        max_bytes: usize,
    ) -> Result<TranscriptPage, RunError> {
        let meta = self.meta(run_id)?;
        let state = self.fold_run(&meta);
        let full = Self::render(&meta, &state);
        self.write_transcript_text(&meta, &full)?;

        let total = u64::try_from(full.len()).unwrap_or(u64::MAX);
        let start = usize::try_from(offset.min(total)).unwrap_or(usize::MAX);
        // Never split a UTF-8 character: walk back to a boundary, then forward for the end.
        let start = floor_char_boundary(&full, start);
        let mut end = floor_char_boundary(&full, start.saturating_add(max_bytes).min(full.len()));
        if end <= start && start < full.len() {
            // `max_bytes` was smaller than the next character — every turn heading contains an em
            // dash, so this is reachable with an accepted page size.
            // Returning an empty page would leave the cursor where it was and a polling caller
            // would spin forever, so emit one whole character instead.
            end = ceil_char_boundary(&full, start.saturating_add(1));
        }
        let text = full.get(start..end).unwrap_or_default().to_owned();
        let next = u64::try_from(end).unwrap_or(total);

        Ok(TranscriptPage {
            text,
            offset: u64::try_from(start).unwrap_or(offset),
            next_offset: next,
            total_bytes: total,
            at_end: next >= total,
        })
    }

    /// Stop a running consultation.
    ///
    /// Everything collected so far is kept.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation.
    pub fn cancel(&self, run_id: &RunId) -> Result<RunStatus, RunError> {
        let meta = self.meta(run_id)?;
        let state = self.fold_run(&meta);
        if !state.transcript.outcome().is_terminal()
            && let Some(record) = state.newest_launch
        {
            self.launcher.terminate(record.launched);
            let turn = self.turn_dir(run_id, state.newest_turn_index);
            write_json(
                &turn.exit(),
                &ExitRecord {
                    status: None,
                    cancelled: true,
                },
            )?;
        }
        self.status(run_id)
    }

    /// Recent consultations, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] when the run directory cannot be listed.
    pub fn list(&self, limit: usize) -> Result<Vec<RunSummary>, RunError> {
        // Ordered by the recorded creation time, not by the id.
        // Ids lead with a second-precision timestamp, so two consultations started in the same
        // second — the common case, since fanning one question out across both vendors starts them
        // back to back — would otherwise be ordered by their random suffix.
        let mut metas: Vec<Meta> = self
            .run_ids()?
            .into_iter()
            // A half-written or hand-mangled run directory must not break `list`, which is the
            // tool a caller reaches for precisely when something has gone wrong.
            .filter_map(|run_id| self.meta(&run_id).ok())
            .collect();
        metas.sort_unstable_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.run_id.cmp(&a.run_id))
        });
        metas.truncate(limit);

        Ok(metas
            .into_iter()
            .map(|meta| {
                let state = self.fold_run(&meta);
                RunSummary {
                    run_id: meta.run_id,
                    delegate: meta.delegate.summary(),
                    state: state.transcript.outcome().state(),
                    created_at: meta.created_at,
                    question: state
                        .transcript
                        .turns
                        .first()
                        .map(|turn| first_line(&turn.question, 120))
                        .unwrap_or_default(),
                }
            })
            .collect())
    }

    /// Delete consultations past their retention.
    ///
    /// Runs once at server start.
    /// There is no timer and no daemon.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] when the run directory cannot be listed.
    pub fn sweep(&self) -> Result<usize, RunError> {
        let mut removed = 0;
        for run_id in self.run_ids()? {
            let Ok(meta) = self.meta(&run_id) else {
                continue;
            };
            if meta.retention == Retention::UntilReleased {
                continue;
            }
            let age = Utc::now().signed_duration_since(meta.created_at);
            let Ok(age) = age.to_std() else { continue };
            if age < Self::TTL {
                continue;
            }
            // A run whose child is still alive is never swept, however old: a long review that
            // outlived its TTL is exactly the one worth keeping.
            if !self.fold_run(&meta).transcript.outcome().is_terminal() {
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
    /// Returns [`RunError::NotFound`] when there is no such consultation.
    pub fn remove(&self, run_id: &RunId) -> Result<(), RunError> {
        let dir = self.run_dir(run_id);
        if !dir.is_dir() {
            return Err(RunError::NotFound(run_id.clone()));
        }
        std::fs::remove_dir_all(&dir).map_err(RunError::io(format!("removing {run_id}")))
    }

    /// What agentmux recorded when the consultation began.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, or
    /// [`RunError::Corrupt`] when its metadata cannot be parsed.
    pub fn meta(&self, run_id: &RunId) -> Result<Meta, RunError> {
        let path = self.run_dir(run_id).join("meta.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(RunError::NotFound(run_id.clone()));
            }
            Err(source) => {
                return Err(RunError::Io {
                    context: format!("reading {}", path.display()),
                    source,
                });
            }
        };
        serde_json::from_str(&text).map_err(|source| RunError::Corrupt {
            run_id: run_id.clone(),
            source,
        })
    }

    /// Poll until a consultation reaches a terminal state, or until `timeout` elapses.
    ///
    /// Never fails for taking too long: on timeout it returns the status as it stands, still
    /// running, and the consultation carries on.
    /// Both hosts cap how long a tool call may block — Codex at sixty seconds by default — so
    /// callers pass a cap well under theirs.
    ///
    /// The wait re-folds the capture files but renders and writes nothing until it is done, and
    /// backs its interval off towards a two-second ceiling.
    /// A quick answer still returns almost immediately; a ninety-minute review is not re-rendered
    /// several thousand times on its way there.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation.
    pub async fn wait_until_terminal(
        &self,
        run_id: &RunId,
        timeout: Duration,
    ) -> Result<RunStatus, RunError> {
        let meta = self.meta(run_id)?;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut interval = Self::MIN_POLL;
        while !self.fold_run(&meta).transcript.outcome().is_terminal() {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep(interval.min(deadline - now)).await;
            interval = (interval * 2).min(Self::MAX_POLL);
        }
        self.status(run_id)
    }

    fn run_ids(&self) -> Result<Vec<RunId>, RunError> {
        let dir = self.runs_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(RunError::Io {
                    context: format!("listing {}", dir.display()),
                    source,
                });
            }
        };
        Ok(entries
            .flatten()
            .filter_map(|entry| RunId::parse(entry.file_name().to_str()?).ok())
            .collect())
    }
}

/// Everything the fold over a run's capture files established.
#[derive(Debug)]
pub(super) struct RunState {
    pub(super) transcript: Transcript,
    pub(super) session: Option<SessionRef>,
    pub(super) newest_turn_index: u32,
    pub(super) newest_launch: Option<LaunchRecord>,
    pub(super) last_activity: Option<SystemTime>,
}

impl RunStore {
    /// Render the whole consultation as Markdown.
    ///
    /// The result is **append-only for the life of the consultation**, which is the contract
    /// [`RunStore::read_transcript`] pages by byte offset and `tail` hands back as a cursor.
    /// Two things make that true, and both are easy to break:
    ///
    /// - this header is built only from values fixed when the consultation was created, so
    ///   nothing here may ever carry state, elapsed time or a message count;
    /// - each turn's body grows only at its end, which
    ///   [`crate::transcript::render_turn`] documents and enforces.
    ///
    /// Adding a mutable field to the header would silently invalidate every outstanding cursor.
    fn render(meta: &Meta, state: &RunState) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = write!(
            out,
            "# Consultation {}\n\n\
             - delegate: {}\n\
             - working directory: `{}`\n\
             - started: {}\n",
            meta.run_id,
            meta.delegate.summary(),
            meta.cwd.display(),
            meta.created_at.to_rfc3339(),
        );
        for turn in &state.transcript.turns {
            out.push_str(&render_turn(turn));
        }
        out
    }

    fn write_transcript(&self, meta: &Meta, state: &RunState) -> Result<u64, RunError> {
        let text = Self::render(meta, state);
        self.write_transcript_text(meta, &text)?;
        Ok(u64::try_from(text.len()).unwrap_or(u64::MAX))
    }

    /// Write the rendered transcript, replacing it atomically.
    ///
    /// A reader outside agentmux — a human with an editor, another agent with `cat` — must
    /// never see a half-written file.
    fn write_transcript_text(&self, meta: &Meta, text: &str) -> Result<(), RunError> {
        let path = self.transcript_path(&meta.run_id);

        // Publication has to be ordered, not merely atomic.
        // Two callers can fold at different instants and finish in the other order, and the
        // slower one would then rename its older, shorter render over the newer one, so the
        // advertised path would lose already-published content until some later read repaired
        // it.
        // The render is append-only, so a snapshot that is a prefix of what is already published
        // is simply older, and skipping it is the correct ordering rule.
        if let Ok(published) = std::fs::read_to_string(&path)
            && published.len() > text.len()
            && published.starts_with(text)
        {
            return Ok(());
        }
        // A unique temp name per write.
        // Two tool calls against the same consultation — a `tail` polling while a `status` lands —
        // would otherwise share one temp path and interleave their writes into it before either
        // rename.
        let temp = path.with_extension(format!("md.{}.tmp", uuid::Uuid::new_v4().simple()));
        let write = || -> Result<(), RunError> {
            std::fs::write(&temp, text).map_err(RunError::io("writing the rendered transcript"))?;
            std::fs::rename(&temp, &path).map_err(RunError::io("replacing the rendered transcript"))
        };
        let result = write();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct LaunchRecord {
    launched: Launched,
    started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ExitRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<ExitStatus>,
    #[serde(default)]
    cancelled: bool,
}

/// What was tried, recorded before the spawn so a failed launch still leaves evidence.
///
/// Environment *keys* only.
/// The values are credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct InvocationRecord {
    program: String,
    args: Vec<String>,
    env_keys: Vec<String>,
    cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    resumed_from: Option<SessionRef>,
}

/// Claim a turn directory, failing if another caller already claimed it.
fn reserve_turn_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(path)?;
    restrict(path);
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), RunError> {
    std::fs::create_dir_all(path).map_err(RunError::io(format!("creating {}", path.display())))?;
    restrict(path);
    Ok(())
}

/// Keep a run directory readable only by its owner.
///
/// Transcripts contain whatever the caller asked about, which is often a private codebase.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RunError> {
    let text = serde_json::to_string_pretty(value).map_err(|source| RunError::Io {
        context: "serialising run metadata".to_owned(),
        source: std::io::Error::other(source),
    })?;
    std::fs::write(path, text).map_err(RunError::io(format!("writing {}", path.display())))
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
fn read_complete_records(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    String::from_utf8_lossy(bytes.get(..end).unwrap_or_default()).into_owned()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let cut: String = line.chars().take(max).collect();
    format!("{cut}…")
}

fn tail_of_file(path: &Path, max_bytes: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if text.len() <= max_bytes {
        return text;
    }
    let start = floor_char_boundary(&text, text.len().saturating_sub(max_bytes));
    text.get(start..).unwrap_or_default().to_owned()
}

/// Walk `index` back to the nearest UTF-8 character boundary.
///
/// Slicing a rendered transcript by byte offset would otherwise panic on a multi-byte character,
/// and transcripts routinely contain them.
/// Walk `index` forward to the nearest UTF-8 character boundary.
///
/// Used only to guarantee forward progress when a page size is smaller than one character.
fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index = index.saturating_add(1);
    }
    index
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// How long a consultation has been going, or how long it took.
///
/// A finished consultation's clock stops at the last write to a capture file rather than at now,
/// so a run collected an hour after it finished does not report a one-hour duration.
fn elapsed_since(
    created_at: DateTime<Utc>,
    outcome: &Outcome,
    last_activity: Option<SystemTime>,
) -> Duration {
    let end = if outcome.is_terminal() {
        last_activity.map_or_else(Utc::now, DateTime::<Utc>::from)
    } else {
        Utc::now()
    };
    end.signed_duration_since(created_at)
        .to_std()
        .unwrap_or_default()
}
