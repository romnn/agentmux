//! Where a consultation and its turns live on disk.

use std::path::{Path, PathBuf};

use crate::appserver::mailbox::Mailbox;

use super::RunStore;
use super::error::RunError;
use super::id::RunId;

/// Where one turn's files live, and what agentmux knows about its child.
///
/// The one place a turn's file names are spelled; the driver of a turn over the app-server reads
/// its files through this too.
#[derive(Debug, Clone)]
pub(crate) struct TurnDir {
    pub(crate) root: PathBuf,
}

impl TurnDir {
    pub(super) fn question(&self) -> PathBuf {
        self.root.join("question.md")
    }
    pub(super) fn events(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }
    pub(super) fn stderr(&self) -> PathBuf {
        self.root.join("stderr.log")
    }
    pub(crate) fn last_message(&self) -> PathBuf {
        self.root.join("last-message.md")
    }
    /// The reply recovered from the CLI's own output file, kept once it has been rendered.
    pub(super) fn recovered(&self) -> PathBuf {
        self.root.join("recovered.md")
    }
    pub(super) fn launch(&self) -> PathBuf {
        self.root.join("launch.json")
    }
    pub(super) fn invocation(&self) -> PathBuf {
        self.root.join("invocation.json")
    }
    /// Where the usage window recovered from a vendor's own session record is kept.
    ///
    /// Cached because finding it means searching the vendor's session tree, and a fold runs on
    /// every `status` and every page of a `tail`.
    pub(super) fn rate_limit(&self) -> PathBuf {
        self.root.join("rate-limit.json")
    }
    pub(super) fn exit(&self) -> PathBuf {
        self.root.join("exit.json")
    }
    /// Present from the moment a cancellation was asked for.
    ///
    /// Whoever then observes the child gone — this caller, or a concurrent `status` — records
    /// the departure as a cancellation rather than as a death, so the two cannot disagree.
    /// A turn driven over the app-server also watches for it, and interrupts its turn when it
    /// appears.
    pub(crate) fn cancelling(&self) -> PathBuf {
        self.root.join("cancelling")
    }
    /// The plan a turn driven over the app-server runs under.
    pub(super) fn app_server_plan(&self) -> PathBuf {
        self.root.join("app-server.json")
    }
    /// Where steered messages wait for the driver of a turn driven over the app-server.
    pub(crate) fn steer(&self) -> PathBuf {
        self.root.join("steer")
    }
    pub(super) fn mailbox(&self) -> Mailbox {
        Mailbox::new(self.steer())
    }
}

impl RunStore {
    pub(super) fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    pub(super) fn run_dir(&self, run_id: &RunId) -> PathBuf {
        self.runs_dir().join(run_id.as_str())
    }

    pub(super) fn turn_dir(&self, run_id: &RunId, index: u32) -> TurnDir {
        TurnDir {
            root: self
                .run_dir(run_id)
                .join("turns")
                .join(format!("{index:04}")),
        }
    }

    pub(super) fn transcript_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join("transcript.md")
    }

    /// Every turn directory of a run, in order, ending at the first index that has none.
    pub(super) fn turn_dirs(&self, run_id: &RunId) -> impl Iterator<Item = (u32, TurnDir)> {
        (0..u32::MAX)
            .map(move |index| (index, self.turn_dir(run_id, index)))
            .take_while(|(_, dir)| dir.root.is_dir())
    }
}

/// Claim a directory, failing if it — or the directory it belongs in — already exists or does
/// not.
///
/// Never creates the parent: a turn directory whose run has been removed must not come back as
/// an orphan holding a running child.
pub(super) fn reserve_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)?;
    restrict(path);
    Ok(())
}

pub(super) fn create_private_dir(path: &Path) -> Result<(), RunError> {
    std::fs::create_dir_all(path).map_err(RunError::io(format!("creating {}", path.display())))?;
    restrict(path);
    Ok(())
}

/// Keep a run directory readable only by its owner.
///
/// Transcripts contain whatever the caller asked about, which is often a private codebase.
pub(super) fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // A filesystem without Unix permissions refuses this, and refusing the store on it
        // would refuse every consultation, so the failure is said rather than returned.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .inspect_err(|err| {
                tracing::warn!(?err, path = %path.display(), "cannot make a run directory private");
            })
            .ok();
    }
    #[cfg(not(unix))]
    let _ = path;
}
