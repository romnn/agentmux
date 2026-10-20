//! Run directories, ids, lifecycle, retention and resume state.
//!
//! Changes when storage changes.
//!
//! # The shape on disk
//!
//! ```text
//! runs/<run_id>.lock           held while the run is folded, its render published and its turns
//!                              settled, a turn is claimed or its delegate started, a steer is
//!                              submitted, a cancellation is marked or recorded, or the run is
//!                              removed
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
//!     launch.json              the delegate's pid, start time and process group, so a later call
//!                              can check on it
//!     supervisor-plan.json     what the turn's supervisor runs, with no environment in it
//!     supervisor.json          the supervisor's own pid and start time, and how far it got
//!     supervisor.log           what the supervisor printed
//!     app-server.json          the plan a turn driven over `codex app-server` runs under
//!     steer/                   messages steered into such a turn, and the driver's replies
//!     cancelling               present once a cancellation was asked for
//!     exit.json                how the child ended and how much it had written, written once and
//!                              never rewritten
//!     settled.json             the commitment that the turn's transcript is published and final,
//!                              written last and never rewritten
//! ```
//!
//! A turn directory counts as a turn once it holds a launch, a supervisor or an exit record.
//! Before that it is a claim: the directory is created under the run's lock as the claim on its
//! index, and the launch is recorded before the lock is released, so a claim with no record can
//! only have been abandoned by a caller that died in between.
//!
//! A store opened [`with_supervisor`](RunStore::with_supervisor) launches each turn's
//! `agentmux supervise`, which starts the delegate itself and settles the turn the moment the
//! delegate has gone; one opened without launches the delegate directly, and its turns settle
//! when they are next read.
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
//! `settled.json` is the one record a waiter returns on: written after the transcript holding the
//! turn's final section, by whichever settles the turn first, under the lock.
//! Terminal state is decided in this order:
//!
//! 1. a terminal event in the stream — authoritative, because it survives everything else;
//! 2. `exit.json`, when the child was reaped, observed gone, or cancelled;
//! 3. process liveness, as a last resort.
//!
//! # What the lock is for
//!
//! Several agentmux processes may address one consultation — an MCP server and a terminal, or a
//! server restarted underneath a `tail` — and every change to a run's shape is a check followed
//! by a write that another process could act between.
//! So each runs while a `LockedRun` holds the run's advisory lock, either as one of its
//! methods or inside a store method that holds one; the module says which are which.

mod cancel;
mod error;
mod id;
pub(crate) mod layout;
mod lock;
mod publish;
mod quota;
mod records;
mod retention;
mod settle;
mod spawn;
mod status;
mod steer;
mod supervise;
mod wait;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use crate::delegate::{AccountChoice, Billing, SessionRef, Vendor};
use crate::launch::{Launched, Launcher, Liveness};
use crate::transcript::{Outcome, Transcript};

use layout::create_private_dir;
use records::{
    ExitRecord, LaunchRecord, SettledRecord, SupervisorRecord, TurnFiles, read_if_present,
    read_json, tail_of_file,
};
use status::{STATUS_STDERR_TAIL, elapsed_since, first_line, page_of};

pub use error::{NotResumable, NotRunningReason, RunError, SteerError, UnsteerableReason};
pub use id::{RunId, RunIdError};
pub use quota::QuotaReport;
pub use records::{LaunchMode, Meta, SettledTurn};
pub use retention::Retention;
pub use spawn::StartRequest;
pub use status::{HookReopening, RunStatus, RunSummary, TranscriptPage, describe_delegate};
pub use supervise::{SUBCOMMAND as SUPERVISE_SUBCOMMAND, supervise};
pub use wait::{TurnRef, WaitFor, Waited};

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
    /// The agentmux executable that drives a Codex turn over `codex app-server`, when this store
    /// launches such turns that way.
    codex_driver: Option<PathBuf>,
    /// The agentmux executable that supervises each turn this store launches, when it launches
    /// them that way.
    supervisor: Option<PathBuf>,
}

impl std::fmt::Debug for RunStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl RunStore {
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
        // Absolute, because every path under it is handed to children that run in the
        // delegate's working directory: the driver's plan, and `AGENTMUX_STATE_DIR` itself.
        let root = std::path::absolute(root.into())
            .map_err(RunError::io("resolving the state directory"))?;
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
            // `exec` for every Codex turn until a caller names a driver: embedding this library
            // must not start an executable the embedder did not provide.
            codex_driver: None,
            // Unsupervised for the same reason.
            supervisor: None,
        })
    }

    /// Launch every turn under a supervisor, `<supervisor> supervise <plan>`, which starts the
    /// turn's delegate, waits for it to go, and settles the turn the moment it has.
    ///
    /// `supervisor` is an `agentmux` executable, which runs [`supervise`](fn@supervise).
    /// With one, a turn's `settled.json` and its final transcript are on disk within moments of
    /// its delegate leaving, whether or not anything is asking about the consultation.
    /// Without one, the delegate is launched directly and its turn settles when something next
    /// reads it.
    /// Independent of [`RunStore::with_codex_app_server`]: a driven turn's driver is the delegate
    /// the supervisor starts.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: impl Into<PathBuf>) -> Self {
        self.supervisor = Some(supervisor.into());
        self
    }

    /// Drive every Codex turn that can run over `codex app-server` through `driver`, so it can be
    /// steered while it runs.
    ///
    /// `driver` is an `agentmux` executable: the run store launches it as
    /// `<driver> codex-app-server <plan>`, and it runs [`crate::appserver::drive`].
    /// Only a Codex delegate that inherits its account's settings runs this way — see
    /// [`crate::delegate::Delegate::app_server_settings`] — and every other turn still runs over
    /// `codex exec` or `claude --print`.
    #[must_use]
    pub fn with_codex_app_server(mut self, driver: impl Into<PathBuf>) -> Self {
        self.codex_driver = Some(driver.into());
        self
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
            let files = self.read_turn_files(run_id, index, dir)?;
            if !files.is_recorded() {
                break;
            }
            turns.push(files);
        }
        Ok(turns)
    }

    /// One turn directory's records, whether or not they make it a turn.
    ///
    /// A settlement marker is validated as it is read, so no reader folds, renders or continues
    /// a turn from a marker that does not hold.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::InvalidSettlement`] for a marker that does not hold, besides what
    /// reading a record fails with.
    fn read_turn_files(
        &self,
        run_id: &RunId,
        index: u32,
        dir: layout::TurnDir,
    ) -> Result<TurnFiles, RunError> {
        let settled = read_json::<SettledRecord>(&dir.settled())?;
        if let Some(record) = &settled {
            SettledTurn::validate(run_id, &dir, index, &self.transcript_path(run_id), record)?;
        }
        Ok(TurnFiles {
            index,
            launch: read_json::<LaunchRecord>(&dir.launch())?,
            exit: read_json::<ExitRecord>(&dir.exit())?,
            supervisor: read_json::<SupervisorRecord>(&dir.supervisor())?,
            settled,
            dir,
        })
    }

    /// Whether a turn's delegate has gone, so nothing it does can still change its session.
    ///
    /// A turn is over the moment its stream says so, but both CLIs save the session on their way
    /// out, and a follow-up that resumed before then could miss the end of the conversation or
    /// write beside the process still saving it.
    /// A child whose identity cannot be checked has not gone: it may be the delegate still
    /// saving, and the uncertainty clears once nothing holds its pid.
    ///
    /// A supervised turn whose delegate is not recorded yet has not gone either: its supervisor
    /// is still starting it, unless a record says it never started.
    fn delegate_has_exited(&self, files: &TurnFiles) -> bool {
        match (&files.exit, &files.launch) {
            // A supervisor starts nothing once a record exists, so a turn cancelled before its
            // delegate was recorded never had one.
            (Some(ExitRecord::Exited { .. } | ExitRecord::NotStarted { .. }), _)
            | (Some(ExitRecord::Cancelled { .. }), None) => true,
            (_, Some(record)) => self.child_has_gone(record.launched),
            (None, None) => files.supervisor.is_none(),
        }
    }

    /// Whether a live supervisor other than this process will settle its turn's departure.
    ///
    /// Only the supervisor holds its delegate's handle, so only it can record the exit status a
    /// silent failure is explained by; another observer that recorded the departure first would
    /// freeze an exit of unknown status in its place.
    /// A supervisor that has gone settles nothing, and the next observer repairs its turn.
    fn supervisor_will_settle(&self, supervisor: Option<&SupervisorRecord>) -> bool {
        supervisor.is_some_and(|supervisor| {
            !is_this_process(supervisor.launched) && !self.child_has_gone(supervisor.launched)
        })
    }

    /// Whether a launched child has gone: reaped through the handle this process kept, or no
    /// longer the process its record names.
    ///
    /// Reaped first, because a finished child of this process lingers as a zombie until it is,
    /// and its liveness would read alive.
    /// A child whose identity cannot be checked has not gone.
    pub(super) fn child_has_gone(&self, launched: Launched) -> bool {
        self.launcher.reap(launched).is_some() || self.launcher.liveness(launched) == Liveness::Gone
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
    /// Most callers want only the page, and each would otherwise discard the state half of the
    /// pair.
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
        // Folded and published under the lock, so the reading that is published is one no other
        // caller can settle differently in between; everything slow happens after it.
        // Every turn that has become final is settled by the same hold, so a reader repairs what
        // a supervisor that died left behind, and an older store is settled as it is read.
        let publish::Published {
            mut state,
            rendered: publish::Rendered { text: full, .. },
        } = self.lock(run_id)?.fold_and_publish(&meta)?;
        self.resolve_rate_limit_lookups(&meta, &mut state);

        // A run with no turn yet describes where its first one will be.
        let turn = self.turn_dir(
            run_id,
            state.newest.as_ref().map_or(0, |newest| newest.index),
        );
        let outcome = state.transcript.outcome();
        let quota = self.quota_for_failure(run_id, &meta, &outcome);
        // Only a failure the stream reported: one settled by its exit record already quotes the
        // tail that record froze, and repeating it here would say the same thing twice.
        let delegate_stderr = match &outcome {
            Outcome::Failed { .. }
                if state
                    .newest
                    .as_ref()
                    .is_none_or(|newest| newest.settled_by != Some(SettledBy::ExitRecord)) =>
            {
                tail_of_file(&turn.stderr(), STATUS_STDERR_TAIL)
                    .ok()
                    .map(|text| text.trim().to_owned())
                    .filter(|text| !text.is_empty())
            }
            _ => None,
        };

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
            resolved_model: state.transcript.resolved_model().map(ToOwned::to_owned),
            rewritten_from: meta.rewritten_from.clone(),
            unsaved_session_store: state.unsaved_session_store().map(Path::to_path_buf),
            delegate_stderr,
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
            has_undecoded_app_server_messages: state
                .transcript
                .turns
                .iter()
                .any(|turn| turn.has_undecoded_app_server_messages),
            transcript_path: self.transcript_path(run_id),
            events_path: turn.events(),
            stderr_path: turn.stderr(),
            transcript_bytes: u64::try_from(full.len()).unwrap_or(u64::MAX),
            // Advertised only when a follow-up would actually work, because `next_steps` turns
            // this into a recommendation and a caller that takes it pays for the turn.
            resumable: Self::resumable_session(run_id, &state).is_ok(),
            steerable: state.newest.as_ref().is_some_and(|newest| {
                Self::accepts_steers(&outcome, newest.launch_mode, newest.turn_started, &turn)
            }),
            billing: state.transcript.turns.last().and_then(|turn| turn.billing),
            settled: state.newest.as_ref().is_some_and(|newest| newest.settled),
            outcome,
        };
        Ok((status, full))
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
                // The outcome and the pricing are the newest turn's, as `status` reports them, and
                // the question is the first turn's own file; none needs the whole consultation
                // folded.
                let newest = self
                    .lock(&meta.run_id)
                    .and_then(|run| run.fold_newest_turn(&meta))
                    .ok()?;
                let outcome = newest
                    .as_ref()
                    .map_or(Outcome::Running, |turn| turn.outcome.clone());
                let billing = newest.and_then(|turn| turn.billing);
                let question = std::fs::read_to_string(self.turn_dir(&meta.run_id, 0).question())
                    .map(|question| first_line(&question, 120))
                    .unwrap_or_default();
                let mut delegate = describe_delegate(&meta.delegate, meta.rewritten_from.as_ref());
                // Said where every listing shows it, as the status header says it.
                if billing == Some(Billing::Api) {
                    delegate.push_str(" billing=api");
                }
                Some(RunSummary {
                    run_id: meta.run_id,
                    delegate,
                    outcome,
                    created_at: meta.created_at,
                    question,
                    billing,
                })
            })
            .collect())
    }

    /// What agentmux recorded when the consultation began.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, or
    /// [`RunError::Corrupt`] when its metadata cannot be parsed.
    pub fn meta(&self, run_id: &RunId) -> Result<Meta, RunError> {
        let path = self.run_dir(run_id).join("meta.json");
        let Some(text) = read_if_present(&path, std::fs::read_to_string)? else {
            return Err(RunError::NotFound(run_id.clone()));
        };
        let mut meta: Meta = serde_json::from_str(&text).map_err(|source| RunError::Corrupt {
            run_id: run_id.clone(),
            source,
        })?;
        // Every account is resolved when a consultation starts, so an unresolved one is a record
        // written before agentmux pinned the CLI login, which is what such a record ran on.
        if *meta.delegate.account() == AccountChoice::Default {
            meta.delegate = meta.delegate.with_account(AccountChoice::CliLogin);
        }
        Ok(meta)
    }

    /// Poll until a consultation's newest turn reaches a terminal state, or until `timeout`
    /// elapses.
    ///
    /// Never fails for taking too long: on timeout it simply returns, the consultation carries
    /// on, and the caller's next `status` or `view` says so.
    /// Both hosts cap how long a tool call may block — Codex at sixty seconds by default — so
    /// callers pass a cap well under theirs.
    /// This is a bounded observation, not a completion signal: it returns when the stream says
    /// the turn ended, which may be before the turn has settled.
    /// [`RunStore::wait_for_settlement`] is the wait that returns only then.
    ///
    /// The turn is the newest one when the wait begins, and stays that one: a follow-up started
    /// meanwhile is not waited for in its place.
    /// The wait folds only that turn on each poll, renders and writes nothing, and backs its
    /// interval off towards a two-second ceiling.
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
        // A run with no turn at all has nothing to wait for.
        let Some(pinned) = self.turn_files(run_id)?.last().map(|files| files.index) else {
            return Ok(());
        };
        // A timeout too large to add to the clock is a wait with no deadline.
        let deadline = tokio::time::Instant::now().checked_add(timeout);
        let mut interval = Self::MIN_POLL;
        loop {
            let running = {
                let run = self.lock(run_id)?;
                match self
                    .turn_files(run_id)?
                    .iter()
                    .find(|files| files.index == pinned)
                {
                    Some(files) => !run.fold_turn(&meta, files)?.turn.outcome.is_terminal(),
                    None => false,
                }
            };
            if !running {
                return Ok(());
            }
            let now = tokio::time::Instant::now();
            let sleep = match deadline {
                Some(deadline) if now >= deadline => return Ok(()),
                Some(deadline) => interval.min(deadline - now),
                None => interval,
            };
            tokio::time::sleep(sleep).await;
            interval = (interval * 2).min(Self::MAX_POLL);
        }
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
    /// What the fold established about the newest turn, or `None` for a run with no turn.
    pub(super) newest: Option<NewestTurn>,
    pub(super) last_activity: Option<SystemTime>,
    /// The usage windows the fold left to [`RunStore::resolve_rate_limit_lookups`].
    pending_rate_limit_lookups: Vec<settle::RateLimitLookup>,
    /// Whether each turn, by index, has settled or can.
    finality: BTreeMap<u32, settle::Finality>,
}

/// What the fold established about a run's newest turn, which is the one `status` describes, a
/// steer reaches, and a follow-up continues.
#[derive(Debug, Clone, Copy)]
pub(super) struct NewestTurn {
    /// Its index.
    pub(super) index: u32,
    /// What decided its outcome, once something has.
    pub(super) settled_by: Option<SettledBy>,
    /// How its delegate was launched.
    pub(super) launch_mode: LaunchMode,
    /// Whether its capture says the turn itself began.
    pub(super) turn_started: bool,
    /// Whether it is settled, committed by this fold or an earlier one, which a follow-up waits
    /// for.
    pub(super) settled: bool,
    /// Whether its delegate has gone, from the same observation the settlement was decided on.
    ///
    /// Apart from `settled` only for a cancellation the delegate survived: the turn is frozen
    /// and settled, but the process saving its session is still there.
    pub(super) delegate_gone: bool,
}

/// What decided a turn's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettledBy {
    /// A terminal event in its stream.
    TerminalEvent,
    /// Its exit record, because the stream carried no terminal event.
    ExitRecord,
}

impl RunState {
    /// Where the newest turn's delegate could not save its session, when it could not.
    ///
    /// The newest turn decides: it is the one a follow-up would resume, and a turn that was not
    /// kept leaves nothing for the CLI to continue from, whatever the earlier turns saved.
    fn unsaved_session_store(&self) -> Option<&Path> {
        self.transcript
            .turns
            .last()
            .and_then(|turn| turn.unsaved_session_store.as_deref())
    }
}

/// Whether a recorded process is this one, which a supervisor asks of its own record so that
/// it never waits on itself.
fn is_this_process(launched: Launched) -> bool {
    launched.pid == std::process::id()
}
