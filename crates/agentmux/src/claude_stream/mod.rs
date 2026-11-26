//! Claude turns driven over stream-json input, so a running turn can be steered.
//!
//! Changes when Claude's stream-json input, or what agentmux asks of it, changes.
//!
//! The driver is the turn's delegate, `agentmux claude-stream <plan>` run where `claude --print`
//! would have run: its stdin is the question, its stdout the turn's `events.jsonl`.
//! It runs `claude` with the arguments `--print` would have had, plus
//! `--input-format stream-json --replay-user-messages`; ADR 0034 records why and how.
//!
//! ```text
//! driver                                  claude
//!   the question, uuid Q               ->
//!                                      <-  system/init        (the mailbox opens)
//!                                      <-  replay of Q        (dropped)
//!   a steer, uuid S, priority next     ->  one at a time, from the mailbox
//!                                      <-  replay of S        => agentmux.steer
//!                                      <-  result             (the mailbox closes, then stdin)
//!                                      <-  a further turn for a message sent before its result
//!                                          and taken after it, and that turn's result
//!   Claude exits                       =>  agentmux.end, with Claude's exit status
//! ```
//!
//! What the driver keeps true:
//!
//! - Every line Claude writes goes into the capture as it arrives, but for two replays: the
//!   question's is dropped, and a steer's becomes [`CaptureEvent::Steer`], where Claude took it.
//! - The mailbox ([`crate::appserver::mailbox`]) is served from `system/init` until the first
//!   `result`, one message at a time, so a message not yet claimed can still be withdrawn; a
//!   `result` already read closes it before another message is claimed.
//! - A claimed message is answered with what the driver knows: accepted once its replay is in the
//!   capture, rejected when it is known unsent, and unconfirmed otherwise.
//! - [`CaptureEvent::End`] is the capture's last line, written once Claude has exited and what it
//!   wrote before then has been read, unless the turn was cancelled, whose end the store records.
//! - A Claude the driver stopped is stopped with what ran below it then, and with what is left
//!   in the process group the driver leads.
//!
//! [`CaptureEvent::Steer`]: crate::stream::claude::CaptureEvent::Steer
//! [`CaptureEvent::End`]: crate::stream::claude::CaptureEvent::End

mod driver;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::delegate::Billing;
use crate::run::layout::TurnDir;

#[cfg(test)]
mod tests;

pub(crate) use driver::STOP_BUDGET;
pub use driver::drive;

/// The subcommand of the `agentmux` executable that runs [`drive`].
///
/// Hidden from its help: nothing but the run store launches it.
pub(crate) const SUBCOMMAND: &str = "claude-stream";

/// Everything a driver needs to run one Claude turn, written into the turn directory before the
/// launch.
///
/// The question is not here: it arrives on the driver's stdin, from the same file
/// `claude --print` would have read it from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DriverPlan {
    /// The `claude` executable, resolved against the driver's `PATH`.
    pub(crate) program: String,
    /// The arguments `claude --print` would have been launched with, to which the driver adds
    /// the ones that make Claude read its input as stream-json lines.
    pub(crate) args: Vec<String>,
    /// The directory the delegate reads the project from.
    pub(crate) cwd: PathBuf,
    /// The pricing the turn may run on.
    ///
    /// Under [`Billing::Subscription`] the driver scrubs Claude's environment again.
    pub(crate) billing: Billing,
    /// The turn's directory, which holds the mailbox and the cancel marker.
    pub(crate) turn_dir: PathBuf,
}

impl DriverPlan {
    /// Read a plan the run store wrote.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or is not a plan.
    pub(crate) fn read(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    fn turn(&self) -> TurnDir {
        TurnDir {
            root: self.turn_dir.clone(),
        }
    }

    /// The marker `cancel` creates, which makes the driver stop Claude.
    fn cancel_marker_path(&self) -> PathBuf {
        self.turn().cancelling()
    }

    /// The directory steered messages arrive in.
    fn mailbox_dir(&self) -> PathBuf {
        self.turn().steer_dir()
    }
}
