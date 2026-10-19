//! Files created exactly once: the first writer's contents stay, and every later writer learns
//! that it lost.
//!
//! This is how two processes that race to decide one thing — which story a departed child's
//! exit record tells, whether a steer was claimed or withdrawn — agree without a lock.
//! A rename cannot arbitrate: on Windows `std::fs::rename` renames through a handle opened
//! beforehand, so racing renames of one file all succeed, each moving it on.
//!
//! A file only one writer writes is instead replaced whole by [`write_whole`], where a rename
//! has nothing to decide.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Create `path` holding `contents`, unless it already exists.
///
/// Returns `true` when this call created it and `false` when something else had.
/// The contents are written beside `path` and hard-linked into place, so a reader sees all of
/// them or no file at all.
/// A filesystem without hard links, such as FAT or exFAT, gets an exclusive create instead,
/// which still lets exactly one writer win but can show a reader a partial file for a moment.
///
/// # Errors
///
/// Returns the error of writing the contents, or of creating the file for a reason other than
/// its existing.
pub(crate) fn create_once(path: &Path, contents: &[u8]) -> std::io::Result<bool> {
    let temp = temporary(path);
    std::fs::write(&temp, contents)?;
    let placed = match std::fs::hard_link(&temp, path) {
        Err(err) if err.kind() != std::io::ErrorKind::AlreadyExists => {
            File::create_new(path).and_then(|mut file| {
                // A partial file left in place would be the decision for good, so a write that
                // fails takes back the file this call created.
                file.write_all(contents).inspect_err(|_| {
                    drop(file);
                    std::fs::remove_file(path)
                        .inspect_err(|err| {
                            tracing::warn!(
                                ?err,
                                path = %path.display(),
                                "cannot remove a partial file"
                            );
                        })
                        .ok();
                })
            })
        }
        linked => linked,
    };
    std::fs::remove_file(&temp)
        .inspect_err(|err| {
            tracing::warn!(?err, path = %temp.display(), "cannot remove a temporary file");
        })
        .ok();
    match placed {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err),
    }
}

/// Write `path` so that a reader sees what was there before or all of `contents`, never part.
///
/// The contents are written beside `path` and renamed over it.
/// A rename arbitrates nothing, as the module says, so this is for a file one writer owns.
///
/// # Errors
///
/// Returns the error of writing the contents or of renaming them into place.
pub(crate) fn write_whole(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let temp = temporary(path);
    let written = std::fs::write(&temp, contents).and_then(|()| std::fs::rename(&temp, path));
    if written.is_err() {
        std::fs::remove_file(&temp)
            .inspect_err(|err| {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(?err, path = %temp.display(), "cannot remove a temporary file");
                }
            })
            .ok();
    }
    written
}

/// A name beside `path` that nothing else uses: its file name with a unique `.tmp` suffix.
fn temporary(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::create_once;

    /// The first writer's contents stay; a second writer is told it lost and changes nothing.
    #[gtest]
    fn the_first_writer_wins() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let path = dir.path().join("decision.json");

        assert_that!(create_once(&path, b"first").or_fail()?, eq(true));
        assert_that!(create_once(&path, b"second").or_fail()?, eq(false));
        assert_that!(std::fs::read_to_string(&path).or_fail()?, eq("first"));
        // No temporary file is left behind either way.
        assert_that!(std::fs::read_dir(dir.path()).or_fail()?.count(), eq(1));
        Ok(())
    }
}
