//! A scripted launcher, so the whole transcript pipeline runs with no credentials and no network.
//!
//! This is why [`Launcher`] is a trait.
//! Every claim the crate makes — that a blocking hook cannot replace the report, that a failure
//! keeps what it collected, that the child does not inherit the host session — is about what
//! happens between a spawn and a stream, and none of it is testable against a real CLI in CI.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::delegate::Invocation;
use crate::launch::{ExitStatus, LaunchError, LaunchSpec, Launched, Launcher, Liveness};

/// Take a lock, recovering from a poisoned one rather than panicking.
///
/// A failing assertion in one test unwinds and poisons any lock it held.
/// Panicking here would turn that one failure into a cascade of unrelated ones and bury the real
/// message.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What one scripted child does when it is launched.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// The event stream the child writes to `events.jsonl`.
    pub events: String,
    /// What the child writes to `stderr.log`.
    pub stderr: String,
    /// The exit status a later reap reports, or `None` for a child that is still running.
    pub exit: Option<ExitStatus>,
    /// Events appended to the capture file the first time this child is observed leaving.
    ///
    /// A child that exits on its own flushes at its first observation; one that is still running
    /// flushes once it has been asked to stop.
    /// Both model a real child flushing the tail of its stream as it exits, which is what makes
    /// the gap between reading the capture file and observing the process a race worth testing.
    pub flush_on_departure: Option<String>,
    /// Whether the child outlives every request to stop, as one stuck in an uninterruptible
    /// wait does.
    pub survives_stop: bool,
    /// Whether the launcher cannot tell the running child from another process holding its pid,
    /// as an agentmux restarted without a recorded start time cannot.
    ///
    /// Such a child reports [`Liveness::Unknown`] until it is
    /// [marked gone](ScriptedLauncher::mark_process_gone).
    pub unidentifiable: bool,
    /// How many observations — reaps and liveness checks — the child is still there for, after
    /// which it has exited cleanly; `None` for a child whose [`Script::exit`] decides.
    ///
    /// For the race between two observations of one child in one reading of the store.
    pub alive_for_observations: Option<usize>,
    /// What such a child writes to `last-message.md` as it leaves, as Codex writes its closing
    /// message on its way out.
    pub last_message_on_departure: Option<String>,
}

impl Script {
    /// A child that emits `events` and exits cleanly.
    #[must_use]
    pub fn completed(events: impl Into<String>) -> Self {
        Self {
            events: events.into(),
            exit: Some(0.into()),
            ..Self::default()
        }
    }

    /// A child that emits `events`, writes `stderr`, and exits with `code`.
    #[must_use]
    pub fn exited(events: impl Into<String>, stderr: impl Into<String>, code: i32) -> Self {
        Self {
            events: events.into(),
            stderr: stderr.into(),
            exit: Some(code.into()),
            ..Self::default()
        }
    }

    /// A child that emits `events` and is still running.
    #[must_use]
    pub fn running(events: impl Into<String>) -> Self {
        Self {
            events: events.into(),
            ..Self::default()
        }
    }

    /// A child that emits `events`, then flushes `tail` and exits at the moment it is first
    /// observed.
    ///
    /// This reproduces the one interleaving that matters: agentmux reads the capture file, the
    /// child writes its terminal event and exits, and only then is the process observed.
    #[must_use]
    pub fn flushes_as_it_exits(events: impl Into<String>, tail: impl Into<String>) -> Self {
        Self {
            events: events.into(),
            exit: Some(0.into()),
            flush_on_departure: Some(tail.into()),
            ..Self::default()
        }
    }

    /// A child that emits `events` and keeps running however it is asked to stop.
    #[must_use]
    pub fn survives_stop(events: impl Into<String>) -> Self {
        Self {
            events: events.into(),
            survives_stop: true,
            ..Self::default()
        }
    }

    /// A child that emits `events` and keeps running, which the launcher cannot identify.
    #[must_use]
    pub fn unidentifiable(events: impl Into<String>) -> Self {
        Self {
            events: events.into(),
            unidentifiable: true,
            ..Self::default()
        }
    }

    /// A child that emits `events` and is still there for the first `observations` times anything
    /// reaps it or asks after it, and has exited cleanly from then on.
    #[must_use]
    pub fn leaves_after(events: impl Into<String>, observations: usize) -> Self {
        Self {
            events: events.into(),
            alive_for_observations: Some(observations),
            ..Self::default()
        }
    }

    /// A child that emits `events`, keeps running, and flushes `tail` as it dies once it has been
    /// asked to stop.
    ///
    /// Models a delegate that had already finished its turn when the signal landed and got its
    /// terminal event out before going.
    #[must_use]
    pub fn flushes_when_stopped(events: impl Into<String>, tail: impl Into<String>) -> Self {
        Self {
            events: events.into(),
            flush_on_departure: Some(tail.into()),
            ..Self::default()
        }
    }
}

/// Write a configuration file that only its owner can write, whatever the umask.
///
/// `Config::load` refuses a file other users could write, and a shell with `umask 002` would
/// otherwise make every file a test writes one of those.
///
/// # Errors
///
/// Whatever writing the file or setting its permissions fails with.
pub fn write_config_file(
    path: impl AsRef<std::path::Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    let path = path.as_ref();
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// What a scripted launcher was actually asked to run.
#[derive(Debug, Clone)]
pub struct RecordedLaunch {
    /// The executable name.
    pub program: String,
    /// The argument vector, excluding `argv[0]`.
    pub args: Vec<String>,
    /// The child's entire environment.
    pub env: BTreeMap<String, String>,
    /// The directory the child was started in.
    pub cwd: PathBuf,
    /// Where the child's event stream was captured.
    pub events: PathBuf,
    /// The question, read back out of the file that would have been the child's stdin.
    pub question: String,
}

impl RecordedLaunch {
    /// Whether the argument vector contains `flag` followed by exactly `value`.
    #[must_use]
    pub fn has_flag_with(&self, flag: &str, value: &str) -> bool {
        self.args.windows(2).any(|pair| {
            pair.first().is_some_and(|f| f == flag) && pair.get(1).is_some_and(|v| v == value)
        })
    }

    /// Whether the argument vector contains `flag` at all.
    #[must_use]
    pub fn has_flag(&self, flag: &str) -> bool {
        self.args.iter().any(|arg| arg == flag)
    }
}

/// A launcher that plays back canned event streams instead of starting processes.
///
/// Each launcher hands out pids from a range of its own, so two launchers serving one store, as
/// two agentmux processes would, never name each other's children; a pid a launcher did not hand
/// out is a process it cannot see, which it reports gone, as the real launcher reports a process
/// that is not there.
#[derive(Debug)]
pub struct ScriptedLauncher {
    /// The first pid this launcher hands out.
    first_pid: u32,
    scripts: Mutex<VecDeque<Script>>,
    launches: Mutex<Vec<RecordedLaunch>>,
    terminated: Mutex<Vec<u32>>,
    killed: Mutex<Vec<u32>>,
    flushed: Mutex<BTreeSet<u32>>,
    gone_pids: Mutex<BTreeSet<u32>>,
    /// How many times each child has been reaped or asked after.
    observations: Mutex<BTreeMap<u32, usize>>,
}

impl ScriptedLauncher {
    /// A launcher that will play `scripts` in order, one per launch.
    #[must_use]
    pub fn new(scripts: impl IntoIterator<Item = Script>) -> Self {
        let launcher_index = NEXT_LAUNCHER_INDEX.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            first_pid: FAKE_PID_BASE
                .saturating_add(launcher_index.saturating_mul(PIDS_PER_LAUNCHER)),
            scripts: Mutex::new(scripts.into_iter().collect()),
            launches: Mutex::new(Vec::new()),
            terminated: Mutex::new(Vec::new()),
            killed: Mutex::new(Vec::new()),
            flushed: Mutex::new(BTreeSet::new()),
            gone_pids: Mutex::new(BTreeSet::new()),
            observations: Mutex::new(BTreeMap::new()),
        }
    }

    /// Everything the launcher was asked to run, in order.
    #[must_use]
    pub fn launches(&self) -> Vec<RecordedLaunch> {
        lock(&self.launches).clone()
    }

    /// The handle this launcher returns for its `index`th launch, counting from zero, so a test
    /// can stop that child as a real one would stop.
    #[must_use]
    pub fn launch_handle(&self, index: usize) -> Launched {
        Launched {
            pid: self
                .first_pid
                .saturating_add(u32::try_from(index).unwrap_or(u32::MAX)),
            process_group: None,
            started: None,
        }
    }

    /// Which of this launcher's launches `pid` is, or `None` for a pid it never handed out.
    fn launch_index_for_pid(&self, pid: u32) -> Option<usize> {
        pid.checked_sub(self.first_pid)
            .filter(|offset| *offset < PIDS_PER_LAUNCHER)
            .and_then(|offset| usize::try_from(offset).ok())
    }

    /// The pids the launcher was asked to terminate.
    #[must_use]
    pub fn terminated(&self) -> Vec<u32> {
        lock(&self.terminated).clone()
    }

    /// The pids the launcher was asked to kill outright, after termination was not enough.
    #[must_use]
    pub fn killed(&self) -> Vec<u32> {
        lock(&self.killed).clone()
    }

    /// Take a child out of the process table without being asked, as one agentmux cannot signal
    /// leaves when it is done.
    pub fn mark_process_gone(&self, launched: Launched) {
        lock(&self.gone_pids).insert(launched.pid);
    }

    /// Whether a scripted child has been asked to stop.
    ///
    /// A scripted child is cooperative: it dies on the first request, the way both real CLIs do
    /// on `SIGTERM`.
    /// A script that flushes on liveness still gets to flush first, which is how the "outran the
    /// signal" interleaving is reproduced.
    fn stopped(&self, pid: u32) -> bool {
        lock(&self.terminated).contains(&pid) || lock(&self.killed).contains(&pid)
    }

    fn script_for(&self, index: usize) -> Script {
        lock(&self.scripts).get(index).cloned().unwrap_or_default()
    }

    /// Count one observation of `pid`, and say whether a child that leaves after a number of
    /// them has left by this one, writing its closing message as it goes.
    fn observe_departure(&self, pid: u32, index: usize, script: &Script) -> bool {
        let Some(alive_for) = script.alive_for_observations else {
            return false;
        };
        let seen = {
            let mut observations = lock(&self.observations);
            let seen = observations.entry(pid).or_insert(0);
            *seen = seen.saturating_add(1);
            *seen
        };
        if seen == alive_for.saturating_add(1)
            && let Some(text) = &script.last_message_on_departure
            && let Some(recorded) = lock(&self.launches).get(index)
        {
            std::fs::write(recorded.events.with_file_name("last-message.md"), text)
                .inspect_err(|err| {
                    tracing::warn!(?err, "a scripted closing message was not written");
                })
                .ok();
        }
        seen > alive_for
    }

    /// Append the script's late tail the first time the child is observed.
    ///
    /// The flush happens before the answer, so a caller that reads the capture file and then
    /// observes the process sees exactly the interleaving a real exiting child produces.
    fn flush_on_first_observation(&self, pid: u32, index: usize, script: &Script) {
        let leaving = script.exit.is_some() || self.stopped(pid);
        if leaving
            && let Some(tail) = &script.flush_on_departure
            && lock(&self.flushed).insert(pid)
            && let Some(recorded) = lock(&self.launches).get(index)
            && let Ok(mut existing) = std::fs::read_to_string(&recorded.events)
        {
            existing.push_str(tail);
            let _ = std::fs::write(&recorded.events, existing);
        }
    }
}

impl Default for ScriptedLauncher {
    fn default() -> Self {
        Self::new([])
    }
}

/// Pids above `i32::MAX` so a stray reap can never name a real process.
const FAKE_PID_BASE: u32 = 4_000_000_000;

/// How many pids each launcher's range holds, which is more launches than any test makes.
const PIDS_PER_LAUNCHER: u32 = 1024;

/// How many launchers this process has made, which places the next one's pid range.
static NEXT_LAUNCHER_INDEX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl Launcher for ScriptedLauncher {
    /// A scripted delegate is no CLI, and names itself after the program it stands in for.
    fn version(&self, invocation: &Invocation, _cwd: &Path) -> Option<String> {
        Some(format!("{} (scripted)", invocation.program()))
    }

    fn launch(&self, spec: &LaunchSpec<'_>) -> Result<Launched, LaunchError> {
        let mut launches = lock(&self.launches);
        let index = launches.len();
        let script = self.script_for(index);

        std::fs::write(spec.events, &script.events).map_err(|source| LaunchError::Io {
            path: spec.events.to_owned(),
            source,
        })?;
        std::fs::write(spec.stderr, &script.stderr).map_err(|source| LaunchError::Io {
            path: spec.stderr.to_owned(),
            source,
        })?;

        launches.push(RecordedLaunch {
            program: spec.invocation.program().to_owned(),
            args: spec.invocation.args().to_vec(),
            env: spec.invocation.env().clone(),
            cwd: spec.cwd.to_owned(),
            events: spec.events.to_owned(),
            question: std::fs::read_to_string(spec.question).unwrap_or_default(),
        });

        Ok(self.launch_handle(index))
    }

    fn liveness(&self, launched: Launched) -> Liveness {
        let Some(index) = self.launch_index_for_pid(launched.pid) else {
            return Liveness::Gone;
        };
        let script = self.script_for(index);
        self.flush_on_first_observation(launched.pid, index, &script);

        if script.exit.is_some()
            || self.observe_departure(launched.pid, index, &script)
            || (self.stopped(launched.pid) && !script.survives_stop)
            || lock(&self.gone_pids).contains(&launched.pid)
        {
            Liveness::Gone
        } else if script.unidentifiable {
            Liveness::Unknown
        } else {
            Liveness::Alive
        }
    }

    // Both stop requests are recorded only for a child that is still there, as the real
    // launcher delivers them: a test counting terminations sees what a process would have seen.
    fn terminate(&self, launched: Launched) {
        if self.liveness(launched) == Liveness::Alive {
            lock(&self.terminated).push(launched.pid);
        }
    }

    fn kill(&self, launched: Launched) {
        if self.liveness(launched) == Liveness::Alive {
            lock(&self.killed).push(launched.pid);
        }
    }

    fn reap(&self, launched: Launched) -> Option<ExitStatus> {
        let index = self.launch_index_for_pid(launched.pid)?;
        let script = self.script_for(index);
        self.flush_on_first_observation(launched.pid, index, &script);
        if self.observe_departure(launched.pid, index, &script) {
            return Some(0.into());
        }
        script.exit.or_else(|| {
            // A stopped child reports the signal that stopped it, as a real one would.
            (self.stopped(launched.pid) && !script.survives_stop).then_some(ExitStatus {
                code: None,
                signal: Some(15),
            })
        })
    }
}

/// Keeps this process's standard handles out of the processes it starts, as agentmux does before
/// every spawn of its own.
///
/// A test that runs the real `agentmux` with its output captured calls it before each spawn.
/// On Windows the test's own standard handles are the runner's capture pipes, and without it
/// they would pass to `agentmux` beside the pipes the test gives it, and on from there into the
/// detached supervisor and delegate, where agentmux cannot tell them from any other handle.
/// Elsewhere it does nothing.
pub fn keep_std_handles_from_children() {
    crate::launch::keep_std_handles_from_children();
}

/// A recorded event stream, committed under `fixtures/`.
///
/// Every fixture was captured from a real CLI run with the exact flags [`crate::delegate`] builds.
/// They are the only thing that keeps this crate honest about vendor JSON it does not control.
pub mod fixtures {
    /// A short Claude consultation that answered and finished cleanly.
    pub const CLAUDE_HAPPY: &str = include_str!("../fixtures/claude/happy.jsonl");

    /// A Claude consultation resumed with `--resume`.
    ///
    /// Establishes two things the reopening rule depends on.
    /// A resumed stream carries only the new turn — prior turns are not replayed as `user` text —
    /// so a genuine follow-up never trips the hook warning.
    /// And `system/init` reports the same session id that was resumed, which is what makes a
    /// silently-abandoned resume detectable.
    pub const CLAUDE_RESUME: &str = include_str!("../fixtures/claude/resume.jsonl");

    /// A Claude consultation that used a tool.
    ///
    /// Its `user` event carries a `tool_result` block, never a `text` one, which is what makes
    /// reopening detection structural rather than a string match.
    pub const CLAUDE_TOOL_USE: &str = include_str!("../fixtures/claude/tool-use.jsonl");

    /// The fixture this project exists for.
    ///
    /// A blocking `Stop` hook reopened the finished turn eight times.
    /// The report is the second assistant message; `result.result` came back as the **empty
    /// string** with `is_error: false`, `subtype: "success"` and `terminal_reason: "completed"`.
    /// Anything that read the last message, or the CLI's own `result` field, would have reported a
    /// clean run and lost the answer.
    pub const CLAUDE_STOP_HOOK: &str =
        include_str!("../fixtures/claude/stop-hook-reopens-the-turn.jsonl");

    /// An unknown Claude model: exit 1, `subtype: "success"`, `is_error: true`,
    /// `terminal_reason: "api_error"`, `api_error_status: 404`.
    pub const CLAUDE_UNKNOWN_MODEL: &str = include_str!("../fixtures/claude/unknown-model.jsonl");

    /// A Claude account that has exhausted one model's usage window.
    ///
    /// The refusal arrives as `api_error_status: 429` alongside `subtype: "success"`, so the
    /// classification is structural and survives the vendor rewording the message.
    /// It also carries the `rate_limit_event` this crate reads the reopening time out of, which is
    /// the only thing that distinguishes a window worth waiting for from one worth switching away
    /// from.
    pub const CLAUDE_RATE_LIMITED: &str = include_str!("../fixtures/claude/rate-limited.jsonl");

    /// A short Codex consultation that answered and finished cleanly.
    pub const CODEX_HAPPY: &str = include_str!("../fixtures/codex/happy.jsonl");

    /// A Codex consultation that ran a shell command, so `command_execution` items appear.
    pub const CODEX_TOOL_USE: &str = include_str!("../fixtures/codex/tool-use.jsonl");

    /// A Codex resume whose model differed from the recorded one.
    ///
    /// It emits `item.completed` with `item.type: "error"` — and then completes successfully.
    /// That event is a warning, not a failure.
    pub const CODEX_RESUME_WARNING: &str =
        include_str!("../fixtures/codex/resume-with-model-mismatch-warning.jsonl");

    /// A Codex model the account cannot use: one advisory item error, one top-level `error`, and
    /// `turn.failed`, all for a single failure.
    pub const CODEX_UNSUPPORTED_MODEL: &str =
        include_str!("../fixtures/codex/unsupported-model.jsonl");

    /// Every committed fixture, paired with the vendor that produced it.
    #[must_use]
    pub fn all() -> Vec<(crate::delegate::Vendor, &'static str, &'static str)> {
        use crate::delegate::Vendor::{Claude, Codex};
        vec![
            (Claude, "claude/happy", CLAUDE_HAPPY),
            (Claude, "claude/rate-limited", CLAUDE_RATE_LIMITED),
            (Claude, "claude/tool-use", CLAUDE_TOOL_USE),
            (Claude, "claude/resume", CLAUDE_RESUME),
            (
                Claude,
                "claude/stop-hook-reopens-the-turn",
                CLAUDE_STOP_HOOK,
            ),
            (Claude, "claude/unknown-model", CLAUDE_UNKNOWN_MODEL),
            (Codex, "codex/happy", CODEX_HAPPY),
            (Codex, "codex/tool-use", CODEX_TOOL_USE),
            (
                Codex,
                "codex/resume-with-model-mismatch-warning",
                CODEX_RESUME_WARNING,
            ),
            (Codex, "codex/unsupported-model", CODEX_UNSUPPORTED_MODEL),
        ]
    }
}
