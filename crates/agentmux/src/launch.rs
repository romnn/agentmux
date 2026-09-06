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
//! # The one trait in the crate
//!
//! [`Launcher`] exists because tests need to exercise the whole transcript pipeline — including
//! the hook-reopen case — with no credentials and no network.
//! It is the seam that pays; everything else stays concrete.

use std::fs::File;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};

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
    /// Kept explicitly because it is what makes both liveness and cancellation safe across an
    /// agentmux restart: a recycled pid that is not in the group agentmux created is not this
    /// child.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_group: Option<u32>,
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

/// Whether a child is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The process exists and belongs to the group agentmux created for it.
    Alive,
    /// The process is gone.
    Gone,
    /// This platform cannot answer without holding the child handle.
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

    /// Collect the exit status of a child that has finished, without blocking.
    ///
    /// Only meaningful while agentmux is still the child's parent; after an agentmux restart the
    /// child has been reparented and this reports nothing.
    /// Reaping also stops a finished child lingering as a zombie, which would otherwise make
    /// [`Launcher::liveness`] report it alive forever.
    fn reap(&self, launched: Launched) -> Option<ExitStatus>;
}

/// The real launcher: a detached child with its output redirected to files.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessLauncher;

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
        let stdin = open_read(spec.question)?;
        let stdout = open_append(spec.events)?;
        let stderr = open_append(spec.stderr)?;

        let mut command = std::process::Command::new(spec.invocation.program);
        command
            .args(&spec.invocation.args)
            .current_dir(spec.cwd)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        // env_clear plus the allowlist the delegate built.
        // Never a denylist: the host that launched agentmux is itself an agent session, and its
        // environment carries that session's identity.
        command.env_clear();
        for (key, value) in &spec.invocation.env {
            command.env(key, value);
        }

        #[cfg(unix)]
        command.process_group(0);

        let child = command.spawn().map_err(|source| LaunchError::Spawn {
            program: spec.invocation.program.to_owned(),
            source,
        })?;
        let pid = child.id();

        // The child is detached, so nothing waits on it here.
        // On unix it is reparented to init when agentmux exits; while agentmux lives the zombie is
        // reaped by `reap`.
        Ok(Launched {
            pid,
            process_group: if cfg!(unix) { Some(pid) } else { None },
        })
    }

    #[cfg(unix)]
    fn liveness(&self, launched: Launched) -> Liveness {
        use nix::unistd::Pid;

        let Ok(pid) = i32::try_from(launched.pid) else {
            return Liveness::Unknown;
        };
        let pid = Pid::from_raw(pid);
        // A recycled pid is a real hazard for a run that outlives an agentmux restart.
        // The child leads its own process group, so requiring the group to match rules that out: a
        // different process would have to have been given the same pid *and* the same group.
        match nix::unistd::getpgid(Some(pid)) {
            Ok(group) => match launched.process_group {
                Some(expected) if i32::try_from(expected) != Ok(group.as_raw()) => Liveness::Gone,
                _ => Liveness::Alive,
            },
            Err(_) => Liveness::Gone,
        }
    }

    #[cfg(not(unix))]
    fn liveness(&self, _launched: Launched) -> Liveness {
        Liveness::Unknown
    }

    #[cfg(unix)]
    fn terminate(&self, launched: Launched) {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        if self.liveness(launched) != Liveness::Alive {
            return;
        }
        let Some(group) = launched.process_group.and_then(|g| i32::try_from(g).ok()) else {
            return;
        };
        // The whole group: a delegate CLI spawns tool subprocesses, and killing only the leader
        // leaves them running and still writing.
        let group = Pid::from_raw(group);
        let _ = killpg(group, Signal::SIGTERM);
    }

    #[cfg(not(unix))]
    fn terminate(&self, _launched: Launched) {}

    #[cfg(unix)]
    fn reap(&self, launched: Launched) -> Option<ExitStatus> {
        use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
        use nix::unistd::Pid;

        let pid = Pid::from_raw(i32::try_from(launched.pid).ok()?);
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => Some(ExitStatus {
                code: Some(code),
                signal: None,
            }),
            Ok(WaitStatus::Signaled(_, signal, _)) => Some(ExitStatus {
                code: None,
                signal: Some(signal as i32),
            }),
            _ => None,
        }
    }

    #[cfg(not(unix))]
    fn reap(&self, _launched: Launched) -> Option<ExitStatus> {
        None
    }
}
