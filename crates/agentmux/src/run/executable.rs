//! The agentmux executable a store launches its supervisors and drivers from, and whether it is
//! still the build this process started with.
//!
//! A long-lived process, the MCP server above all, outlives an upgrade: the file at its
//! executable's path is replaced while it keeps running the old build.
//! A turn it launched then would be supervised or driven by the new build, which may not read the
//! plan the old one writes, and the launch fails only once the child has started.
//! The store remembers what the file was when it was given the path, and refuses to launch once it
//! has changed, so the caller learns at once to restart rather than from a failed turn.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::error::RunError;

/// An agentmux executable, as it was when a store was given it.
#[derive(Debug, Clone)]
pub(super) struct Executable {
    path: PathBuf,
    as_given: Option<FileStamp>,
}

/// What tells one build of a file from another without reading it: a replaced executable differs
/// in its size or its modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

impl Executable {
    pub(super) fn new(path: PathBuf) -> Self {
        let as_given = FileStamp::of(&path);
        Self { path, as_given }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Refuse to launch from a file that is no longer the one this store was given.
    ///
    /// A file that could not be read either time is taken as unchanged: nothing says it was
    /// replaced, and its launch fails on its own if it is gone.
    pub(super) fn refuse_if_replaced(&self) -> Result<(), RunError> {
        if FileStamp::of(&self.path) == self.as_given {
            Ok(())
        } else {
            Err(RunError::ExecutableReplaced {
                path: self.path.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::Executable;
    use crate::run::RunError;

    /// The file the store was given is launched from until something replaces it.
    #[gtest]
    fn a_replaced_executable_is_refused() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let path = dir.path().join("agentmux");
        std::fs::write(&path, "build one").or_fail()?;
        let executable = Executable::new(path.clone());
        assert_that!(executable.refuse_if_replaced(), ok(anything()));

        std::fs::write(&path, "build two, which is longer").or_fail()?;
        assert_that!(
            executable.refuse_if_replaced(),
            err(matches_pattern!(RunError::ExecutableReplaced {
                path: eq(&path)
            }))
        );
        Ok(())
    }
}
