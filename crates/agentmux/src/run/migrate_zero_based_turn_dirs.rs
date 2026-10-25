//! MIGRATION, temporary: renumbers turn directories from the zero-based layout, whose first turn is
//! `turns/0000`, to the one-based one, whose first turn is `turns/0001`.
//!
//! Delete this file and its one call in [`RunStore::open`] once state directories written before
//! the change have been opened by a release that runs it.

use std::path::Path;

use super::RunStore;
use super::id::RunId;

impl RunStore {
    /// Renumber every run whose first turn directory is still `turns/0000`.
    ///
    /// Best effort: a run that cannot be renumbered is logged and left as it was.
    pub(super) fn migrate_zero_based_turn_dirs(&self) {
        let Ok(entries) = std::fs::read_dir(self.runs_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let turns = entry.path().join("turns");
            let Some(run_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| RunId::parse(name).ok())
            else {
                continue;
            };
            if !turns.join("0000").is_dir() {
                continue;
            }
            // Under the run's lock, so no other process claims or folds a turn mid-rename.
            let locked = match self.lock(&run_id) {
                Ok(locked) => locked,
                Err(err) => {
                    tracing::warn!(?err, %run_id, "cannot lock a run to renumber its turns");
                    continue;
                }
            };
            renumber(&turns)
                .inspect_err(|err| {
                    tracing::warn!(?err, %run_id, "cannot renumber a run's turn directories");
                })
                .ok();
            drop(locked);
        }
    }
}

/// Move each `turns/<n>` to `turns/<n + 1>`, the highest first so no rename lands on another.
fn renumber(turns: &Path) -> std::io::Result<()> {
    // Another process may have renumbered it between the check and the lock.
    if !turns.join("0000").is_dir() {
        return Ok(());
    }
    let mut indices: Vec<u32> = std::fs::read_dir(turns)?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    indices.sort_unstable_by(|left, right| right.cmp(left));
    for index in indices {
        std::fs::rename(
            turns.join(format!("{index:04}")),
            turns.join(format!("{:04}", index.saturating_add(1))),
        )?;
    }
    Ok(())
}
