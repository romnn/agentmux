//! The run lock, and [`LockedRun`], the only way to change a run's shape.
//!
//! Several agentmux processes may address one consultation — an MCP server and a terminal, or a
//! server restarted underneath a `tail` — and every change to a run's shape is a check followed
//! by a write, which another process could act between.
//! So each one is a method of [`LockedRun`], which only [`RunStore::lock`] makes: folding a turn
//! (which may settle it), recovering a late reply, publishing the render, claiming a turn, marking
//! and recording a cancellation, and removing the run.
//! Nothing reachable from a [`LockedRun`] takes the lock again, because a second lock on the same
//! file from one process waits on the first for ever.

use std::fs::File;
use std::path::PathBuf;

use super::RunStore;
use super::error::RunError;
use super::id::RunId;

/// A consultation held under its lock.
///
/// Released when dropped.
pub(super) struct LockedRun<'s> {
    /// The store the run belongs to.
    pub(super) store: &'s RunStore,
    /// The run held.
    pub(super) run_id: RunId,
    #[expect(
        dead_code,
        reason = "never read: holding the open file is what holds the advisory lock, and dropping \
                  it releases the lock"
    )]
    lock: File,
}

impl RunStore {
    pub(super) fn lock_path(&self, run_id: &RunId) -> PathBuf {
        self.runs_dir().join(format!("{run_id}.lock"))
    }

    /// Take the consultation's lock, waiting for whoever holds it.
    ///
    /// The lock file sits beside the run directory rather than inside it, so removing the run
    /// removes nothing another process may be holding, and it is never unlinked: two processes
    /// locking two different inodes under one name would each believe they held the lock.
    /// It is an advisory lock the operating system releases when its holder dies, so a crashed
    /// holder leaves nothing to recover from.
    pub(super) fn lock(&self, run_id: &RunId) -> Result<LockedRun<'_>, RunError> {
        let path = self.lock_path(run_id);
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| RunError::Io {
                context: format!("opening {}", path.display()),
                source,
            })?;
        lock.lock().map_err(|source| RunError::Io {
            context: format!("locking {}", path.display()),
            source,
        })?;
        Ok(LockedRun {
            store: self,
            run_id: run_id.clone(),
            lock,
        })
    }
}
