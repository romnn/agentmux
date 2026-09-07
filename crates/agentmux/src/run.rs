//! Run directories, ids, lifecycle, retention and resume state.
//!
//! Changes when storage changes.
//!
//! # The shape on disk
//!
//! ```text
//! runs/<run_id>.lock           held while a turn is claimed, a cancellation is in progress, the
//!                              rendered transcript is replaced, or the run is removed
//! runs/<run_id>/
//!   meta.json                  delegate, question, cwd, retention, created_at
//!   transcript.md              the rendered transcript, a cache of the fold below
//!   turns/0000/
//!     question.md              what was asked; also the child's stdin
//!     invocation.json          what was run, written before the spawn
//!     events.jsonl             the delegate's raw event stream, exactly as emitted
//!     stderr.log               the delegate's stderr
//!     last-message.md          Codex's closing message, written by the CLI itself
//!     recovered.md             that message, once agentmux has rendered it in place of a reply
//!                              the stream lost
//!     launch.json              pid, start time and process group, so a later call can check on it
//!     cancelling               present once a cancellation was asked for
//!     exit.json                how the child ended and how much it had written, written once and
//!                              never rewritten
//! ```
//!
//! A turn directory counts as a turn once it holds a launch or an exit record.
//! Before that it is a claim: the directory is created under the run's lock as the claim on its
//! index, and the launch is recorded before the lock is released, so a claim with neither record
//! can only have been abandoned by a caller that died in between.
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
//! 2. `exit.json`, when the child was reaped, observed gone, or cancelled;
//! 3. process liveness, as a last resort.
//!
//! # What the lock is for
//!
//! Several agentmux processes may address one consultation — an MCP server and a terminal, or a
//! server restarted underneath a `tail`.
//! Reading needs no coordination: every fold rebuilds from the files.
//! The four things that change a run's shape do, because each is a check followed by a write and
//! another process can act between the two: claiming a turn, cancelling, publishing the rendered
//! transcript, and deleting the run.
//! The lock is an advisory file lock the operating system releases when its holder dies, so a
//! crashed holder leaves nothing to recover from.

mod settle;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::delegate::{Delegate, DelegateError, Isolation, SessionRef, TurnPlan, Vendor};
use crate::launch::{ExitStatus, LaunchError, LaunchSpec, Launched, Launcher, Liveness};
use crate::transcript::{
    FailureKind, Outcome, RateLimit, Transcript, UnrecognisedEvents, Usage, render_turn,
};

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
    /// The machine's account configuration could not be loaded.
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    /// agentmux is already running as somebody's delegate.
    ///
    /// Reachable only when a delegate inherited its account's MCP servers and agentmux is among
    /// them, which is exactly the case isolation would otherwise have prevented.
    #[error(
        "this agentmux is itself running inside a delegate, so it will not launch another one. A \
         delegate that inherits its account's settings also inherits its MCP servers; answer the \
         question you were asked instead."
    )]
    Recursive,
    /// The operator forbade this vendor on this server.
    ///
    /// A same-vendor delegate is a subagent the calling harness already spawns natively and
    /// supervises itself, so an operator registering agentmux inside one harness denies that
    /// harness's own vendor here.
    #[error(
        "this agentmux server does not launch `{vendor}` delegates: it was started with `--deny \
         {vendor}` because the harness it serves is itself `{vendor}` and spawns same-vendor \
         subagents natively. Consult the other vendor here, and use your own harness's subagents \
         for `{vendor}`."
    )]
    VendorDenied {
        /// The vendor that was asked for.
        vendor: Vendor,
    },
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
    /// A record agentmux wrote itself cannot be parsed.
    ///
    /// Records are written whole, so this is a file that was edited by hand or damaged, and
    /// reading it as absent would make the turn it belongs to change shape.
    #[error("the record at {path} cannot be parsed: {source}")]
    CorruptRecord {
        /// The file.
        path: PathBuf,
        /// The parse failure.
        #[source]
        source: serde_json::Error,
    },
    /// A follow-up was asked of a consultation that cannot take one.
    #[error("{0}")]
    NotResumable(#[from] NotResumable),
    /// Another caller claimed this turn first.
    #[error(
        "consultation {run_id} already has a turn {index}, started by another caller. Only one \
         follow-up can be outstanding at a time; read the consultation again with `result`."
    )]
    TurnAlreadyClaimed {
        /// Which consultation.
        run_id: RunId,
        /// The turn index that was already claimed.
        index: u32,
    },
    /// The operation needs the consultation finished, and it is not.
    #[error(
        "consultation {run_id} is still running. Wait for it with `result`, or `cancel` it first."
    )]
    StillRunning {
        /// Which consultation.
        run_id: RunId,
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

    /// The consultation ended without the delegate ever saying anything.
    #[error(
        "consultation {run_id} ended ({state}) without the delegate saying anything, so there is \
         no conversation to continue. Read its transcript for why, then start a fresh \
         consultation."
    )]
    NothingSaid {
        /// Which consultation.
        run_id: RunId,
        /// How it ended: `completed` or `failed`.
        state: &'static str,
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
    /// Swept once [`RunStore::TTL`] has passed since its delegate last wrote anything.
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
    ///
    /// Made absolute before it is recorded, because the record outlives the process that made it
    /// and a later turn must read the same directory.
    pub cwd: PathBuf,
    /// How long to keep the consultation after its delegate last wrote anything.
    pub retention: Retention,
    /// Extra environment for the delegate, applied over the machine and account layers.
    pub env: BTreeMap<String, String>,
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
}

/// Where one turn's files live, and what agentmux knows about its child.
#[derive(Debug, Clone)]
pub(super) struct TurnDir {
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
    /// The reply recovered from the CLI's own output file, kept once it has been rendered.
    fn recovered(&self) -> PathBuf {
        self.root.join("recovered.md")
    }
    fn launch(&self) -> PathBuf {
        self.root.join("launch.json")
    }
    fn invocation(&self) -> PathBuf {
        self.root.join("invocation.json")
    }
    /// Where the usage window recovered from a vendor's own session record is kept.
    ///
    /// Cached because finding it means searching the vendor's session tree, and a fold runs on
    /// every `status` and every page of a `tail`.
    fn rate_limit(&self) -> PathBuf {
        self.root.join("rate-limit.json")
    }
    fn exit(&self) -> PathBuf {
        self.root.join("exit.json")
    }
    /// Present from the moment a cancellation was asked for.
    ///
    /// Whoever then observes the child gone — this caller, or a concurrent `status` — records
    /// the departure as a cancellation rather than as a death, so the two cannot disagree.
    fn cancelling(&self) -> PathBuf {
        self.root.join("cancelling")
    }
}

/// A consultation as a caller sees it.
///
/// Serialises as the machine-readable form both front ends print, so a field added here reaches
/// `--json` without anyone remembering to copy it.
#[derive(Debug, Clone, Serialize)]
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
    /// `running`, `completed`, `failed` or `cancelled`, with the detail of a failure.
    pub outcome: Outcome,
    /// When `start` was called.
    pub created_at: DateTime<Utc>,
    /// How long the consultation has been going, or how long it took.
    #[serde(rename = "elapsed_seconds", serialize_with = "whole_seconds")]
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
    /// What it means that a hook reopened a finished turn, when one did.
    ///
    /// Decided here rather than by each front end, because the same event means opposite things
    /// depending on what the run loaded and every reader has to say the same thing about it.
    pub hook_reopening: Option<HookReopening>,
    /// An earlier turn that failed, when the newest turn is not the one that failed.
    ///
    /// Without this a follow-up onto a failed turn would report the consultation as `completed`
    /// and leave the failure visible only deep inside the transcript body.
    pub earlier_failure: Option<(u32, FailureKind)>,
    /// What each account of this vendor had left, when the consultation was rate limited.
    ///
    /// Empty for every other outcome: probing costs a process, and the answer is only actionable
    /// when the caller has just been refused.
    pub quota: Vec<crate::quota::AccountQuota>,
    /// The most recent usage window the delegate reported, when it reported one.
    ///
    /// Carried out of the transcript because the right response to a rate limit depends on how
    /// long the window has left, and a caller should not have to read the transcript body to find
    /// a timestamp the stream already gave up.
    pub rate_limit: Option<RateLimit>,
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

fn whole_seconds<S: serde::Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(duration.as_secs())
}

impl RunStatus {
    /// Whether nothing more can arrive.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.outcome.is_terminal()
    }
}

/// What it means that a hook reopened a finished turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookReopening {
    /// The run inherited the account's settings, so a hook is a documented consequence of what
    /// the caller asked for.
    Expected,
    /// The run loaded no settings, so no hook should have been able to reach it, and the whole
    /// transcript is suspect.
    Unexpected,
}

impl HookReopening {
    /// What a reopening means for a run with this isolation, when one happened.
    fn of(reopened: bool, isolation: Option<Isolation>) -> Option<Self> {
        if !reopened {
            return None;
        }
        Some(match isolation {
            Some(Isolation::Inherit) => Self::Expected,
            // `None` means the run predates isolation being recorded, so the safe reading is the
            // alarming one: it is written out rather than wildcarded so a third mode cannot land
            // here by default.
            None | Some(Isolation::Isolated) => Self::Unexpected,
        })
    }
}

/// One line about a consultation, for `list`.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    /// The consultation's id.
    pub run_id: RunId,
    /// A one-line description of the delegate.
    pub delegate: String,
    /// How the consultation stands.
    pub outcome: Outcome,
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
    probe: Arc<dyn crate::quota::QuotaProbe>,
    host_env: BTreeMap<String, String>,
    /// Vendors this store will not launch, whatever a caller asks for.
    denied: BTreeSet<Vendor>,
    /// Whether this process runs inside a delegate, decided once: neither witness can change
    /// while the process lives.
    inside_delegate: OnceLock<bool>,
}

impl std::fmt::Debug for RunStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl RunStore {
    /// How long a `ttl` consultation survives after its delegate last wrote anything.
    pub const TTL: Duration = Duration::from_hours(24);

    /// Environment variable that overrides where run directories live.
    pub const STATE_DIR_ENV: &'static str = "AGENTMUX_STATE_DIR";

    /// First interval between polls, so a question answered in a second returns in about a second.
    const MIN_POLL: Duration = Duration::from_millis(250);

    /// Longest interval a wait backs off to.
    const MAX_POLL: Duration = Duration::from_secs(2);

    /// How long a cancelled child is given to leave after being asked.
    ///
    /// Both CLIs exit on `SIGTERM` well inside this; what needs the time is a tool subprocess
    /// they are waiting on.
    const TERMINATE_GRACE: Duration = Duration::from_secs(3);

    /// How long a killed child is given to actually disappear.
    const KILL_GRACE: Duration = Duration::from_secs(2);

    /// How often a cancellation checks whether the child has gone.
    const STOP_POLL: Duration = Duration::from_millis(100);

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
            // Asking nothing by default, so embedding this library never spawns a process the
            // embedder did not ask for; the binary installs a real probe in `main`.
            probe: Arc::new(crate::quota::DisabledProbe),
            host_env,
            // Every vendor reachable until an operator says otherwise: a terminal denies none.
            denied: BTreeSet::new(),
            inside_delegate: OnceLock::new(),
        })
    }

    /// The environment this store resolves configuration and delegates against.
    ///
    /// Exposed so that a caller building something environment-dependent, such as the MCP
    /// server's instructions, resolves against the same snapshot a consultation will rather than
    /// against whatever the process happens to hold.
    #[must_use]
    pub fn host_env(&self) -> &BTreeMap<String, String> {
        &self.host_env
    }

    /// Install the probe used to ask accounts what they have left.
    #[must_use]
    pub fn with_quota_probe(mut self, probe: Arc<dyn crate::quota::QuotaProbe>) -> Self {
        self.probe = probe;
        self
    }

    /// Refuse to launch delegates of these vendors, whatever a caller asks for.
    ///
    /// `agentmux mcp --deny <vendor>` is the only thing that sets this, so a terminal keeps both
    /// vendors while a server registered inside a harness gives up the one that harness already
    /// spawns natively.
    /// Set by argument and never by environment, like isolation, so what a server offers is fixed
    /// by its registration rather than by whatever environment the host started it in.
    #[must_use]
    pub fn with_denied_vendors(mut self, denied: impl IntoIterator<Item = Vendor>) -> Self {
        self.denied = denied.into_iter().collect();
        self
    }

    /// The vendors this store will not launch.
    ///
    /// Exposed so that a front end can leave a denied vendor out of what it offers, rather than
    /// advertising a choice every call would refuse.
    #[must_use]
    pub fn denied_vendors(&self) -> &BTreeSet<Vendor> {
        &self.denied
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

    fn lock_path(&self, run_id: &RunId) -> PathBuf {
        self.runs_dir().join(format!("{run_id}.lock"))
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

    /// Take the consultation's lock, waiting for whoever holds it.
    ///
    /// The lock file sits beside the run directory rather than inside it, so removing the run
    /// removes nothing another process may be holding, and it is never unlinked: two processes
    /// locking two different inodes under one name would each believe they held the lock.
    /// Released when the returned handle is dropped.
    fn lock(&self, run_id: &RunId) -> Result<File, RunError> {
        let path = self.lock_path(run_id);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| RunError::Io {
                context: format!("opening {}", path.display()),
                source,
            })?;
        file.lock().map_err(|source| RunError::Io {
            context: format!("locking {}", path.display()),
            source,
        })?;
        Ok(file)
    }

    /// Every turn directory of a run, in order, ending at the first index that has none.
    fn turn_dirs(&self, run_id: &RunId) -> impl Iterator<Item = (u32, TurnDir)> {
        (0..u32::MAX)
            .map(move |index| (index, self.turn_dir(run_id, index)))
            .take_while(|(_, dir)| dir.root.is_dir())
    }

    /// Every turn of a run, with its launch and exit records, in order.
    ///
    /// Stops at the first directory holding neither record: that is a claim, not a turn, and
    /// nothing past it can be one either.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] when a record exists but cannot be read, and
    /// [`RunError::CorruptRecord`] when one cannot be parsed.
    fn turn_files(&self, run_id: &RunId) -> Result<Vec<TurnFiles>, RunError> {
        let mut turns = Vec::new();
        for (index, dir) in self.turn_dirs(run_id) {
            let launch = read_json::<LaunchRecord>(&dir.launch())?;
            let exit = read_json::<ExitRecord>(&dir.exit())?;
            if launch.is_none() && exit.is_none() {
                break;
            }
            turns.push(TurnFiles {
                index,
                dir,
                launch,
                exit,
            });
        }
        Ok(turns)
    }

    /// The outcome of a run's newest turn, folded on its own, or `None` for a run with no turn.
    ///
    /// A run's outcome is its newest turn's, and a turn folds from its own files alone, so the
    /// callers that only need to know whether a consultation is over — the poll loop, `list`,
    /// the sweep — do not re-read every settled turn's capture to find out.
    fn newest_turn_outcome(&self, meta: &Meta) -> Result<Option<Outcome>, RunError> {
        let turns = self.turn_files(&meta.run_id)?;
        let Some(newest) = turns.last() else {
            return Ok(None);
        };
        Ok(Some(self.fold_turn(meta, newest, false)?.turn.outcome))
    }

    /// Whether any recorded child of a run is still there.
    ///
    /// A turn is terminal the moment its stream says so, and the child behind it can outlive
    /// that by a little — Codex writes its closing message file afterwards — or, when it was
    /// told to stop and would not, by more.
    /// Deleting a run out from under a live child would leave it writing into unlinked files,
    /// billing, and unreachable.
    fn a_child_is_alive(&self, run_id: &RunId) -> Result<bool, RunError> {
        Ok(self
            .turn_files(run_id)?
            .into_iter()
            .filter(|turn| turn.exit.is_none())
            .filter_map(|turn| turn.launch)
            .any(|record| {
                self.launcher.reap(record.launched).is_none()
                    && self.launcher.liveness(record.launched) == Liveness::Alive
            }))
    }

    /// Whether this process was started by a delegate agentmux launched.
    ///
    /// Two witnesses, because neither reaches everywhere.
    /// The environment marker every delegate carries is passed on by Claude Code and by both
    /// CLIs' shells, but Codex starts its MCP servers with an environment of its own choosing
    /// that does not include it.
    /// The process tree is the other witness: if any ancestor of this process is a delegate this
    /// store launched and that delegate is still running, this process is inside it, however
    /// many shells or shims sit in between.
    ///
    /// Decided once per process.
    /// The marker comes from the captured environment, and an ancestor cannot become a delegate
    /// after the fact, so the answer cannot change while the process lives.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] or [`RunError::CorruptRecord`] when the store cannot be read,
    /// because a guard that read an unreadable record as "no delegate here" would be blind in
    /// exactly the case it exists for.
    pub fn running_inside_a_delegate(&self) -> Result<bool, RunError> {
        if let Some(known) = self.inside_delegate.get() {
            return Ok(*known);
        }
        // Exported-but-empty means unset, as it does for every other agentmux variable: a shell
        // profile that exports it unconditionally must not refuse every launch.
        let inside = self
            .host_env
            .get(crate::delegate::DELEGATE_MARKER)
            .is_some_and(|value| !value.is_empty())
            || self.an_ancestor_is_a_live_delegate()?;
        let _ = self.inside_delegate.set(inside);
        Ok(inside)
    }

    fn an_ancestor_is_a_live_delegate(&self) -> Result<bool, RunError> {
        let ancestors = crate::launch::ancestors();
        if ancestors.is_empty() {
            return Ok(false);
        }
        for run_id in self.run_ids()? {
            let live = self
                .turn_files(&run_id)?
                .into_iter()
                // A pid alone is not a witness, because pids are recycled.
                // The record has to name the process's start time, be of a turn nobody has
                // settled, and describe a process that is still the one recorded.
                .filter(|turn| turn.exit.is_none())
                .filter_map(|turn| turn.launch)
                .filter(|record| record.launched.started.is_some())
                .filter(|record| ancestors.contains(&record.launched.pid))
                .any(|record| self.launcher.liveness(record.launched) == Liveness::Alive);
            if live {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Begin a consultation.
    ///
    /// Returns as soon as the child is running, not when it answers.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the configuration cannot be read, agentmux is itself running
    /// inside a delegate, the run directory cannot be created, or the delegate cannot be launched
    /// — which includes a working directory that is not there.
    /// A failed start leaves no run directory behind.
    pub fn start(&self, request: &StartRequest) -> Result<RunStatus, RunError> {
        // Everything that can be decided without touching the store is decided first, so a
        // refused request leaves no half-made consultation for `list` to report as running.
        self.refuse_to_spawn(request.delegate.vendor())?;
        let cwd = std::path::absolute(&request.cwd)
            .map_err(RunError::io("resolving the working directory"))?;
        let config = crate::config::Config::load(&self.host_env, &cwd)?;

        let (run_id, dir) = self.create_run_dir()?;
        let meta = Meta {
            env: request.env.clone(),
            run_id: run_id.clone(),
            // Resolved now rather than at each launch, so the record names the identity that
            // actually ran and every later turn resumes as the same one.
            // A configuration edited mid-consultation must not silently move a follow-up to
            // another account, whose session it would then fail to resume.
            delegate: Self::pin_defaults(&request.delegate, &config),
            cwd,
            retention: request.retention,
            created_at: Utc::now(),
        };
        let started = write_json(&dir.join("meta.json"), &meta)
            .and_then(|()| self.launch_turn(&meta, 0, &request.question, None, &config));
        if let Err(error) = started {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error);
        }
        self.status(&run_id)
    }

    /// Mint an id and claim its directory.
    ///
    /// The claim is the exclusive create: an id that already exists — eight random hex
    /// characters make that rare, not impossible — is not written over but drawn again.
    fn create_run_dir(&self) -> Result<(RunId, PathBuf), RunError> {
        let runs = self.runs_dir();
        create_private_dir(&runs)?;
        loop {
            let run_id = RunId::generate();
            let dir = runs.join(run_id.as_str());
            match reserve_dir(&dir) {
                Ok(()) => {
                    create_private_dir(&dir.join("turns"))?;
                    return Ok((run_id, dir));
                }
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(RunError::Io {
                        context: format!("creating {}", dir.display()),
                        source,
                    });
                }
            }
        }
    }

    /// Fill in whatever the configuration decides, so the record names what actually ran.
    ///
    /// Both the account and the isolation are resolved once, here, rather than at each launch.
    /// A configuration edited mid-consultation would otherwise move a follow-up onto another
    /// identity — whose session it could not resume — or silently change whether the delegate
    /// loads hooks, in the middle of one transcript.
    ///
    /// Isolation is resolved before the account is pinned, because the answer depends on which
    /// file chose the account, and a pinned alias no longer says.
    /// A default alias the configuration does not define is left unpinned, so the launch that
    /// follows reports it with the file that selected it.
    fn pin_defaults(delegate: &Delegate, config: &crate::config::Config) -> Delegate {
        let isolation = delegate.resolved_isolation(config);
        let mut pinned = delegate.clone();
        if pinned.account().is_none()
            && let Some(alias) = config
                .default_account(pinned.vendor())
                .map(|default| default.alias)
                .filter(|name| config.account(pinned.vendor(), name).is_some())
                .and_then(|name| crate::delegate::AccountAlias::parse(name).ok())
        {
            pinned = pinned.with_account(alias);
        }
        pinned.with_isolation(isolation)
    }

    /// Continue a consultation with another question, in the delegate's own session.
    ///
    /// The run id does not change: a consultation is a conversation.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, and
    /// [`RunError::NotResumable`] when the previous turn is still running, was cancelled, never
    /// announced a session id, or ended without the delegate saying anything.
    pub fn follow_up(&self, run_id: &RunId, question: &str) -> Result<RunStatus, RunError> {
        let mut meta = self.meta(run_id)?;
        let state = self.fold_run(&meta)?;
        let session = Self::resumable_session(run_id, &state)?;

        // A record from before isolation was pinned could only have run isolated.
        if meta.delegate.isolation().is_none() {
            meta.delegate = meta.delegate.clone().with_isolation(Isolation::Isolated);
        }
        // Re-read each turn rather than cached at `start`: a follow-up hours later should see
        // the configuration as it is now, not as it was.
        let config = crate::config::Config::load(&self.host_env, &meta.cwd)?;
        let next = u32::try_from(state.transcript.turns.len()).unwrap_or(u32::MAX);
        self.launch_turn(&meta, next, question, Some(&session), &config)?;
        self.status(run_id)
    }

    /// The session a follow-up would continue, or why there is none.
    ///
    /// The one predicate behind both `follow_up` and `status.resumable`, so what is advertised
    /// and what is accepted cannot drift apart.
    /// Every condition is evidence rather than a rule about failure categories: a session was
    /// announced, the delegate said something worth continuing, and the newest turn is finished
    /// and was not interrupted mid-thought.
    /// A consultation in which the delegate never said anything is not continued whatever its
    /// outcome; one that has, and whose newest turn was refused — a rate limit, say — is merely
    /// paused, and its session is still there to continue.
    /// A reply the CLI wrote out late is only ever recovered into the newest turn, so allowing a
    /// follow-up here cannot let one land above the turn that followed it.
    fn resumable_session(run_id: &RunId, state: &RunState) -> Result<SessionRef, NotResumable> {
        let outcome = state.transcript.outcome();
        if !outcome.is_terminal() {
            return Err(NotResumable::StillRunning {
                run_id: run_id.clone(),
            });
        }
        if outcome == Outcome::Cancelled {
            return Err(NotResumable::Cancelled {
                run_id: run_id.clone(),
            });
        }
        if !state.transcript.has_delegate_content() {
            return Err(NotResumable::NothingSaid {
                run_id: run_id.clone(),
                state: outcome.state(),
            });
        }
        state
            .session
            .clone()
            .ok_or_else(|| NotResumable::NoSession {
                run_id: run_id.clone(),
            })
    }

    /// Refuse a launch for the reasons that belong to this process rather than to any run.
    ///
    /// Both are settled before anything is written, because neither can change while the process
    /// lives: agentmux inside a delegate spawns nothing, and a narrowed store spawns nothing of a
    /// denied vendor.
    fn refuse_to_spawn(&self, vendor: Vendor) -> Result<(), RunError> {
        if self.running_inside_a_delegate()? {
            return Err(RunError::Recursive);
        }
        if self.denied.contains(&vendor) {
            return Err(RunError::VendorDenied { vendor });
        }
        Ok(())
    }

    /// Spawn one turn's delegate.
    ///
    /// The refusals are repeated here rather than left to `start` alone because this is the one
    /// place a child is actually spawned.
    /// `follow_up` and anything added later are then covered by construction rather than by
    /// remembering: a consultation begun from a terminal is not a way to drive a denied vendor
    /// from a server, because its follow-up spawns the same delegate again from here.
    fn launch_turn(
        &self,
        meta: &Meta,
        index: u32,
        question: &str,
        resume: Option<&SessionRef>,
        config: &crate::config::Config,
    ) -> Result<(), RunError> {
        self.refuse_to_spawn(meta.delegate.vendor())?;
        let _lock = self.lock(&meta.run_id)?;
        // The run may have been swept or removed while this caller waited for the lock, and a
        // turn launched into a directory with no metadata would be a child nothing can reach.
        if !self.run_dir(&meta.run_id).join("meta.json").is_file() {
            return Err(RunError::NotFound(meta.run_id.clone()));
        }
        let turn = self.turn_dir(&meta.run_id, index);
        claim_turn(&turn, index, &meta.run_id)?;
        // A claim that does not end in a running child is given back, so a refused follow-up
        // does not leave a phantom failed turn in the transcript.
        let launched = self.launch_into(&turn, meta, index, question, resume, config);
        if launched.is_err() {
            let _ = std::fs::remove_dir_all(&turn.root);
        }
        launched
    }

    /// Everything between claiming a turn directory and recording the running child in it.
    fn launch_into(
        &self,
        turn: &TurnDir,
        meta: &Meta,
        index: u32,
        question: &str,
        resume: Option<&SessionRef>,
        config: &crate::config::Config,
    ) -> Result<(), RunError> {
        std::fs::write(turn.question(), question)
            .map_err(RunError::io(format!("writing turn {index} question")))?;

        let last_message = turn.last_message();
        let plan = TurnPlan {
            question_path: &turn.question(),
            last_message_path: &last_message,
            resume,
            extra_env: &meta.env,
        };
        let mut invocation = meta.delegate.invocation(&plan, &self.host_env, config)?;
        // The store's own root, resolved, rather than whatever the host exported: an agentmux
        // started underneath this delegate has to open the same store to recognise the delegate
        // as its ancestor, and the platform default can differ between the two environments.
        invocation.env.insert(
            Self::STATE_DIR_ENV.to_owned(),
            self.root.to_string_lossy().into_owned(),
        );

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
        let recorded = write_json(
            &turn.launch(),
            &LaunchRecord {
                launched,
                started_at: Utc::now(),
            },
        );
        if let Err(error) = recorded {
            // A child nothing records is a child nothing can ever stop; better to stop it now,
            // while the handle is still warm, than to leave it billing behind a failed write.
            self.launcher.terminate(launched);
            self.launcher.kill(launched);
            return Err(error);
        }
        tracing::debug!(run_id = %meta.run_id, turn = index, pid = launched.pid, "delegate running");
        Ok(())
    }

    /// Everything currently known about a consultation.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation.
    pub fn status(&self, run_id: &RunId) -> Result<RunStatus, RunError> {
        let (status, _) = self.snapshot(run_id)?;
        Ok(status)
    }

    /// The state of a consultation and a page of its transcript, from one reading of the files.
    ///
    /// One fold serves both, so the state and the page describe the same instant: a `tail` cannot
    /// say `running` above a page that already carries the completed footer.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation.
    pub fn view(
        &self,
        run_id: &RunId,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(RunStatus, TranscriptPage), RunError> {
        let (status, full) = self.snapshot(run_id)?;
        Ok((status, page_of(&full, offset, max_bytes)))
    }

    /// One page of the rendered transcript, from the same single fold as [`RunStore::view`].
    ///
    /// Most callers want only the page, and each would otherwise discard the state half of the pair.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation.
    pub fn page(
        &self,
        run_id: &RunId,
        offset: u64,
        max_bytes: usize,
    ) -> Result<TranscriptPage, RunError> {
        let (_, page) = self.view(run_id, offset, max_bytes)?;
        Ok(page)
    }

    /// Fold once, publish the render, and describe the consultation from that one reading.
    ///
    /// Returns the whole render alongside the status, for the caller to page.
    fn snapshot(&self, run_id: &RunId) -> Result<(RunStatus, String), RunError> {
        let meta = self.meta(run_id)?;
        let state = self.fold_run(&meta)?;
        let full = Self::render(&meta, &state);
        self.write_transcript_text(&meta, &full)?;

        let newest = state.newest_turn_index;
        let turn = self.turn_dir(run_id, newest);
        let outcome = state.transcript.outcome();
        let quota = self.quota_for_failure(run_id, &meta, &outcome);

        let status = RunStatus {
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
            hook_reopening: HookReopening::of(
                state.transcript.was_reopened_by_hook(),
                meta.delegate.isolation(),
            ),
            earlier_failure: state.transcript.earlier_failure(),
            quota,
            // The newest window wins: an earlier turn's reading is stale the moment another
            // arrives, and a caller acting on it would wait against a window that already moved.
            rate_limit: state
                .transcript
                .turns
                .iter()
                .rev()
                .find_map(|turn| turn.rate_limit.clone()),
            broke_continuity: state
                .transcript
                .turns
                .iter()
                .any(|turn| turn.broke_continuity),
            transcript_path: self.transcript_path(run_id),
            events_path: turn.events(),
            stderr_path: turn.stderr(),
            transcript_bytes: u64::try_from(full.len()).unwrap_or(u64::MAX),
            // Advertised only when a follow-up would actually work, because `next_steps` turns
            // this into a recommendation and a caller that takes it pays for the turn.
            resumable: Self::resumable_session(run_id, &state).is_ok(),
            outcome,
        };
        Ok((status, full))
    }

    /// The other accounts' figures, when this consultation was refused for a rate limit.
    ///
    /// Probed only on that failure, where the answer is exactly what the caller needs and the
    /// figures cannot be stale in the way that matters: the window just closed.
    /// Written beside the run so repeated `status` calls cost nothing, and so the record of why a
    /// consultation was told to go elsewhere survives with the consultation.
    fn quota_for_failure(
        &self,
        run_id: &RunId,
        meta: &Meta,
        outcome: &Outcome,
    ) -> Vec<crate::quota::AccountQuota> {
        if !matches!(
            outcome,
            Outcome::Failed {
                kind: FailureKind::RateLimited,
                ..
            }
        ) {
            return Vec::new();
        }
        let path = self.run_dir(run_id).join("quota.json");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(cached) = serde_json::from_str(&text)
        {
            return cached;
        }
        let Ok(config) = crate::config::Config::load(&self.host_env, &meta.cwd) else {
            return Vec::new();
        };
        let reported = crate::quota::probe_vendor(
            meta.delegate.vendor(),
            &config,
            &self.host_env,
            self.probe.as_ref(),
        );
        // Best effort: a consultation that cannot cache its quota still reports it.
        let _ = write_json(&path, &reported);
        reported
    }

    /// Ask each configured account what it has left, for the given vendors.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Config`] when the machine's configuration cannot be read.
    pub fn quota(
        &self,
        vendor: Option<Vendor>,
    ) -> Result<Vec<crate::quota::AccountQuota>, RunError> {
        // Discovered from agentmux's own directory: a quota question is about this machine, not
        // about any one consultation.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config = crate::config::Config::load(&self.host_env, &cwd)?;
        let vendors: &[Vendor] = vendor.as_ref().map_or(&Vendor::ALL, std::slice::from_ref);
        Ok(vendors
            .iter()
            .flat_map(|vendor| {
                crate::quota::probe_vendor(*vendor, &config, &self.host_env, self.probe.as_ref())
            })
            .collect())
    }

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
        // Nothing under the lock calls back into anything that takes it: `status` does, and a
        // second lock on the same file from one process would wait on the first for ever.
        let marked = {
            let _lock = self.lock(run_id)?;
            let newest = self.turn_files(run_id)?.into_iter().last();
            match newest {
                None => None,
                Some(newest) => {
                    let cancelling = !self
                        .fold_turn(&meta, &newest, false)?
                        .turn
                        .outcome
                        .is_terminal();
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
        // Outside the lock: stopping can take seconds, and a `status` in the meantime should
        // not wait on it.
        if let Some(record) = newest.launch.as_ref() {
            self.stop(record.launched).await;
        }
        if cancelling {
            // Frozen at the complete records that exist now, which is exactly what a later fold
            // reads back.
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
    async fn stop(&self, launched: Launched) {
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
    async fn wait_for_departure(&self, launched: Launched, grace: Duration) -> bool {
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
            .filter_map(|meta| {
                // The outcome is the newest turn's, and the question is the first turn's own
                // file; neither needs the whole consultation folded.
                let outcome = self
                    .newest_turn_outcome(&meta)
                    .ok()?
                    .unwrap_or(Outcome::Running);
                let question = std::fs::read_to_string(self.turn_dir(&meta.run_id, 0).question())
                    .map(|question| first_line(&question, 120))
                    .unwrap_or_default();
                Some(RunSummary {
                    run_id: meta.run_id,
                    delegate: meta.delegate.summary(),
                    outcome,
                    created_at: meta.created_at,
                    question,
                })
            })
            .collect())
    }

    /// Delete consultations past their retention.
    ///
    /// Runs once at server start.
    /// There is no timer and no daemon.
    ///
    /// A run is removed only under its lock, so a follow-up claimed between the check and the
    /// deletion cannot lose its capture files, and a run with a child still alive is never
    /// swept, however old: a long review that outlived its TTL is exactly the one worth keeping.
    /// A run that never got a turn — a start that failed after the directory was made — is
    /// swept like a finished one.
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
            // The clock runs from the last thing the delegate wrote, not from the start: a
            // consultation that ran for longer than the TTL is not stale the moment it ends.
            // File times alone decide it, so a young run costs a few stats and no fold.
            let last = self
                .turn_dirs(&run_id)
                .filter_map(|(_, dir)| settle::last_write(&dir))
                .max()
                .map_or(meta.created_at, DateTime::<Utc>::from)
                .max(meta.created_at);
            let Ok(age) = Utc::now().signed_duration_since(last).to_std() else {
                continue;
            };
            if age < Self::TTL {
                continue;
            }
            let Ok(_lock) = self.lock(&run_id) else {
                continue;
            };
            let finished = match self.newest_turn_outcome(&meta) {
                Ok(outcome) => outcome.is_none_or(|outcome| outcome.is_terminal()),
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
        let _lock = self.lock(run_id)?;
        let meta = self.meta(run_id)?;
        if self
            .newest_turn_outcome(&meta)?
            .is_some_and(|outcome| !outcome.is_terminal())
            || self.a_child_is_alive(run_id)?
        {
            return Err(RunError::StillRunning {
                run_id: run_id.clone(),
            });
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
    /// Never fails for taking too long: on timeout it simply returns, the consultation carries
    /// on, and the caller's next `status` or `view` says so.
    /// Both hosts cap how long a tool call may block — Codex at sixty seconds by default — so
    /// callers pass a cap well under theirs.
    ///
    /// The wait folds only the newest turn on each poll, renders and writes nothing, and backs
    /// its interval off towards a two-second ceiling.
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
    ) -> Result<(), RunError> {
        let meta = self.meta(run_id)?;
        // A timeout too large to add to the clock is a wait with no deadline.
        let deadline = tokio::time::Instant::now().checked_add(timeout);
        let mut interval = Self::MIN_POLL;
        // A run with no turn at all has nothing to wait for.
        while self
            .newest_turn_outcome(&meta)?
            .is_some_and(|outcome| !outcome.is_terminal())
        {
            let now = tokio::time::Instant::now();
            let sleep = match deadline {
                Some(deadline) if now >= deadline => break,
                Some(deadline) => interval.min(deadline - now),
                None => interval,
            };
            tokio::time::sleep(sleep).await;
            interval = (interval * 2).min(Self::MAX_POLL);
        }
        Ok(())
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
            // Lock files sit beside the run directories and carry a `.lock` suffix, which is not
            // an id.
            .filter(|entry| entry.path().is_dir())
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
    pub(super) last_activity: Option<SystemTime>,
}

impl RunStore {
    /// Render the whole consultation as Markdown.
    ///
    /// The result is **append-only for the life of the consultation**, which is the contract
    /// [`RunStore::view`] pages by byte offset and `tail` hands back as a cursor.
    /// Two things make that true, and both are easy to break:
    ///
    /// - this header is built only from values fixed when the consultation was created, so
    ///   nothing here may ever carry state, elapsed time or a message count;
    /// - each turn's body grows only at its end, which
    ///   [`crate::transcript::render_turn`] documents and enforces, and the fold behind it never
    ///   reads past a turn's settlement, which [`settle`] documents and enforces.
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
        // is simply older, and skipping it is the correct ordering rule — under the lock, so the
        // comparison and the replacement are one step.
        // The published length is checked before the published text is read: a render that
        // has grown — every poll of a live consultation — never needs the old file read at all,
        // and a render that has not changed — every read of a finished one — is not rewritten.
        let _lock = self.lock(&meta.run_id)?;
        let published_len = std::fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if published_len >= u64::try_from(text.len()).unwrap_or(u64::MAX)
            && let Ok(published) = std::fs::read_to_string(&path)
            && published.starts_with(text)
        {
            return Ok(());
        }
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
}

impl ExitRecord {
    /// How much of the capture is part of the consultation; nothing past it is read.
    fn events_len(&self) -> u64 {
        match self {
            Self::Exited { events_len, .. } | Self::Cancelled { events_len } => *events_len,
        }
    }
}

/// One turn's directory with the two records that decide whether it is a turn at all.
#[derive(Debug)]
pub(super) struct TurnFiles {
    pub(super) index: u32,
    pub(super) dir: TurnDir,
    pub(super) launch: Option<LaunchRecord>,
    pub(super) exit: Option<ExitRecord>,
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

/// Claim a turn index by creating its directory.
///
/// Creating the directory *is* the claim, and it has to fail if someone else already made it:
/// two callers that both fold a one-turn consultation both choose index 1, and with a
/// forgiving create they would both launch a paid child into the same capture file.
///
/// A directory that exists but holds neither a launch nor an exit record was claimed by a
/// caller that died before recording its child.
/// Under the lock nobody else can be between those two steps, so such a claim is abandoned
/// and is taken over rather than left to block every follow-up for good; its files are set
/// aside, because a child spawned in that window may still be writing to them.
fn claim_turn(turn: &TurnDir, index: u32, run_id: &RunId) -> Result<(), RunError> {
    let claim = |source: std::io::Error| RunError::Io {
        context: format!("reserving turn {index}"),
        source,
    };
    match reserve_dir(&turn.root) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            let recorded = read_json::<LaunchRecord>(&turn.launch())?.is_some()
                || read_json::<ExitRecord>(&turn.exit())?.is_some();
            if recorded {
                return Err(RunError::TurnAlreadyClaimed {
                    run_id: run_id.clone(),
                    index,
                });
            }
            // Set aside rather than deleted: the caller that died may have spawned its child
            // first, and that child is still writing into these files.
            // A name no fold walks keeps them out of the consultation and keeps them for a human.
            let aside = turn
                .root
                .with_extension(format!("abandoned.{}", uuid::Uuid::new_v4().simple()));
            tracing::warn!(%run_id, turn = index, aside = %aside.display(), "taking over an abandoned turn claim");
            std::fs::rename(&turn.root, &aside).map_err(claim)?;
            reserve_dir(&turn.root).map_err(claim)
        }
        Err(source) => Err(claim(source)),
    }
}

/// Claim a directory, failing if it — or the directory it belongs in — already exists or does
/// not.
///
/// Never creates the parent: a turn directory whose run has been removed must not come back as
/// an orphan holding a running child.
fn reserve_dir(path: &Path) -> std::io::Result<()> {
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

/// Write a record, replacing any there before, so that a reader never sees a partial one.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RunError> {
    let temp = write_json_temp(path, value)?;
    std::fs::rename(&temp, path).map_err(|source| {
        let _ = std::fs::remove_file(&temp);
        RunError::Io {
            context: format!("writing {}", path.display()),
            source,
        }
    })
}

/// Write a record once, and hand back whatever record is there afterwards.
///
/// When another caller got there first, its record is returned rather than overwritten, so two
/// observers of one departure settle on one story.
/// The record is complete before it is visible: it is written beside its final name and linked
/// into place, and the link is what fails when the name is taken.
/// A filesystem without hard links gets an exclusive create instead, which still keeps the
/// first record but can let a reader glimpse a partial one — loudly, as a record that cannot be
/// parsed, never quietly as one that is not there.
fn write_json_once<T: Serialize + for<'de> Deserialize<'de>>(
    path: &Path,
    value: &T,
) -> Result<T, RunError> {
    let temp = write_json_temp(path, value)?;
    let placed = match std::fs::hard_link(&temp, path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => Err(source),
        Err(_) => std::fs::read(&temp).and_then(|text| {
            use std::io::Write as _;
            File::create_new(path)?.write_all(&text)
        }),
    };
    let _ = std::fs::remove_file(&temp);
    match placed {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(RunError::Io {
            context: format!("writing {}", path.display()),
            source,
        }),
    }?;
    read_json(path)?.ok_or_else(|| RunError::Io {
        context: format!("reading back {}", path.display()),
        source: std::io::Error::from(std::io::ErrorKind::NotFound),
    })
}

/// Write a record's whole text to a temporary file beside `path`, and return where.
fn write_json_temp<T: Serialize>(path: &Path, value: &T) -> Result<PathBuf, RunError> {
    let text = serde_json::to_string_pretty(value).map_err(|source| RunError::Io {
        context: format!("serialising {}", path.display()),
        source: std::io::Error::other(source),
    })?;
    let temp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temp, text).map_err(|source| RunError::Io {
        context: format!("writing {}", temp.display()),
        source,
    })?;
    Ok(temp)
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
fn read_complete_records(path: &Path, limit: Option<u64>) -> Result<(String, u64), RunError> {
    let mut bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(RunError::Io {
                context: format!("reading {}", path.display()),
                source,
            });
        }
    };
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
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, RunError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RunError::Io {
                context: format!("reading {}", path.display()),
                source,
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| RunError::CorruptRecord {
            path: path.to_owned(),
            source,
        })
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

/// The last `max_bytes` of a text file, or nothing for a file that is not there.
///
/// A file that cannot be read is an error rather than empty: this is the evidence a settlement
/// is recorded from, and recording "wrote nothing" over a transient failure would freeze the
/// wrong story.
fn tail_of_file(path: &Path, max_bytes: usize) -> Result<String, RunError> {
    let text = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(RunError::Io {
                context: format!("reading {}", path.display()),
                source,
            });
        }
    };
    if text.len() <= max_bytes {
        return Ok(text);
    }
    let start = text.floor_char_boundary(text.len().saturating_sub(max_bytes));
    Ok(text.get(start..).unwrap_or_default().to_owned())
}

/// Slice a render by byte offset without ever splitting a character.
fn page_of(full: &str, offset: u64, max_bytes: usize) -> TranscriptPage {
    let total = u64::try_from(full.len()).unwrap_or(u64::MAX);
    let start = usize::try_from(offset.min(total)).unwrap_or(usize::MAX);
    // Never split a UTF-8 character: walk back to a boundary, then forward for the end.
    let start = full.floor_char_boundary(start);
    let mut end = full.floor_char_boundary(start.saturating_add(max_bytes).min(full.len()));
    if end <= start && start < full.len() {
        // `max_bytes` was smaller than the next character — every turn heading contains an em
        // dash, so this is reachable with an accepted page size.
        // Returning an empty page would leave the cursor where it was and a polling caller
        // would spin forever, so emit one whole character instead.
        end = full.ceil_char_boundary(start.saturating_add(1));
    }
    let text = full.get(start..end).unwrap_or_default().to_owned();
    let next = u64::try_from(end).unwrap_or(total);
    TranscriptPage {
        text,
        offset: u64::try_from(start).unwrap_or(offset),
        next_offset: next,
        total_bytes: total,
        at_end: next >= total,
    }
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
