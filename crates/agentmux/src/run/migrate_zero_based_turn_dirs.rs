//! MIGRATION, temporary: renumbers turn directories from the zero-based layout, whose first turn is
//! `turns/0000` and whose version 1 `settled.json` names a zero-based `turn_index`, to the one-based
//! one, whose first turn is `turns/0001` and whose version 2 `settled.json` names its `turn`.
//!
//! Delete this file and its two calls, in [`RunStore::open`] and [`RunStore::sweep`], once state
//! directories written before the change have been opened by a release that runs it.

use std::path::Path;

use super::RunStore;
use super::error::RunError;
use super::id::RunId;

impl RunStore {
    /// Renumber every run still in the zero-based layout whose turns have all ended.
    ///
    /// A run with a turn still running is left for an open after that turn ends, because its
    /// supervisor and driver hold the directory's old name and would write into another turn's.
    /// A run whose lock another process holds is left for a later open too, so opening the store
    /// never waits.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] when the runs directory cannot be listed or a run cannot be
    /// renumbered.
    pub(super) fn migrate_zero_based_turn_dirs(&self) -> Result<(), RunError> {
        let runs = self.runs_dir();
        let entries = match std::fs::read_dir(&runs) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(RunError::io(format!("listing {}", runs.display()))(err)),
        };
        for entry in entries {
            let entry = entry.map_err(RunError::io(format!("listing {}", runs.display())))?;
            let Some(run_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| RunId::parse(name).ok())
            else {
                continue;
            };
            let run_dir = entry.path();
            if !pending(&run_dir) {
                continue;
            }
            let Some(locked) = self.try_lock(&run_id)? else {
                continue;
            };
            let turns = run_dir.join("turns");
            renumber(&turns).map_err(RunError::io(format!("renumbering {}", turns.display())))?;
            drop(locked);
        }
        Ok(())
    }
}

/// Whether a run directory still has turns in the zero-based layout.
///
/// Such a run reads as having no turns until it is renumbered, so the retention sweep leaves it
/// alone rather than judge its age without them.
pub(super) fn pending(run_dir: &Path) -> bool {
    run_dir.join("turns").join("0000").is_dir()
}

/// Move each zero-based `turns/<n>` to `turns/<n + 1>`, highest first so no rename lands on
/// another, rewriting its `settled.json` before it moves.
///
/// The zero-based turns are the contiguous run from `0000`: a renumbering cut short leaves the
/// ones above the gap already moved, and the next one finishes it without moving them again.
fn renumber(turns: &Path) -> std::io::Result<()> {
    let dir = |index: u32| turns.join(format!("{index:04}"));
    let count = (0..u32::MAX)
        .take_while(|index| dir(*index).is_dir())
        .count();
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    // A turn with neither record may still be running.
    let all_ended = (0..count).all(|index| {
        let turn = dir(index);
        turn.join("settled.json").is_file() || turn.join("exit.json").is_file()
    });
    if !all_ended {
        return Ok(());
    }
    for index in (0..count).rev() {
        rewrite_settled(&dir(index).join("settled.json"))?;
        std::fs::rename(dir(index), dir(index.saturating_add(1)))?;
    }
    Ok(())
}

/// Rewrite a version 1 `settled.json`, which names a zero-based `turn_index`, as version 2, which
/// names the one-based `turn`.
fn rewrite_settled(path: &Path) -> std::io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let mut record: serde_json::Value = serde_json::from_str(&text)?;
    let Some(fields) = record.as_object_mut() else {
        return Ok(());
    };
    let Some(index) = fields.remove("turn_index").and_then(|index| index.as_u64()) else {
        return Ok(());
    };
    fields.insert("turn".to_owned(), index.saturating_add(1).into());
    fields.insert("version".to_owned(), 2.into());
    crate::atomic_file::write_whole(path, serde_json::to_string_pretty(&record)?.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use googletest::prelude::*;

    use super::renumber;

    /// A zero-based turn directory holding a settlement, as the zero-based layout wrote it.
    fn legacy_turn(turns: &Path, index: u32) -> Result<()> {
        let dir = turns.join(format!("{index:04}"));
        std::fs::create_dir_all(&dir).or_fail()?;
        std::fs::write(
            dir.join("settled.json"),
            format!(r#"{{"version":1,"turn_index":{index}}}"#),
        )
        .or_fail()
    }

    fn names(turns: &Path) -> Result<Vec<String>> {
        let mut names: Vec<String> = std::fs::read_dir(turns)
            .or_fail()?
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Ok(names)
    }

    /// Every turn moves up by one, and its settlement names the one-based number.
    #[gtest]
    fn a_settled_run_is_renumbered_from_one() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let turns = dir.path().join("turns");
        for index in 0..3 {
            legacy_turn(&turns, index)?;
        }

        renumber(&turns).or_fail()?;

        assert_that!(
            names(&turns)?,
            elements_are![eq("0001"), eq("0002"), eq("0003")]
        );
        let settled: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(turns.join("0002/settled.json")).or_fail()?,
        )
        .or_fail()?;
        assert_that!(settled, eq(&serde_json::json!({"version": 2, "turn": 2})));
        Ok(())
    }

    /// A renumbering cut short after moving the highest turn finishes without moving it again.
    #[gtest]
    fn a_renumbering_cut_short_is_finished_where_it_stopped() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let turns = dir.path().join("turns");
        for index in 0..2 {
            legacy_turn(&turns, index)?;
        }
        // The third turn was moved, and its settlement rewritten, before the cut.
        std::fs::create_dir_all(turns.join("0003")).or_fail()?;
        std::fs::write(turns.join("0003/settled.json"), r#"{"version":2,"turn":3}"#).or_fail()?;

        renumber(&turns).or_fail()?;

        assert_that!(
            names(&turns)?,
            elements_are![eq("0001"), eq("0002"), eq("0003")]
        );
        Ok(())
    }

    /// A turn with no record of its end may still be running, so the run is left as it is.
    #[gtest]
    fn a_run_with_a_turn_still_running_is_left_alone() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let turns = dir.path().join("turns");
        legacy_turn(&turns, 0)?;
        std::fs::create_dir_all(turns.join("0001")).or_fail()?;

        renumber(&turns).or_fail()?;

        assert_that!(names(&turns)?, elements_are![eq("0000"), eq("0001")]);
        Ok(())
    }
}
