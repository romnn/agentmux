//! What a turn left running when it ended, as the driver of a turn over `codex app-server`
//! found it and kept it in the turn's `left-running.json`.
//!
//! Changes when what is reported about leftover processes changes.
//!
//! A command a delegate starts in the background and does not wait for outlives the turn, and
//! agentmux stops it once the turn is over (ADR 0032,
//! `docs/adr/0032-leftover-processes-reported.md`).
//! Its work is then incomplete, and nothing the delegate said tells the caller so.
//! This record says which processes were still running once the server had exited, and whether
//! each still was after agentmux had signalled it, so the transcript and `status` can say it.
//!
//! The record names each process by pid, start time and process name, never by its arguments,
//! which can carry secrets.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The version of the record this agentmux writes, and the only one it reads.
pub const VERSION: u32 = 1;

/// How many processes a record names; the rest are counted in [`LeftRunning::omitted`].
pub const MAX_PROCESSES: usize = 50;

/// How many characters of a process name a record keeps.
pub const MAX_NAME_CHARS: usize = 64;

/// The processes a turn left running, and what became of each once agentmux signalled it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftRunning {
    /// The record's version, [`VERSION`].
    pub version: u32,
    /// The processes, those still running first, at most [`MAX_PROCESSES`] of them.
    pub processes: Vec<LeftProcess>,
    /// How many more there were, by what became of them.
    #[serde(default)]
    pub omitted: OmittedProcesses,
}

/// One process a turn left running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftProcess {
    /// Its pid.
    pub pid: u32,
    /// When it started, in seconds since the Unix epoch, which tells it apart from a later
    /// holder of its pid.
    pub start_time_secs: u64,
    /// Its process name, as [`bounded_name`] keeps it.
    pub name: String,
    /// What became of it.
    pub fate: ProcessFate,
}

/// What became of a process a turn left running, as agentmux last looked at it after
/// signalling it.
///
/// It says nothing about who stopped a process that is gone: it may have exited on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessFate {
    /// It was gone, or had exited and was waiting to be reaped.
    NoLongerRunning,
    /// It was still running.
    StillRunning,
}

/// How many processes a record counts without naming.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OmittedProcesses {
    /// Those no longer running.
    pub no_longer_running: u32,
    /// Those still running.
    pub still_running: u32,
}

impl LeftRunning {
    /// The record of `processes`, or `None` when there are none.
    ///
    /// Those still running are named first, in the order given otherwise, and those beyond
    /// [`MAX_PROCESSES`] are only counted.
    #[must_use]
    pub fn of(mut processes: Vec<LeftProcess>) -> Option<Self> {
        if processes.is_empty() {
            return None;
        }
        // Stable, so the order given survives within each fate.
        processes.sort_by_key(|process| process.fate != ProcessFate::StillRunning);
        let unnamed = processes.split_off(processes.len().min(MAX_PROCESSES));
        let mut omitted = OmittedProcesses::default();
        for process in &unnamed {
            let count = match process.fate {
                ProcessFate::NoLongerRunning => &mut omitted.no_longer_running,
                ProcessFate::StillRunning => &mut omitted.still_running,
            };
            *count = count.saturating_add(1);
        }
        Some(Self {
            version: VERSION,
            processes,
            omitted,
        })
    }

    /// How many processes the turn left running, named or not.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.named_count()
            .saturating_add(u64::from(self.omitted.no_longer_running))
            .saturating_add(u64::from(self.omitted.still_running))
    }

    /// How many of them were still running once agentmux had signalled them.
    #[must_use]
    pub fn still_running(&self) -> u64 {
        let named = self
            .processes
            .iter()
            .filter(|process| process.fate == ProcessFate::StillRunning)
            .count();
        u64::try_from(named)
            .unwrap_or(u64::MAX)
            .saturating_add(u64::from(self.omitted.still_running))
    }

    fn named_count(&self) -> u64 {
        u64::try_from(self.processes.len()).unwrap_or(u64::MAX)
    }

    /// Read the record a driver kept at `path`, or `None` when there is none.
    ///
    /// A record that cannot be read, is not one, or has another version is treated as absent:
    /// it adds a note to the transcript, and is no reason to fail reading the consultation.
    /// What it says is held to the bounds this agentmux writes, whoever wrote it.
    pub(crate) fn read(path: &Path) -> Option<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                tracing::debug!(?err, path = %path.display(), "cannot read a left-running record");
                return None;
            }
        };
        let record = match serde_json::from_str::<Self>(&text) {
            Ok(record) if record.version == VERSION => record,
            Ok(record) => {
                tracing::debug!(
                    version = record.version,
                    path = %path.display(),
                    "ignoring a left-running record of another version"
                );
                return None;
            }
            Err(err) => {
                tracing::debug!(?err, path = %path.display(), "ignoring a corrupt left-running record");
                return None;
            }
        };
        let Self {
            version,
            mut processes,
            mut omitted,
        } = record;
        for process in &mut processes {
            process.name = bounded_name(&process.name);
        }
        for process in processes.split_off(processes.len().min(MAX_PROCESSES)) {
            let count = match process.fate {
                ProcessFate::NoLongerRunning => &mut omitted.no_longer_running,
                ProcessFate::StillRunning => &mut omitted.still_running,
            };
            *count = count.saturating_add(1);
        }
        (!processes.is_empty() || omitted != OmittedProcesses::default()).then_some(Self {
            version,
            processes,
            omitted,
        })
    }

    /// What became of the processes, as a clause such as `2 are no longer running and 1 still
    /// is`.
    ///
    /// The one phrasing of the fates, so the transcript and every front end say the same thing.
    #[must_use]
    pub fn fates(&self) -> String {
        let count = self.count();
        let still = self.still_running();
        let gone = count.saturating_sub(still);
        match (count, still) {
            (1, 0) => "it is no longer running".to_owned(),
            (1, _) => "it is still running".to_owned(),
            (_, 0) => "none of them is still running".to_owned(),
            _ if gone == 0 => "all of them are still running".to_owned(),
            _ => format!(
                "{gone} {} no longer running and {still} still {}",
                is_or_are(gone),
                is_or_are(still)
            ),
        }
    }

    /// `1 process` or `3 processes`.
    #[must_use]
    pub fn processes_phrase(&self) -> String {
        let count = self.count();
        format!(
            "{count} {}",
            if count == 1 { "process" } else { "processes" }
        )
    }

    /// `it` or `them`, for the processes.
    #[must_use]
    pub fn pronoun(&self) -> &'static str {
        if self.count() == 1 { "it" } else { "them" }
    }
}

fn is_or_are(count: u64) -> &'static str {
    if count == 1 { "is" } else { "are" }
}

/// A process name as a record keeps it: at most [`MAX_NAME_CHARS`] characters, with control
/// characters and backticks escaped, so it reads as one name on one line, in a code span too.
#[must_use]
pub fn bounded_name(name: &str) -> String {
    let mut escaped = String::new();
    for character in name.chars() {
        if character == '`' {
            escaped.extend(character.escape_unicode());
        } else if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    if escaped.chars().count() <= MAX_NAME_CHARS {
        return escaped;
    }
    let mut kept: String = escaped
        .chars()
        .take(MAX_NAME_CHARS.saturating_sub(1))
        .collect();
    kept.push('…');
    kept
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::{
        LeftProcess, LeftRunning, MAX_NAME_CHARS, MAX_PROCESSES, OmittedProcesses, ProcessFate,
        bounded_name,
    };

    fn process(pid: u32, fate: ProcessFate) -> LeftProcess {
        LeftProcess {
            pid,
            start_time_secs: 1_700_000_000,
            name: format!("p{pid}"),
            fate,
        }
    }

    /// No process is no record; still-running processes are named first, and those past the cap
    /// are counted by fate.
    #[gtest]
    fn a_record_names_the_still_running_first_and_counts_the_rest() -> Result<()> {
        assert_that!(LeftRunning::of(Vec::new()), none());

        let mut processes: Vec<LeftProcess> = (0..60)
            .map(|pid| process(pid, ProcessFate::NoLongerRunning))
            .collect();
        processes.push(process(100, ProcessFate::StillRunning));
        let record = LeftRunning::of(processes).or_fail()?;

        assert_that!(record.processes, len(eq(MAX_PROCESSES)));
        assert_that!(
            record.omitted,
            eq(OmittedProcesses {
                no_longer_running: 11,
                still_running: 0,
            })
        );
        // The one still running leads, and the rest keep their order.
        assert_that!(
            record.processes.first().map(|process| process.pid),
            some(eq(100))
        );
        assert_that!(
            record.processes.get(1).map(|process| process.pid),
            some(eq(0))
        );
        assert_that!(record.count(), eq(61));
        assert_that!(record.still_running(), eq(1));
        assert_that!(
            record.fates(),
            eq("60 are no longer running and 1 still is")
        );
        Ok(())
    }

    /// Each fate reads correctly for one process and for several.
    #[gtest]
    fn fates_agree_in_number() -> Result<()> {
        use ProcessFate::{NoLongerRunning as Gone, StillRunning as Still};

        let phrase = |fates: &[ProcessFate]| -> Result<(String, String)> {
            let processes = fates
                .iter()
                .zip(1..)
                .map(|(fate, pid)| process(pid, *fate))
                .collect();
            let record = LeftRunning::of(processes).or_fail()?;
            Ok((record.processes_phrase(), record.fates()))
        };
        assert_that!(
            phrase(&[Gone])?,
            eq(&("1 process".to_owned(), "it is no longer running".to_owned()))
        );
        assert_that!(
            phrase(&[Still])?,
            eq(&("1 process".to_owned(), "it is still running".to_owned()))
        );
        assert_that!(
            phrase(&[Gone, Gone])?,
            eq(&(
                "2 processes".to_owned(),
                "none of them is still running".to_owned()
            ))
        );
        assert_that!(
            phrase(&[Still, Still])?,
            eq(&(
                "2 processes".to_owned(),
                "all of them are still running".to_owned()
            ))
        );
        assert_that!(
            phrase(&[Gone, Gone, Still])?,
            eq(&(
                "3 processes".to_owned(),
                "2 are no longer running and 1 still is".to_owned()
            ))
        );
        assert_that!(
            phrase(&[Gone, Still, Still])?,
            eq(&(
                "3 processes".to_owned(),
                "1 is no longer running and 2 still are".to_owned()
            ))
        );
        Ok(())
    }

    /// A name keeps to one line and one code span, and to its bound.
    #[gtest]
    fn a_name_is_escaped_and_bounded() {
        assert_that!(bounded_name("python3"), eq("python3"));
        assert_that!(bounded_name("a\nb`c"), eq("a\\nb\\u{60}c"));
        let long = "x".repeat(200);
        let bounded = bounded_name(&long);
        assert_that!(bounded.chars().count(), eq(MAX_NAME_CHARS));
        assert_that!(bounded, ends_with("…"));
    }

    /// A record read back is held to the bounds whoever wrote it, and one of another version or
    /// that does not parse is no record at all.
    #[gtest]
    fn a_record_read_back_is_bounded_and_versioned() -> Result<()> {
        let dir = tempfile::tempdir().or_fail()?;
        let path = dir.path().join("left-running.json");
        assert_that!(LeftRunning::read(&path), none());

        std::fs::write(&path, "{").or_fail()?;
        assert_that!(LeftRunning::read(&path), none());

        let mut record = LeftRunning::of(vec![process(1, ProcessFate::StillRunning)]).or_fail()?;
        record.version = 2;
        std::fs::write(&path, serde_json::to_string(&record).or_fail()?).or_fail()?;
        assert_that!(LeftRunning::read(&path), none());

        record.version = super::VERSION;
        record.processes = (0..70)
            .map(|pid| LeftProcess {
                name: format!("a\n{}", "y".repeat(100)),
                ..process(pid, ProcessFate::StillRunning)
            })
            .collect();
        std::fs::write(&path, serde_json::to_string(&record).or_fail()?).or_fail()?;
        let read = LeftRunning::read(&path).or_fail()?;
        assert_that!(read.processes.len(), eq(MAX_PROCESSES));
        assert_that!(read.omitted.still_running, eq(20));
        assert_that!(
            read.processes,
            each(field!(LeftProcess.name, starts_with("a\\n")))
        );
        assert_that!(read.still_running(), eq(70));
        Ok(())
    }
}
