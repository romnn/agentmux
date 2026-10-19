//! What the `codex` binary of this crate plays, and where it reports it.
//!
//! A test writes a [`Config`] as JSON to [`CONFIG_FILE`] in codex's home — `$CODEX_HOME`, or
//! `.codex` in the home directory — where codex keeps its own configuration, and puts the binary
//! on the client's `PATH` as `codex`.
//! That reaches the fake however many processes stand between them, as long as each passes on the
//! variables codex itself reads, which a client that keeps a delegate's environment to an
//! allowlist does.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The file in codex's home holding the fake's [`Config`], as JSON.
pub const CONFIG_FILE: &str = "app-server-mock.json";

/// The first argument of the fake run in a helper role, which every command the fake starts
/// carries, so a test can tell those commands from anything else.
pub const HELPER: &str = "__codex-app-server-mock-helper";

/// What the fake does, and where it reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The scenario.
    pub scenario: Scenario,
    /// Where every message the client sends is appended, one per line.
    pub client_messages_path: Option<PathBuf>,
    /// Where the fake writes its environment, `NAME=value` per line, when it starts.
    pub environment_dump_path: Option<PathBuf>,
    /// Where every command a turn starts appends its pid, once it is running as the scenario
    /// says — in a session of its own, for one that should be.
    pub command_pids_path: PathBuf,
}

/// How the fake server behaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Scenario {
    /// The turn starts a command in a session of its own and runs until steered, which completes
    /// it with a reply, or interrupted, which ends it as codex does; a steer that names another
    /// turn is refused, as codex refuses it.
    ///
    /// The command is left running however the turn ends, as codex-cli 0.159.3 was seen to leave
    /// one on an interrupt.
    Steerable,
    /// The server answers setup; on `turn/start` it starts a command, waits for
    /// `exit_trigger_path` to exist, and then exits as `exit_mode` says.
    ServerExit {
        /// How the server dies.
        exit_mode: ServerExitMode,
        /// The file whose existence lets it.
        exit_trigger_path: PathBuf,
    },
    /// The turn's command forks a helper and exits, so the helper outlives its parent; the helper
    /// exits once `helper_exit_trigger_path` exists, and the turn completes once
    /// `completion_trigger_path` does.
    OrphanedHelper {
        /// The file that completes the turn.
        completion_trigger_path: PathBuf,
        /// The file that lets the helper exit.
        helper_exit_trigger_path: PathBuf,
    },
}

/// How a [`Scenario::ServerExit`] server dies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerExitMode {
    /// It never answers `turn/start` and exits with a failure; its command runs in a session of
    /// its own.
    Crash,
    /// It answers, and exits, leaving its command holding the server's output open.
    LeaveOutputOpen,
    /// It answers, and exits, leaving a command that keeps writing valid notifications to the
    /// server's output.
    LeaveWarningWriter,
}
