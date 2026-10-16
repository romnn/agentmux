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
    pub log: Option<PathBuf>,
    /// Where the fake writes its environment, `NAME=value` per line, when it starts.
    pub environment: Option<PathBuf>,
    /// Where every command a turn starts appends its pid, once it is running as the scenario
    /// says — in a session of its own, for one that should be.
    pub commands: PathBuf,
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
    /// The server answers setup; on `turn/start` it starts a command, waits for `release` to
    /// exist, and then dies as `death` says.
    Dying {
        /// How the server dies.
        death: Death,
        /// The file whose existence lets it.
        release: PathBuf,
    },
    /// The turn's command forks a helper and exits, so the helper outlives its parent; the helper
    /// exits once `helper_release` exists, and the turn completes once `release` does.
    Forking {
        /// The file that completes the turn.
        release: PathBuf,
        /// The file that lets the helper exit.
        helper_release: PathBuf,
    },
}

/// How a [`Scenario::Dying`] server dies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Death {
    /// It never answers `turn/start` and exits with a failure; its command runs in a session of
    /// its own.
    Crash,
    /// It answers, and exits, leaving its command holding the server's output open.
    Linger,
    /// It answers, and exits, leaving a command that keeps writing valid notifications to the
    /// server's output.
    Chatter,
}
