//! Turns a prepared invocation into a detached child whose output goes to files.
//!
//! Changes when process handling changes.
//!
//! # Why the child writes to files rather than to a pipe
//!
//! **The capture file is the state.**
//! The child writes its event stream through a file descriptor agentmux hands it, not through a
//! pipe agentmux must keep draining.
//! If agentmux dies mid-review the child keeps writing, and a later fold reconstructs everything
//! by re-reading the file.
//! There is no in-memory buffer whose loss loses the report.
//!
//! # Why the child is detached
//!
//! A review can run for ninety minutes.
//! Restarting or upgrading the MCP server must not kill it, so the child gets its own process
//! group and does not share this process's terminal.
//!
//! # What survives an agentmux restart
//!
//! While agentmux lives it keeps the child handle, so exit status, termination and a forced kill
//! work on every platform.
//! After a restart the handle is gone, and all that is left is the pid on disk.
//! A pid is recycled, so it is recorded together with the process's start time, and a process
//! found under that pid is the child only if its start time matches; the exit status is lost,
//! which the store records as an exit with an unknown status.
//!
//! # The one trait in the crate
//!
//! [`Launcher`] exists because tests need to exercise the whole transcript pipeline — including
//! the hook-reopen case — with no credentials and no network.
//! It is the seam that pays; everything else stays concrete.

use std::collections::HashMap;
use std::fs::File;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::delegate::Invocation;

/// A child process could not be started.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// A capture file could not be created, or the question file could not be opened.
    #[error("cannot open {path}: {source}")]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The directory the delegate was to read from is not there.
    ///
    /// Reported apart from a spawn failure: the operating system reports both as "not found",
    /// and telling a caller its CLI is missing when its path was mistyped sends it to reinstall a
    /// working tool.
    #[error("the working directory {path} is not a directory")]
    WorkingDirectory {
        /// The directory that was asked for.
        path: PathBuf,
    },
    /// The delegate CLI could not be executed.
    #[error("cannot run `{program}`: {source}. Is it installed and on PATH?")]
    Spawn {
        /// The executable that was not runnable.
        program: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
}

/// Everything a launcher needs in order to start one turn's child.
#[derive(Debug, Clone, Copy)]
pub struct LaunchSpec<'a> {
    /// What to run, with which arguments and environment.
    pub invocation: &'a Invocation,
    /// The directory the delegate reads the project from.
    pub cwd: &'a Path,
    /// File holding the question, opened as the child's stdin.
    pub question: &'a Path,
    /// File the child's stdout is redirected to.
    /// This is the event stream.
    pub events: &'a Path,
    /// File the child's stderr is redirected to.
    pub stderr: &'a Path,
}

/// A started child, identified well enough to check on and to stop later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launched {
    /// The child's process id.
    pub pid: u32,
    /// The process group agentmux created for it, when the platform has process groups.
    ///
    /// Equal to `pid`, because the child is the group leader.
    /// Kept explicitly because it is the address a termination is sent to: a delegate CLI spawns
    /// tool subprocesses, and stopping only the leader would leave them running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group: Option<u32>,
    /// When the child started, in seconds since the epoch, as the operating system reports it.
    ///
    /// The pid alone does not identify a process across an agentmux restart: it is recycled, and
    /// a recycled pid that happens to lead a process group of its own passes every other test.
    /// A process found under this pid is the child only if it also started at this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
}

/// How a child ended, as recorded once it has been reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    /// The exit code, absent when the child was killed by a signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// The signal that killed the child, where the platform reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

impl ExitStatus {
    /// Whether the child exited cleanly.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.code == Some(0) && self.signal.is_none()
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(f, "exit code {code}"),
            (None, Some(signal)) => write!(f, "signal {signal}"),
            (None, None) => f.write_str("an unknown status"),
        }
    }
}

impl From<i32> for ExitStatus {
    /// A plain exit code, with no signal.
    fn from(code: i32) -> Self {
        Self {
            code: Some(code),
            signal: None,
        }
    }
}

impl From<std::process::ExitStatus> for ExitStatus {
    fn from(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt as _;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
        }
    }
}

/// Whether a child is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The process exists and is the one that was launched.
    Alive,
    /// The process is gone, or the pid now belongs to something else.
    Gone,
    /// This platform cannot answer without the child handle, which an agentmux restart lost.
    Unknown,
}

/// Starts delegate processes.
///
/// Implemented for real by [`ProcessLauncher`] and substituted in tests by a scripted fake.
pub trait Launcher: Send + Sync {
    /// Start a child for one turn.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError`] when a capture file cannot be created or the CLI cannot be executed.
    fn launch(&self, spec: &LaunchSpec<'_>) -> Result<Launched, LaunchError>;

    /// Whether a previously launched child is still running.
    fn liveness(&self, launched: Launched) -> Liveness;

    /// Ask a running child, and everything it spawned, to stop.
    ///
    /// Best effort: a child that has already exited is not an error.
    fn terminate(&self, launched: Launched);

    /// Stop a child that did not honour [`Launcher::terminate`].
    ///
    /// Best effort, like `terminate`.
    fn kill(&self, launched: Launched);

    /// Collect the exit status of a child that has finished, without blocking.
    ///
    /// Only meaningful while agentmux is still the child's parent; after an agentmux restart the
    /// child has been reparented and this reports nothing.
    /// Reaping also stops a finished child lingering as a zombie, which would otherwise make
    /// [`Launcher::liveness`] report it alive forever.
    /// Asking again reports the same status again.
    fn reap(&self, launched: Launched) -> Option<ExitStatus>;
}

/// What one non-blocking wait on a retained handle said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observed {
    Running,
    Finished(ExitStatus),
    /// The handle could not be waited on, so the platform's own answer has to decide.
    Unobservable,
}

/// Wait on a retained handle without blocking.
///
/// The standard library remembers a collected status inside the handle, so asking twice is safe
/// and the child is reaped by whichever call sees it finish first.
fn observe(child: &mut Child) -> Observed {
    match child.try_wait() {
        Ok(Some(status)) => Observed::Finished(status.into()),
        Ok(None) => Observed::Running,
        Err(_) => Observed::Unobservable,
    }
}

/// A child this process started, kept by its pid together with the identity that pid had.
#[derive(Debug)]
struct Tracked {
    /// The identity handed out at launch, so a pid the platform has since given to a later
    /// child of this same process is not mistaken for the earlier one.
    launched: Launched,
    state: State,
}

#[derive(Debug)]
enum State {
    /// The handle is kept while the child may still be running.
    Running(Child),
    /// Waited on already; the handle — and on some platforms the descriptor behind it — has been
    /// released, and the status stays for whoever asks, however often.
    Finished(ExitStatus),
}

impl Tracked {
    fn observe(&mut self) -> Observed {
        match &mut self.state {
            State::Finished(status) => Observed::Finished(*status),
            State::Running(child) => {
                let observed = observe(child);
                if let Observed::Finished(status) = observed {
                    self.state = State::Finished(status);
                }
                observed
            }
        }
    }
}

/// The real launcher: a detached child with its output redirected to files.
///
/// Keeps the handle of every child it started until that child has finished, and its status
/// after that, so exit status and termination work without platform process-table tricks for
/// as long as agentmux lives.
#[derive(Debug, Default)]
pub struct ProcessLauncher {
    children: Mutex<HashMap<u32, Tracked>>,
}

impl ProcessLauncher {
    /// A launcher tracking no children yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn children(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Tracked>> {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The retained entry for `launched`, if it is still the child that was launched.
    fn tracked(children: &mut HashMap<u32, Tracked>, launched: Launched) -> Option<&mut Tracked> {
        children
            .get_mut(&launched.pid)
            .filter(|tracked| tracked.launched == launched)
    }

    /// Wait on every retained child that has finished, so none lingers as a zombie.
    ///
    /// A consultation nobody reads again after its terminal event is never reaped by a fold, and
    /// a long-lived server would otherwise keep one defunct process, and one open handle, per
    /// consultation.
    fn collect_finished(&self) {
        for tracked in self.children().values_mut() {
            let _ = tracked.observe();
        }
    }
}

fn open_read(path: &Path) -> Result<File, LaunchError> {
    File::open(path).map_err(|source| LaunchError::Io {
        path: path.to_owned(),
        source,
    })
}

fn open_append(path: &Path) -> Result<File, LaunchError> {
    File::options()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| LaunchError::Io {
            path: path.to_owned(),
            source,
        })
}

impl Launcher for ProcessLauncher {
    fn launch(&self, spec: &LaunchSpec<'_>) -> Result<Launched, LaunchError> {
        self.collect_finished();
        if !spec.cwd.is_dir() {
            return Err(LaunchError::WorkingDirectory {
                path: spec.cwd.to_owned(),
            });
        }
        let stdin = open_read(spec.question)?;
        let stdout = open_append(spec.events)?;
        let stderr = open_append(spec.stderr)?;

        let spawn = |program: &str| -> Result<Child, LaunchError> {
            let clone = |file: &File, path: &Path| {
                file.try_clone().map_err(|source| LaunchError::Io {
                    path: path.to_owned(),
                    source,
                })
            };
            let mut command = std::process::Command::new(program);
            command
                .args(&spec.invocation.args)
                .current_dir(spec.cwd)
                .stdin(clone(&stdin, spec.question)?)
                .stdout(clone(&stdout, spec.events)?)
                .stderr(clone(&stderr, spec.stderr)?);
            // env_clear plus the allowlist the delegate built.
            // Never a denylist: the host that launched agentmux is itself an agent session, and
            // its environment carries that session's identity.
            command.env_clear();
            for (key, value) in &spec.invocation.env {
                command.env(key, value);
            }
            #[cfg(unix)]
            command.process_group(0);
            command.spawn().map_err(|source| LaunchError::Spawn {
                program: program.to_owned(),
                source,
            })
        };

        let program = spec.invocation.program;
        let child = match spawn(program) {
            Ok(child) => child,
            // An npm-installed CLI on Windows is a `.cmd` shim, which `Command` does not find
            // by its bare name.
            #[cfg(windows)]
            Err(LaunchError::Spawn { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                spawn(&format!("{program}.cmd")).map_err(|_| LaunchError::Spawn {
                    program: program.to_owned(),
                    source,
                })?
            }
            Err(error) => return Err(error),
        };
        let pid = child.id();
        // Read before the handle is put away, so a child that has already exited is still a
        // zombie holding its pid and its start time is still on record.
        let started = identity::start_time(pid);
        let launched = Launched {
            pid,
            process_group: if cfg!(unix) { Some(pid) } else { None },
            started,
        };
        self.children().insert(
            pid,
            Tracked {
                launched,
                state: State::Running(child),
            },
        );
        Ok(launched)
    }

    fn liveness(&self, launched: Launched) -> Liveness {
        let observed = Self::tracked(&mut self.children(), launched).map(Tracked::observe);
        match observed {
            Some(Observed::Running) => Liveness::Alive,
            Some(Observed::Finished(_)) => Liveness::Gone,
            None | Some(Observed::Unobservable) => identity::liveness(launched),
        }
    }

    fn terminate(&self, launched: Launched) {
        self.signal(launched, Signal::Terminate);
    }

    fn kill(&self, launched: Launched) {
        self.signal(launched, Signal::Kill);
    }

    fn reap(&self, launched: Launched) -> Option<ExitStatus> {
        // The status stays collectable, and the entry stays: two callers folding the same turn
        // must both see the same status, or the one that lost the race would settle the turn
        // as an exit of unknown status first and the other would adopt that.
        match Self::tracked(&mut self.children(), launched)?.observe() {
            Observed::Finished(status) => Some(status),
            Observed::Running | Observed::Unobservable => None,
        }
    }
}

/// Which of the two stop requests to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    /// Ask nicely; both CLIs exit on it.
    Terminate,
    /// Do not ask.
    Kill,
}

impl ProcessLauncher {
    /// Deliver a stop request to a child that is still this child.
    ///
    /// For a child whose handle is retained, the check and the signal happen under the one lock a
    /// concurrent `reap` would need to free the pid, so the pid cannot be recycled between them.
    /// After a restart there is no handle, and the recorded identity is the only check.
    fn signal(&self, launched: Launched, signal: Signal) {
        let mut children = self.children();
        match Self::tracked(&mut children, launched).map(|tracked| &mut tracked.state) {
            Some(State::Running(child)) => match observe(child) {
                Observed::Running => {
                    platform::signal(launched, Some(child), signal);
                    return;
                }
                Observed::Finished(_) => return,
                // A handle that cannot be waited on decides nothing; the process table does.
                Observed::Unobservable => {}
            },
            Some(State::Finished(_)) => return,
            None => {}
        }
        drop(children);
        if identity::liveness(launched) == Liveness::Alive {
            platform::signal(launched, None, signal);
        }
    }
}

/// Telling the launched child apart from whatever else has held its pid since.
///
/// The operating system's process table is the only witness that outlives agentmux, and it is
/// asked through a crate rather than through platform calls of our own so the answer is the same
/// shape on every release target and needs no `unsafe` here.
mod identity {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

    use super::{Launched, Liveness};

    /// The process table entry for one pid, freshly read.
    pub(super) fn lookup(pid: u32) -> Option<System> {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return None;
        }
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
            false,
            ProcessRefreshKind::nothing(),
        );
        Some(system)
    }

    /// When a process started, if it is in the table.
    pub(super) fn start_time(pid: u32) -> Option<u64> {
        lookup(pid)?
            .process(Pid::from_u32(pid))
            .map(sysinfo::Process::start_time)
    }

    /// The pid of a process's parent, if both are in the table.
    pub(super) fn parent_of(pid: u32) -> Option<u32> {
        lookup(pid)?
            .process(Pid::from_u32(pid))
            .and_then(sysinfo::Process::parent)
            .map(Pid::as_u32)
    }

    pub(super) fn liveness(launched: Launched) -> Liveness {
        let Some(system) = lookup(launched.pid) else {
            return Liveness::Unknown;
        };
        let Some(process) = system.process(Pid::from_u32(launched.pid)) else {
            return Liveness::Gone;
        };
        // A zombie has exited; it lingers only until a parent that is not us waits on it.
        if matches!(
            process.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        ) {
            return Liveness::Gone;
        }
        // The recorded start time is the identity; a pid handed to a different process since is
        // not this child, however much else it has in common.
        match launched.started {
            Some(started) if started == process.start_time() => Liveness::Alive,
            Some(_) => Liveness::Gone,
            None => platform::same_group(launched),
        }
    }

    #[cfg(unix)]
    mod platform {
        use nix::unistd::Pid;

        use super::{Launched, Liveness};

        /// With no start time on record, the process group is the next best witness: the child
        /// leads its own, and a recycled pid that does too is the residual hazard.
        pub(super) fn same_group(launched: Launched) -> Liveness {
            let Ok(pid) = i32::try_from(launched.pid) else {
                return Liveness::Unknown;
            };
            match nix::unistd::getpgid(Some(Pid::from_raw(pid))) {
                Ok(group) => match launched.process_group {
                    Some(expected) if i32::try_from(expected) != Ok(group.as_raw()) => {
                        Liveness::Gone
                    }
                    _ => Liveness::Alive,
                },
                Err(_) => Liveness::Gone,
            }
        }
    }

    #[cfg(not(unix))]
    mod platform {
        use super::{Launched, Liveness};

        pub(super) fn same_group(_launched: Launched) -> Liveness {
            Liveness::Alive
        }
    }
}

/// The pids of this process's ancestors, nearest first.
///
/// Read from the process table rather than from `getppid` alone, so a wrapper between a delegate
/// and the agentmux it started — a version manager's shim, a shell — does not hide the delegate.
/// The walk is bounded, and stops where the table stops answering.
#[must_use]
pub fn ancestors() -> Vec<u32> {
    const MAX_DEPTH: usize = 32;
    let mut chain = Vec::new();
    let Ok(current) = sysinfo::get_current_pid() else {
        return chain;
    };
    let mut pid = current.as_u32();
    while chain.len() < MAX_DEPTH {
        let Some(parent) = identity::parent_of(pid) else {
            break;
        };
        // pid 0 and pid 1 are the platform's own, and a chain that reaches them is complete; a
        // parent equal to the child would loop.
        if parent <= 1 || parent == pid || chain.contains(&parent) {
            break;
        }
        chain.push(parent);
        pid = parent;
    }
    chain
}

#[cfg(unix)]
mod platform {
    use std::process::Child;

    use nix::sys::signal::killpg;
    use nix::unistd::Pid;

    use super::{Launched, Signal};

    /// Signal the whole process group.
    ///
    /// A delegate CLI spawns tool subprocesses, and signalling only the leader would leave them
    /// running and still writing; the retained handle is not needed because the group is the
    /// address.
    pub(super) fn signal(launched: Launched, _child: Option<&mut Child>, signal: Signal) {
        let Some(group) = launched.process_group.and_then(|g| i32::try_from(g).ok()) else {
            return;
        };
        let signal = match signal {
            Signal::Terminate => nix::sys::signal::Signal::SIGTERM,
            Signal::Kill => nix::sys::signal::Signal::SIGKILL,
        };
        let _ = killpg(Pid::from_raw(group), signal);
    }
}

#[cfg(not(unix))]
mod platform {
    use std::process::Child;

    use sysinfo::Pid;

    use super::{Launched, Signal};

    /// Windows has no graceful signal; both requests terminate the process outright.
    ///
    /// Only the direct child is reached: nothing here creates a job object, so a tool
    /// subprocess the delegate started may outlive it.
    /// The handle is used while agentmux still holds it; afterwards the process is found by pid,
    /// which the caller has already verified is still this child.
    pub(super) fn signal(launched: Launched, child: Option<&mut Child>, _signal: Signal) {
        if let Some(child) = child {
            let _ = child.kill();
            return;
        }
        if let Some(system) = super::identity::lookup(launched.pid)
            && let Some(process) = system.process(Pid::from_u32(launched.pid))
        {
            let _ = process.kill();
        }
    }
}
