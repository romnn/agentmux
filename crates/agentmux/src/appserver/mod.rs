//! Codex turns driven over `codex app-server`, so a running turn can be steered.
//!
//! Changes when the app-server protocol, or what agentmux asks of it, changes.
//!
//! `codex exec` takes its prompt once, on stdin, and offers nothing afterwards: a turn launched
//! that way cannot be told anything until it ends.
//! `codex app-server` speaks JSON-RPC on stdio and accepts `turn/steer` for a running turn, which
//! adds a user instruction the delegate sees before its next model call.
//! This module is agentmux's client of that protocol.
//!
//! # The driver is the delegate
//!
//! A JSON-RPC server needs a client holding its stdin for as long as the turn runs, and a
//! consultation must survive the agentmux that started it.
//! So the client is not the MCP server or the CLI process: it is a detached child of its own, the
//! *driver* — the `agentmux` executable run as `agentmux codex-app-server <plan>` — and it stands
//! in exactly where `codex exec` would.
//!
//! - It is launched by the same [`crate::launch::Launcher`], into the same turn directory, with
//!   the same environment, so the launch record, liveness, reaping, settlement and the recursion
//!   guard need nothing new.
//! - Its stdin is the turn's question file; its stdout is the turn's `events.jsonl`, written in
//!   the `codex exec --json` shape, so [`crate::stream::codex`] folds both launch modes and the
//!   rendered transcript does not depend on which one ran; its stderr, shared with the server it
//!   spawns, is the turn's `stderr.log`.
//! - It spawns `codex app-server` as its own child, in the driver's process group, so a driver the
//!   store has to signal takes the server with it, and writes the closing message to
//!   `last-message.md` as `--output-last-message` would.
//! - Two threads move the bytes — one reads the server's output, one writes its input — and the
//!   driver's own thread decides everything, so a server that stops reading cannot keep the driver
//!   from its deadlines or from a cancellation.
//! - When the turn is over it closes the server's input, gives the server a few seconds, and then
//!   stops whatever the server left running: codex's sandbox starts each command in a session of
//!   its own, which no signal to a process group reaches, so the driver reads the server's
//!   process tree while it still holds the server's input open, and signals what survives by pid
//!   and start time.
//!   While the turn runs it also records that tree every second, so a command a crashed server
//!   orphaned is still stopped; on Linux it adopts such orphans as well, and waits for each one
//!   that exits so none lingers in the process table.
//!   That is best effort: on macOS, a command a crashed server's command started after the last
//!   record is out of reach.
//!   On Windows the driver holds everything below it in a kill-on-close job object instead, and
//!   reads no process tree, because a recorded parent pid proves nothing there.
//!
//! # Protocol flow
//!
//! ```text
//! driver                                      codex app-server
//!   initialize, initialized               ->
//!   thread/start | thread/resume          ->  model, cwd, sandbox, approvalPolicy = never
//!                                         <-  thread id      => {"type":"thread.started",...}
//!   turn/start (the question)             ->
//!                                         <-  turn/started   => {"type":"turn.started"}
//!                                         <-  item/completed, thread/tokenUsage/...
//!                                             translated to exec events as they arrive
//!   turn/steer (from the mailbox)         ->  expectedTurnId = the running turn
//!                                         <-  ok             => {"type":"agentmux.steer",...}
//!   turn/interrupt (when `cancelling`)    ->
//!                                         <-  turn/completed => turn.completed | turn.failed
//!   close stdin, wait for exit, stop what it left running
//! ```
//!
//! One driver runs one turn and exits; a follow-up starts a new driver that resumes the thread
//! with `thread/resume` before `turn/start`.
//! Keeping a server alive between turns would buy nothing — the thread lives in Codex's own
//! rollout on disk, which is also what `codex exec resume` reads — and would leave a parked
//! process behind every kept consultation.
//! It also makes an agentmux restart a non-event: a running turn's driver is detached and keeps
//! going, and any later agentmux reaches it through files alone.
//! A server may start a turn, or finish it, before it answers `turn/start`; the turn is followed
//! from whichever names it first, and its end is remembered wherever it arrives and written once.
//! `turn.started` is written once the driver knows the turn, which is when it serves the
//! mailbox, so the store advertises a steer only from then.
//!
//! # Steering and cancelling go through files
//!
//! Whoever steers writes a request into the turn's [`mailbox`]; the driver claims it by renaming
//! it, sends `turn/steer`, and writes a reply beside it.
//! A caller that gives up first withdraws the request by renaming it too, and exactly one of the
//! two renames can succeed, so a message is either claimed or withdrawn and never sent.
//! A claimed message is answered with exactly what the driver knows:
//!
//! - accepted, once the server said so and the capture holds it as [`STEER_EVENT`], where it
//!   arrived and before the end of the turn, which the fold stops at;
//! - rejected, when the server refused it or it was never sent;
//! - unconfirmed, when it was sent and the server never said — the turn or the server ended
//!   first, or the capture could not record it — because codex may have taken it, and a caller
//!   told "refused" would send it again.
//!
//! `cancel` already marks a turn `cancelling` before it signals anything; the driver watches for
//! that marker, sends nothing new, interrupts a turn that has started with `turn/interrupt`,
//! waits for it to close, and exits without writing a terminal event, so the store records the
//! cancellation as it does for `exec`.
//! The store waits longer than the driver can take for all of that before it signals, and its
//! signals remain the fallback for a driver that does not leave.
//!
//! # Approvals
//!
//! A consultation runs with nobody to ask, so threads start with `approvalPolicy: never`, which is
//! what `codex exec` runs with: a command the sandbox refuses fails back to the model rather than
//! waiting on a person.
//! Every request the server can make of a client is answered at once: an approval or an input
//! request is declined with the answer its schema documents, noted in the transcript, and
//! anything else gets a JSON-RPC error.
//!
//! # Who pays
//!
//! Under [`crate::delegate::Billing::Subscription`] the environment the driver gives the server is
//! scrubbed again where the server is spawned, so no key exported anywhere upstream reaches it.
//! What codex's own configuration and login say is the user's to decide, as it is for `exec`.
//!
//! # What runs this way
//!
//! Only a Codex delegate that inherits its account's settings: see
//! [`crate::delegate::Delegate::app_server_settings`].
//! An isolated delegate's isolation is `codex exec --ignore-user-config`, and codex-cli 0.159.3's
//! app-server has no such flag; its own source keeps that loader override to `exec` and the TUI.
//! Running it with a private `CODEX_HOME` holding only the login instead was rejected, because
//! none of its hazards can be contained from outside codex:
//!
//! - a login kept in the operating system's keyring is filed under a hash of the canonical
//!   `CODEX_HOME` path (`compute_store_key` in `codex-rs/login`), so another directory has no
//!   login at all;
//! - a copied `auth.json` refreshes its own tokens, and codex treats a refresh token used twice as
//!   exhausted (`refresh_token_reused`), so the copy would log the user's own codex out;
//! - sessions and codex's SQLite state would live in the private directory, so the user's
//!   `codex resume`, and agentmux's own rate-limit reading, would not find the consultation.
//!
//! So isolation is never traded for steering silently: an isolated consultation runs over `exec`
//! and `steer` refuses it, naming `inherit_settings: true`, which is the explicit and documented
//! opt-in to running with the account's configuration.
//! `codex-app-server-codegen refresh` refuses a codex whose app-server accepts
//! `--ignore-user-config`, so the release that adds it is the one that changes this.
//! Everything else, and every Codex turn when the store was opened without a driver, runs over
//! `codex exec` and refuses `steer` with the reason.
//!
//! # Where the protocol comes from
//!
//! The payloads, the method bindings and the JSON-RPC envelope are
//! [`codex_app_server_protocol`]'s, which generates the first two from the JSON Schema the codex
//! CLI itself emits and says why codex's own protocol crate is not used instead.
//! What is written here is agentmux's side of the conversation: which requests it sends with
//! which settings, and that it declines whatever it is asked.
//!
//! What each notification becomes follows `codex exec`, which in that release is a client of
//! this same protocol; `translate.rs` names the reference.
//! A message the server sends that no longer matches the generated types, or whose method the
//! pinned schema never had, is recorded with its payload as drift in the transcript rather than
//! dropped, and the ids the setup answers name are still read, so a codex release that moves a
//! field agentmux does not use is loud without being fatal.
//!
//! [`STEER_EVENT`]: crate::stream::codex::STEER_EVENT

mod driver;
pub mod mailbox;
mod protocol;
mod translate;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::delegate::{AppServerSettings, Billing, SessionRef};

pub use driver::{Deadlines, Ending, STOP_BUDGET, drive, drive_with};
pub(crate) use protocol::initialize_params;
pub use translate::UNDECODED_EVENT;

/// What to do when a driven Codex turn fails or codex sends what agentmux cannot read: run the
/// consultation with `codex exec`, which does not depend on the app-server protocol.
///
/// The one phrasing the driver, the MCP server and the command line all give.
pub const EXEC_FALLBACK_HINT: &str = "if the installed codex no longer speaks the app-server \
    protocol this agentmux expects, run the Codex consultation with `codex exec` instead: \
    `codex_exec: true` on the MCP `start` or `ask` tool, `--codex-launch exec` on the command \
    line, or AGENTMUX_CODEX_LAUNCH=exec for every consultation an MCP server starts";

/// The subcommand of the `agentmux` executable that runs [`drive`].
///
/// Hidden from its help: nothing but the run store launches it.
pub const SUBCOMMAND: &str = "codex-app-server";

/// Everything a driver needs to run one turn, written into the turn directory before the launch.
///
/// The question is not here: it arrives on the driver's stdin, from the same file `codex exec`
/// would have read it from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// What starts the server, and the model and sandbox its thread runs under.
    pub settings: AppServerSettings,
    /// The directory the delegate reads the project from.
    pub cwd: PathBuf,
    /// The thread to continue, for a follow-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<SessionRef>,
    /// The pricing the turn may run on.
    ///
    /// Under [`Billing::Subscription`] the driver scrubs the server's environment again.
    pub billing: Billing,
    /// Where the closing message is written, as `codex exec --output-last-message` would.
    pub last_message: PathBuf,
    /// The marker `cancel` creates, which makes the driver interrupt the turn.
    pub cancel_marker: PathBuf,
    /// The directory steered messages arrive in.
    pub mailbox: PathBuf,
}

impl Plan {
    /// Read a plan the run store wrote.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or is not a plan.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }
}
