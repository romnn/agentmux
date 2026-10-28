//! What can go wrong with a consultation, as callers see it.

use std::path::PathBuf;

use crate::config::BlockedModel;
use crate::delegate::{DelegateError, ModelId, Vendor};
use crate::launch::LaunchError;
use crate::transcript::{TurnState, describe_unsaved_session_store};

use super::id::{RunId, RunIdError};

/// Anything that can go wrong while running a consultation.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// No consultation with that id.
    #[error("no consultation with id {0}; call `list` to see recent ids")]
    NotFound(RunId),
    /// A run id was not of the accepted shape.
    #[error("{0}")]
    BadRunId(#[from] RunIdError),
    /// A delegate argument was rejected.
    #[error(transparent)]
    Delegate(#[from] DelegateError),
    /// The machine's account configuration could not be loaded.
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    /// agentmux is already running as somebody's delegate.
    ///
    /// Reachable only when a delegate inherited its account's MCP servers and agentmux is among
    /// them, which is exactly the case isolation would otherwise have prevented.
    #[error(
        "this agentmux is itself running inside a delegate, so it will not launch another one. A \
         delegate that inherits its account's settings also inherits its MCP servers; answer the \
         question you were asked instead."
    )]
    Recursive,
    /// The operator forbade this vendor on this server.
    ///
    /// A same-vendor delegate is a subagent the calling harness already spawns natively and
    /// supervises itself, so an operator registering agentmux inside one harness denies that
    /// harness's own vendor here.
    #[error(
        "this agentmux server does not launch `{vendor}` delegates: it was started with `--deny \
         {vendor}` because the harness it serves is itself `{vendor}` and spawns same-vendor \
         subagents natively. Consult the other vendor here, and use your own harness's subagents \
         for `{vendor}`."
    )]
    VendorDenied {
        /// The vendor that was asked for.
        vendor: Vendor,
    },
    /// The machine's configuration blocks the model a new consultation would launch.
    ///
    /// Refused rather than exchanged for the recommendation: which model answers decides what the
    /// caller pays and what it is told, so the caller makes that choice knowingly and asks again.
    #[error(
        "{vendor} model \"{model}\" is blocked by this machine's agentmux configuration; \
         {blocked}. No delegate was launched."
    )]
    ModelBlocked {
        /// Whose model it is.
        vendor: Vendor,
        /// The model that would have been launched, after the machine's rewrite.
        ///
        /// It is the one the caller asked for, because a file that rewrites another name onto a
        /// blocked model is refused when it is read.
        model: ModelId,
        /// What the configuration says instead: the recommendation, and why.
        blocked: BlockedModel,
    },
    /// A child could not be launched.
    #[error(transparent)]
    Launch(#[from] LaunchError),
    /// A run directory could not be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What agentmux was doing.
        context: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// A run's own metadata could not be parsed, so the run is unusable.
    #[error("consultation {run_id} has unreadable metadata: {source}")]
    Corrupt {
        /// Which run.
        run_id: RunId,
        /// The parse failure.
        #[source]
        source: serde_json::Error,
    },
    /// A record agentmux wrote itself cannot be parsed.
    ///
    /// Records are written whole, so this is a file that was edited by hand or damaged, and
    /// reading it as absent would make the turn it belongs to change shape.
    #[error("the record at {path} cannot be parsed: {source}")]
    CorruptRecord {
        /// The file.
        path: PathBuf,
        /// The parse failure.
        #[source]
        source: serde_json::Error,
    },
    /// The agentmux executable this store launches supervisors and drivers from was replaced
    /// after the store was given it, most likely by an upgrade this process outlived.
    ///
    /// A turn launched now would be run by a build that may not read what this one writes, and
    /// would fail only once started, so nothing is launched.
    #[error(
        "{path} changed after this agentmux started, most likely by an upgrade, so a turn it \
         launched now would be run by another build than the one that prepares it; nothing was \
         launched. Restart the agentmux MCP server, or whichever agentmux process this is, and \
         try again."
    )]
    ExecutableReplaced {
        /// The executable.
        path: PathBuf,
    },
    /// A turn's `settled.json` does not hold for the files it vouches for.
    ///
    /// The marker is the commitment every waiter returns on, so one that names a running turn or
    /// bytes that are not on disk is refused loudly rather than handed out.
    #[error("the settlement record at {path} is invalid: {reason}")]
    InvalidSettlement {
        /// The marker.
        path: PathBuf,
        /// What does not hold.
        reason: String,
    },
    /// A consultation has no turn with that number.
    #[error(
        "consultation {run_id} has no turn {turn}; it has {turns} turn(s), numbered from 1 as \
         the transcript's headings show them."
    )]
    NoSuchTurn {
        /// Which consultation.
        run_id: RunId,
        /// The one-based turn number asked for.
        turn: u32,
        /// How many turns it has.
        turns: u32,
    },
    /// A turn's supervisor left while starting its delegate, without recording one.
    ///
    /// A delegate may be running that nothing recorded, so the turn is neither settled, nor
    /// followed up, nor removed.
    #[error(
        "turn {turn} of consultation {run_id} cannot settle: its agentmux supervisor exited while \
         starting the delegate and recorded none, so whether a delegate is running is unknown. \
         Its files are in {dir}; check for a delegate process there before starting another \
         consultation."
    )]
    UncertainStart {
        /// Which consultation.
        run_id: RunId,
        /// The one-based turn number.
        turn: u32,
        /// The turn's directory.
        dir: PathBuf,
    },
    /// A follow-up was asked of a consultation that cannot take one.
    #[error("{0}")]
    NotResumable(#[from] NotResumable),
    /// A steer was refused, or could not be confirmed.
    #[error("{0}")]
    Steer(#[from] SteerError),
    /// MIGRATION, temporary: a consultation in the zero-based turn layout may still be running,
    /// so its turn directories cannot be renumbered yet; delete with
    /// `run/migrate_zero_based_turn_dirs.rs`.
    #[error(
        "consultation {run_id} may still be running under an earlier agentmux, which numbered its \
         turn directories from 0, so they cannot be renumbered yet. Let it finish, or stop it with \
         the agentmux that started it, then run this agentmux again."
    )]
    LegacyRunStillRunning {
        /// Which consultation.
        run_id: RunId,
    },
    /// Another caller claimed this turn first.
    #[error(
        "consultation {run_id} already has a turn {turn}, started by another caller. Only one \
         follow-up can be outstanding at a time; read the consultation again with `result`."
    )]
    TurnAlreadyClaimed {
        /// Which consultation.
        run_id: RunId,
        /// The one-based number of the turn that was already claimed.
        turn: u32,
    },
    /// The operation needs the consultation finished, and it is not.
    #[error(
        "consultation {run_id} is still running. Wait for it to settle with `wait`, or `cancel` it \
         first."
    )]
    StillRunning {
        /// Which consultation.
        run_id: RunId,
    },

    /// The state directory could not be located.
    #[error("cannot locate a state directory; set AGENTMUX_STATE_DIR")]
    NoStateDir,
}

impl RunError {
    pub(super) fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        move |source| Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// Why a consultation cannot take a follow-up.
///
/// Kept apart as variants because the caller does something different about each: wait, fix the
/// delegate arguments, or start over.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NotResumable {
    /// The newest turn has not finished, so there is nothing to continue yet.
    #[error(
        "consultation {run_id} is still running, so it has nothing to continue yet. Wait for it \
         to settle with `wait`, then ask the follow-up."
    )]
    StillRunning {
        /// Which consultation.
        run_id: RunId,
    },

    /// The delegate never announced a session, so there is nothing to reopen.
    #[error(
        "consultation {run_id} has no session to reopen: the delegate never announced one, which \
         usually means it failed before it started. Read its transcript for why, then start a new \
         consultation."
    )]
    NoSession {
        /// Which consultation.
        run_id: RunId,
    },

    /// The consultation ended without the delegate ever saying anything.
    #[error(
        "consultation {run_id} ended ({state}) without the delegate saying anything, so there is \
         no conversation to continue. Read its transcript for why, then start a fresh \
         consultation."
    )]
    NothingSaid {
        /// Which consultation.
        run_id: RunId,
        /// How it ended.
        state: TurnState,
    },

    /// The newest turn is over, but its delegate has not left yet, and both CLIs save the session
    /// a follow-up resumes on their way out.
    #[error(
        "consultation {run_id} has answered, but its delegate is still saving the session a \
         follow-up would resume. Ask the follow-up again in a few seconds."
    )]
    AwaitingDelegateExit {
        /// Which consultation.
        run_id: RunId,
    },

    /// The consultation was cancelled, so the delegate was stopped part-way through a turn.
    #[error(
        "consultation {run_id} was cancelled, so its delegate was stopped part-way through a \
         turn. Everything collected before the cancellation is still readable with `result`, but \
         continuing that turn would resume a delegate mid-thought. Start a new consultation."
    )]
    Cancelled {
        /// Which consultation.
        run_id: RunId,
    },

    /// The delegate could not save its session when the newest turn was launched.
    #[error(
        "consultation {run_id} cannot be continued: {}. Launching from an agent's sandboxed shell \
         does that, because the delegate inherits the sandbox; start a new consultation through \
         the agentmux MCP server, which runs outside the sandbox.",
        describe_unsaved_session_store(.store)
    )]
    SessionNotSaved {
        /// Which consultation.
        run_id: RunId,
        /// Where the session would have been saved.
        store: PathBuf,
    },
}

/// Why a message could not be steered into a consultation's running turn.
///
/// Kept apart as variants because each leaves the caller something different to do: ask a
/// follow-up instead, start the consultation differently, or read the transcript to see whether a
/// message whose delivery could not be confirmed arrived after all.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SteerError {
    /// The message has no content.
    #[error("the steer message is empty. Say what the delegate should do differently.")]
    Empty,

    /// No turn is running.
    #[error(
        "consultation {run_id} is not running ({state}), so there is no turn to steer. Ask the \
         next thing with `follow_up`, which continues the delegate's own session."
    )]
    NotRunning {
        /// Which consultation.
        run_id: RunId,
        /// How it stands.
        state: NotRunningReason,
    },

    /// The running turn was launched in a way that takes no input once it has started.
    #[error("consultation {run_id} cannot be steered: {reason}")]
    NotSteerable {
        /// Which consultation.
        run_id: RunId,
        /// Why its delegate offers no channel into a running turn.
        reason: UnsteerableReason,
    },

    /// The delegate CLI refused the message; it was not delivered.
    #[error(
        "the delegate refused the steer for consultation {run_id}: {detail}. The message was not \
         delivered and is not in the transcript."
    )]
    Rejected {
        /// Which consultation.
        run_id: RunId,
        /// The refusal, in the CLI's own words where it gave any.
        detail: String,
    },

    /// The turn ended, or its driver did not answer, before the message was picked up; it was
    /// withdrawn unsent.
    #[error(
        "the steer for consultation {run_id} was withdrawn and never sent: {why}. Read the result, \
         and ask it with `follow_up` if it still matters."
    )]
    Withdrawn {
        /// Which consultation.
        run_id: RunId,
        /// What happened instead.
        why: String,
    },

    /// The turn's driver is still opening its thread or starting the turn, and takes no message
    /// until the turn is running.
    #[error(
        "consultation {run_id} is still starting its turn, which takes no steer until it is \
         running. Steer it again in a few seconds."
    )]
    Starting {
        /// Which consultation.
        run_id: RunId,
    },

    /// The message was handed to the delegate CLI, which never said whether it took it.
    #[error(
        "the steer for consultation {run_id} was handed to the delegate, but whether it arrived \
         is unknown: {why}. Read the transcript: a delivered steer appears there as \
         `### user (steer)`. Do not send it again blindly."
    )]
    Unconfirmed {
        /// Which consultation.
        run_id: RunId,
        /// What happened instead of an answer.
        why: String,
    },
}

/// Why a consultation has no turn running to steer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotRunningReason {
    /// No turn has been launched yet.
    NotLaunched,
    /// A cancellation is stopping the turn.
    Cancelling,
    /// The turn is over.
    Ended(TurnState),
}

impl std::fmt::Display for NotRunningReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLaunched => f.write_str("not yet launched"),
            Self::Cancelling => f.write_str("cancelling"),
            Self::Ended(state) => state.fmt(f),
        }
    }
}

/// Why a running turn takes no message: its delegate reads its question once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsteerableReason {
    /// A Claude delegate, which runs in print mode.
    ClaudePrint,
    /// An inheriting Codex delegate this agentmux launched with `codex exec`.
    CodexExec,
    /// An isolated Codex delegate, which only `codex exec` can run.
    CodexIsolated,
}

impl std::fmt::Display for UnsteerableReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ClaudePrint => {
                "its delegate runs as `claude --print`, which reads its question once and offers \
                 no channel into a turn that has started"
            }
            Self::CodexExec => {
                "this agentmux launched it with `codex exec`, which reads its question once; Codex \
                 turns are driven over `codex app-server` only when agentmux runs with \
                 AGENTMUX_CODEX_LAUNCH=app-server, which is the default"
            }
            Self::CodexIsolated => {
                "it runs isolated, which takes `codex exec --ignore-user-config`, and `codex exec` \
                 reads its question once. `codex app-server`, the only Codex launch that can be \
                 steered, has no way to skip the account's own configuration, so only a \
                 consultation that loads it runs over it: one with `ignore_user_config: false`, \
                 which is the default unless this machine's agentmux.toml says otherwise, and \
                 which a follow-up may also ask for"
            }
        })
    }
}
