//! What agentmux writes about a consultation and its turns, and how it reads and writes it.
//!
//! Every record is written whole, so a reader never sees a partial one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::delegate::{Billing, Delegate, ModelId, SessionRef};
use crate::launch::{ExitStatus, Launched};

use super::error::RunError;
use super::id::RunId;
use super::layout::TurnDir;
use super::retention::Retention;

/// How a turn's delegate was launched.
///
/// Recorded per turn, because the mode is decided at each launch: a consultation begun from a
/// store that runs every Codex turn over `exec` can be followed up by one that drives it over the
/// app-server, and the thread carries over either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    /// The delegate CLI was run with the question on stdin: `claude --print` or `codex exec`.
    ///
    /// Nothing can reach the turn once it has started.
    #[default]
    Exec,
    /// The turn is driven over `codex app-server` by [`crate::appserver`], and can be steered.
    AppServer,
}

/// What agentmux recorded about a consultation when it began.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    /// The consultation's id.
    pub run_id: RunId,
    /// Who is being consulted.
    pub delegate: Delegate,
    /// Where the delegate is reading from.
    pub cwd: PathBuf,
    /// How long the consultation is kept.
    pub retention: Retention,
    /// When `start` was called.
    pub created_at: DateTime<Utc>,
    /// Extra environment the caller asked for, reapplied on every follow-up.
    ///
    /// Recorded so a later turn runs under the same conditions as the first; a follow-up that
    /// quietly dropped it would answer from a differently configured delegate.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The model the caller named, when the machine's configuration rewrote it to another.
    ///
    /// `delegate` names what ran, as it does for the account.
    /// The identifier that was asked for is kept beside it rather than thrown away, because a
    /// caller that pinned a model and reads a different one back has to be able to tell a rule of
    /// its own machine from agentmux ignoring the pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewritten_from: Option<ModelId>,
    /// The pricing the consultation may run on, decided when it started and asked for again by
    /// every follow-up.
    ///
    /// Pinned like the account, so a follow-up never runs on a pricing its first turn was not
    /// allowed: a checkout that selected the account, or a caller that asked for a subscription,
    /// keeps the consultation on one even after the files change.
    /// `None` only for a consultation recorded before agentmux kept it, whose account decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
    /// Whether every Codex turn runs with `codex exec`, because the caller asked or the store that
    /// started the consultation drives none, kept for the follow-ups.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub codex_exec: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct LaunchRecord {
    pub(super) launched: Launched,
    pub(super) started_at: DateTime<Utc>,
}

/// How a turn's child ended, as far as agentmux could tell.
///
/// Written once and never rewritten: whichever caller observes the departure first settles the
/// turn, and a second observation adopts that record rather than deriving its own.
/// Everything the settlement is derived from is in the record, so a later fold reads the record
/// and not the files it was derived from, which a stray descendant of the child may still be
/// writing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ExitRecord {
    /// The child was reaped, or observed gone.
    Exited {
        /// Absent when agentmux was not the child's parent by the time it ended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ExitStatus>,
        /// How much of the capture existed once the child had gone.
        events_len: u64,
        /// The end of what the child wrote to stderr, which is all the evidence a silent exit
        /// leaves.
        #[serde(default)]
        stderr_tail: String,
    },
    /// A caller stopped it.
    Cancelled {
        /// How much of the capture existed once the child had gone, or as it stood when the
        /// child could not be made to go.
        events_len: u64,
    },
}

impl ExitRecord {
    /// How much of the capture is part of the consultation; nothing past it is read.
    pub(super) fn events_len(&self) -> u64 {
        match self {
            Self::Exited { events_len, .. } | Self::Cancelled { events_len } => *events_len,
        }
    }
}

/// One turn's directory with the two records that decide whether it is a turn at all.
#[derive(Debug)]
pub(super) struct TurnFiles {
    pub(super) index: u32,
    pub(super) dir: TurnDir,
    pub(super) launch: Option<LaunchRecord>,
    pub(super) exit: Option<ExitRecord>,
}

/// What was tried, recorded before the spawn so a failed launch still leaves evidence.
///
/// Environment *keys* only.
/// The values are credentials.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct InvocationRecord {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) env_keys: Vec<String>,
    pub(super) cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resumed_from: Option<SessionRef>,
    /// Where the delegate CLI saves its sessions, when this process could not write there as the
    /// turn was launched, which means the child could not either.
    ///
    /// Probed at every launch, because each turn inherits the confinement of whoever launched it:
    /// a consultation begun through an unconfined server can be continued from a sandboxed shell.
    /// A store that becomes writable later still holds nothing to resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) unsaved_session_store: Option<PathBuf>,
    /// How the delegate was run, which decides whether the turn can be steered.
    #[serde(default)]
    pub(super) mode: LaunchMode,
    /// The pricing the delegate was launched on; absent from a record older than agentmux
    /// keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) billing: Option<Billing>,
    /// The vendor's configuration directory, as the environment the child got resolves it:
    /// where Codex writes the rollout a turn's usage window is recovered from.
    /// Recorded rather than re-derived, because a request's own environment can move it and
    /// is not kept; absent from a record older than agentmux keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) config_dir: Option<PathBuf>,
    /// The delegate CLI's version, as it printed it, for telling apart captures that different
    /// releases wrote; absent when the CLI could not say, and from a record older than agentmux
    /// keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cli_version: Option<String>,
    /// The version of agentmux that launched the turn, whose own capture format changes between
    /// releases too; absent from a record older than agentmux keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agentmux_version: Option<String>,
}

/// Write a record, replacing any there before, so that a reader never sees a partial one.
pub(super) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RunError> {
    let temp = write_json_temp(path, value)?;
    std::fs::rename(&temp, path).map_err(|source| {
        let _ = std::fs::remove_file(&temp);
        RunError::Io {
            context: format!("writing {}", path.display()),
            source,
        }
    })
}

/// Write a record once, and hand back whatever record is there afterwards.
///
/// When another caller got there first, its record is returned rather than overwritten, so two
/// observers of one departure settle on one story.
/// [`crate::once::create_once`] says how: on a filesystem without hard links a reader can glimpse
/// a partial record — loudly, as one that cannot be parsed, never quietly as one that is not
/// there.
pub(super) fn write_json_once<T: Serialize + for<'de> Deserialize<'de>>(
    path: &Path,
    value: &T,
) -> Result<T, RunError> {
    let text = serde_json::to_string_pretty(value).map_err(|source| RunError::Io {
        context: format!("serialising {}", path.display()),
        source: std::io::Error::other(source),
    })?;
    crate::once::create_once(path, text.as_bytes()).map_err(|source| RunError::Io {
        context: format!("writing {}", path.display()),
        source,
    })?;
    read_json(path)?.ok_or_else(|| RunError::Io {
        context: format!("reading back {}", path.display()),
        source: std::io::Error::from(std::io::ErrorKind::NotFound),
    })
}

/// Write a record's whole text to a temporary file beside `path`, and return where.
pub(super) fn write_json_temp<T: Serialize>(path: &Path, value: &T) -> Result<PathBuf, RunError> {
    let text = serde_json::to_string_pretty(value).map_err(|source| RunError::Io {
        context: format!("serialising {}", path.display()),
        source: std::io::Error::other(source),
    })?;
    let temp = crate::once::temporary(path);
    std::fs::write(&temp, text).map_err(|source| RunError::Io {
        context: format!("writing {}", temp.display()),
        source,
    })?;
    Ok(temp)
}

/// Read the complete, newline-terminated records of a capture file.
///
/// A live child is appending, so the file routinely ends mid-line and often mid-character.
/// Decoding the whole file as UTF-8 fails on that trailing fragment, and treating the failure as
/// "no events" would drop every complete message before it: the rendered transcript would shrink,
/// an outstanding cursor would be clamped backwards, and the messages would reappear once the
/// child finished the character.
/// Cutting at the last newline first means only complete records are decoded, and a complete
/// record from either CLI is valid UTF-8.
///
/// A file that is not there yet is empty; a file that cannot be read is an error, because reading
/// it as empty would make the transcript shrink with the failure and grow back after it.
///
/// With a `limit`, only that many bytes of the file are considered at all: it is the length a
/// settled turn's record froze, and it is applied to the raw bytes so that it means the same
/// thing whether or not the file decodes cleanly.
/// Returns the records and how many raw bytes they span, which is the length a record freezes:
/// a later read with that limit yields exactly these records, whatever was appended since.
pub(super) fn read_complete_records(
    path: &Path,
    limit: Option<u64>,
) -> Result<(String, u64), RunError> {
    let mut bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(RunError::Io {
                context: format!("reading {}", path.display()),
                source,
            });
        }
    };
    if let Some(limit) = limit {
        bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    bytes.truncate(end);
    let text = String::from_utf8(bytes)
        .unwrap_or_else(|invalid| String::from_utf8_lossy(invalid.as_bytes()).into_owned());
    Ok((text, u64::try_from(end).unwrap_or(u64::MAX)))
}

/// Read a record agentmux wrote, telling a record that is not there from one that cannot be read.
///
/// Records are written whole, so a file that is there is complete; one that cannot be parsed
/// was damaged or edited, and reading it as absent would change the shape of the turn it
/// belongs to.
pub(super) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, RunError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RunError::Io {
                context: format!("reading {}", path.display()),
                source,
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| RunError::CorruptRecord {
            path: path.to_owned(),
            source,
        })
}

/// The last `max_bytes` of a text file, or nothing for a file that is not there.
///
/// A file that cannot be read is an error rather than empty: this is the evidence a settlement
/// is recorded from, and recording "wrote nothing" over a transient failure would freeze the
/// wrong story.
pub(super) fn tail_of_file(path: &Path, max_bytes: usize) -> Result<String, RunError> {
    let text = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(RunError::Io {
                context: format!("reading {}", path.display()),
                source,
            });
        }
    };
    if text.len() <= max_bytes {
        return Ok(text);
    }
    let start = text.floor_char_boundary(text.len().saturating_sub(max_bytes));
    Ok(text.get(start..).unwrap_or_default().to_owned())
}
