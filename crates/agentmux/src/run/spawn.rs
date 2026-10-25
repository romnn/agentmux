//! Starting a consultation and continuing it: claiming a turn and spawning its delegate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::config::{RefusedModel, ResolvedModel};
use crate::delegate::{
    AccountChoice, AppServerSettings, Billing, Delegate, DelegateError, Isolation, ModelId,
    PermissionOptions, SessionRef, TurnPlan, Vendor, codex_supports_permission_profiles,
};
use crate::launch::{LaunchSpec, Liveness};
use crate::transcript::Outcome;

use super::auto_resume::AutoResumePlan;
use super::error::{NotResumable, RunError};
use super::id::RunId;
use super::layout::{TurnDir, create_private_dir, reserve_dir};
use super::lock::LockedRun;
use super::records::{
    ExitRecord, InvocationRecord, LaunchMode, LaunchRecord, Meta, ProducerCommand, SettledTurn,
    SupervisorPhase, SupervisorPlan, SupervisorRecord, read_json, write_json,
};
use super::retention::Retention;
use super::status::RunStatus;
use super::{RunState, RunStore};

/// What `start` was asked for.
#[derive(Debug, Clone)]
pub struct StartRequest {
    /// Who to consult, and on what terms.
    pub delegate: Delegate,
    /// The question, self-contained: the delegate starts with no conversation context.
    pub question: String,
    /// The directory the delegate reads the project from.
    ///
    /// Made absolute before it is recorded, because the record outlives the process that made it
    /// and a later turn must read the same directory.
    pub cwd: PathBuf,
    /// How long to keep the consultation after its delegate last wrote anything.
    pub retention: Retention,
    /// Extra environment for the delegate, applied over the machine and account layers.
    pub env: BTreeMap<String, String>,
    /// The pricing the caller asks for, or `None` to let the account decide.
    ///
    /// [`Billing::Api`] is refused unless the machine's configuration opts the account in, and
    /// whatever the first turn runs on is pinned for the consultation's follow-ups.
    pub billing: Option<Billing>,
    /// Run every Codex turn of the consultation with `codex exec`, even where this store would
    /// drive it over `codex app-server`.
    ///
    /// For a codex whose app-server protocol no longer matches this agentmux; such a turn cannot
    /// be steered.
    pub force_codex_exec: bool,
}

/// One turn to launch: as which delegate, at which index, asking what, continuing which session.
#[derive(Debug, Clone, Copy)]
pub(super) struct NextTurn<'a> {
    /// What the turn runs as, held to the machine file's limits by whoever admitted it.
    pub(super) delegate: &'a Delegate,
    /// The index the turn claims.
    pub(super) index: u32,
    /// The question, which becomes the child's stdin.
    pub(super) question: &'a str,
    /// The delegate's own session to continue, for a follow-up.
    pub(super) resume: Option<&'a SessionRef>,
}

/// How a turn's child process is built.
#[derive(Debug, Clone, Copy)]
pub(super) enum Resolution<'a> {
    /// From this store's environment and the machine's configuration as it reads now, for every
    /// turn a caller asks for.
    Configured(&'a crate::config::Config),
    /// In this process's own environment, for an automatic resume made by the supervisor of the
    /// turn it continues.
    ///
    /// That supervisor was started with the environment resolved for the turn it supervises,
    /// and holds neither the host environment nor the configuration it was resolved from, so
    /// the resumed turn runs in that environment unchanged.
    Inherited(Inherited),
}

impl Resolution<'_> {
    /// Build `delegate`'s child for `plan`, from `host_env`, the environment the store was opened
    /// with.
    fn resolve(
        self,
        delegate: &Delegate,
        plan: &TurnPlan<'_>,
        host_env: &BTreeMap<String, String>,
    ) -> Result<Resolved, RunError> {
        Ok(match self {
            Self::Configured(config) => Resolved {
                invocation: delegate.invocation(plan, host_env, config)?,
                app_server_settings: delegate.app_server_settings(config),
                auto_resume_attempt: None,
            },
            Self::Inherited(Inherited { billing, attempt }) => Resolved {
                invocation: delegate.continuation(plan, host_env.clone(), billing),
                app_server_settings: delegate
                    .app_server_settings_under(delegate.isolation().unwrap_or(Isolation::Isolated)),
                auto_resume_attempt: Some(attempt),
            },
        })
    }
}

/// A turn's child as a [`Resolution`] built it.
struct Resolved {
    invocation: crate::delegate::Invocation,
    /// How it runs over `codex app-server`, when it can.
    app_server_settings: Option<AppServerSettings>,
    /// Which automatic resume it is, when it is one.
    auto_resume_attempt: Option<u32>,
}

/// What an automatic resume carries over from the turn it continues.
#[derive(Debug, Clone, Copy)]
pub(super) struct Inherited {
    /// The pricing the environment was resolved for.
    pub(super) billing: Billing,
    /// Which automatic resume in a row this is, counted from 1.
    pub(super) attempt: u32,
}

/// What the machine's configuration decided about a requested delegate.
///
/// A named shape rather than a pair: both halves name a model, and nothing about a positional
/// pair would say which of the two ran.
pub(super) struct Pinned {
    /// The delegate as it will be launched and recorded.
    delegate: Delegate,
    /// The model the caller asked for, when that is not the one in `delegate`.
    rewritten_from: Option<ModelId>,
}

impl RunStore {
    /// Whether this process was started by a delegate agentmux launched.
    ///
    /// Two witnesses, because neither reaches everywhere.
    /// The environment marker every delegate carries is passed on by Claude Code and by both
    /// CLIs' shells, but Codex starts its MCP servers with an environment of its own choosing
    /// that does not include it.
    /// Measured, that environment holds only `HOME`, `LANG`, `LC_ALL`, `LOGNAME`, `PATH`, `PWD`,
    /// `SHELL`, `TERM`, `TMPDIR` and `USER`, in a fresh process group.
    /// The process tree is the other witness: if any ancestor of this process, within the bounded
    /// walk [`crate::launch::ancestors`] makes, is a delegate this store launched whose turn has
    /// no exit record and that is still running, this process is inside it, however many shells
    /// or shims sit in between.
    ///
    /// Decided once per store.
    /// The marker comes from the captured environment, and an ancestor cannot become a delegate
    /// after the fact, so the answer cannot change while the store lives.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] or [`RunError::CorruptRecord`] when the store cannot be read,
    /// because a guard that read an unreadable record as "no delegate here" would be blind in
    /// exactly the case it exists for.
    pub fn running_inside_a_delegate(&self) -> Result<bool, RunError> {
        if let Some(known) = self.inside_delegate.get() {
            return Ok(*known);
        }
        // Exported-but-empty means unset, as it does for every other agentmux variable: a shell
        // profile that exports it unconditionally must not refuse every launch.
        let inside = self
            .host_env
            .get(crate::delegate::DELEGATE_MARKER)
            .is_some_and(|value| !value.is_empty())
            || self.an_ancestor_is_a_live_delegate()?;
        // A concurrent caller that decided first decided the same, from the same environment
        // and process tree.
        self.inside_delegate.set(inside).ok();
        Ok(inside)
    }

    pub(super) fn an_ancestor_is_a_live_delegate(&self) -> Result<bool, RunError> {
        let ancestors = crate::launch::ancestors();
        if ancestors.is_empty() {
            return Ok(false);
        }
        for run_id in self.run_ids()? {
            let live = self
                .turn_files(&run_id)?
                .into_iter()
                // A pid alone is not a witness, because pids are recycled.
                // The record has to name the process's start time, be of a turn nobody has
                // settled, and describe a process that is still the one recorded.
                .filter(|turn| turn.exit.is_none())
                .filter_map(|turn| turn.launch)
                .filter(|record| record.launched.started.is_some())
                .filter(|record| ancestors.contains(&record.launched.pid))
                .any(|record| self.launcher.liveness(record.launched) == Liveness::Alive);
            if live {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Begin a consultation.
    ///
    /// Returns as soon as the child is running, not when it answers.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the configuration cannot be read, agentmux is itself running
    /// inside a delegate, the run directory cannot be created, or the delegate cannot be launched
    /// — which includes a working directory that is not there.
    /// Returns [`RunError::ModelBlocked`] when the machine's configuration blocks the model that
    /// would run, and [`RunError::Delegate`] with [`DelegateError::Permission`] when the
    /// isolation or a permission exceeds the machine file's limits, both before anything is
    /// written or launched; a follow-up is never refused for a block added after its
    /// consultation started, but is for a limit tightened since.
    /// A start that fails before its turn is recorded leaves no run directory behind.
    /// Under a supervisor, a delegate that then cannot be started is a failed turn, kept with the
    /// reason.
    pub fn start(&self, request: &StartRequest) -> Result<RunStatus, RunError> {
        // Everything that can be decided without touching the store is decided first, so a
        // refused request leaves no half-made consultation for `list` to report as running.
        self.refuse_to_spawn(request.delegate.vendor())?;
        let cwd = std::path::absolute(&request.cwd)
            .map_err(RunError::io("resolving the working directory"))?;
        let config = crate::config::Config::load(&self.host_env, &cwd)?;

        // Resolved now rather than at each launch, so the record names the identity that
        // actually ran and every later turn resumes as the same one.
        // A configuration edited mid-consultation must not silently move a follow-up to
        // another account, whose session it would then fail to resume.
        let Pinned {
            delegate,
            rewritten_from,
        } = Self::pin_defaults(&request.delegate, &config)?;
        let billing = delegate.resolve_billing(request.billing, &self.host_env, &config)?;
        let (run_id, dir) = self.create_run_dir()?;
        let meta = Meta {
            env: request.env.clone(),
            run_id: run_id.clone(),
            delegate,
            rewritten_from,
            billing: Some(billing),
            // A store that drives nothing runs Codex with `exec`, and says so for the follow-ups,
            // whichever store continues the consultation.
            force_codex_exec: request.force_codex_exec || self.codex_driver.is_none(),
            cwd,
            retention: request.retention,
            created_at: Utc::now(),
        };
        let started = write_json(&dir.join("meta.json"), &meta)
            .and_then(|()| self.lock(&run_id))
            .and_then(|run| {
                let first = NextTurn {
                    delegate: &meta.delegate,
                    index: 0,
                    question: &request.question,
                    resume: None,
                };
                run.launch_turn(&meta, &first, Resolution::Configured(&config))
            });
        if let Err(error) = started {
            std::fs::remove_dir_all(&dir)
                .inspect_err(|err| {
                    tracing::warn!(
                        ?err,
                        path = %dir.display(),
                        "cannot remove a run that never started"
                    );
                })
                .ok();
            return Err(error);
        }
        self.status(&run_id)
    }

    /// Mint an id and claim its directory.
    ///
    /// The claim is the exclusive create: an id that already exists — eight random hex
    /// characters make that rare, not impossible — is not written over but drawn again.
    pub(super) fn create_run_dir(&self) -> Result<(RunId, PathBuf), RunError> {
        let runs = self.runs_dir();
        create_private_dir(&runs)?;
        loop {
            let run_id = RunId::generate();
            let dir = runs.join(run_id.as_str());
            match reserve_dir(&dir) {
                Ok(()) => {
                    create_private_dir(&dir.join("turns"))?;
                    return Ok((run_id, dir));
                }
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(RunError::Io {
                        context: format!("creating {}", dir.display()),
                        source,
                    });
                }
            }
        }
    }

    /// Fill in whatever the configuration decides, so the record names what actually ran.
    ///
    /// The account, the isolation and the model are resolved once, here, rather than at each
    /// launch.
    /// A configuration edited mid-consultation would otherwise move a follow-up onto another
    /// identity — whose session it could not resume — silently change whether the delegate loads
    /// hooks, or answer the second half of one transcript with a different model.
    ///
    /// Isolation is pinned beside the account, because which file chose the account decides it.
    /// The account always leaves here resolved: the configured default by name, even one the
    /// configuration does not define, which the launch then refuses, or the CLI's own login when
    /// there is no default.
    ///
    /// The model is resolved through the machine's model rules: a rewrite is exchanged once, an
    /// effort the caller left unnamed is filled from the file's default for the model that runs,
    /// and stays unnamed — the CLI's own default — when the file has none.
    ///
    /// The permissions are filled from the machine file's defaults and held to its limits, and
    /// what the limits let run is what is recorded.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::ModelBlocked`] when the model that would run is blocked, and
    /// [`RunError::Delegate`] when the isolation or a permission exceeds the limits.
    pub(super) fn pin_defaults(
        delegate: &Delegate,
        config: &crate::config::Config,
    ) -> Result<Pinned, RunError> {
        let isolation = delegate.resolved_isolation(config);
        let mut pinned = delegate.clone();
        if *pinned.account() == AccountChoice::Default {
            // With no default, the CLI's own login, from now on, whatever default is added later.
            let resolved = match config.default_account(pinned.vendor()) {
                None => AccountChoice::CliLogin,
                Some(default) => AccountChoice::Named(default.alias.clone()),
            };
            pinned = pinned.with_account(resolved);
        }
        let vendor = pinned.vendor();
        // Only `start` calls this, so only a new consultation can meet a block.
        let ResolvedModel {
            launched,
            rewritten_from,
            default_effort,
        } = config.model_rules(vendor).resolve(pinned.model()).map_err(
            |RefusedModel { model, blocked }| RunError::ModelBlocked {
                vendor,
                model,
                blocked,
            },
        )?;
        pinned = pinned.with_model(launched);
        if pinned.effort().is_none()
            && let Some(effort) = default_effort
        {
            pinned = pinned.with_effort(effort);
        }
        let delegate = pinned
            .with_isolation(isolation)
            .with_default_permissions(config)
            .held_to_limits(config)?;
        Ok(Pinned {
            delegate,
            rewritten_from,
        })
    }

    /// Continue a consultation with another question, in the delegate's own session.
    ///
    /// The run id does not change: a consultation is a conversation.
    /// Every choice recorded when it started is kept; see [`RunStore::follow_up_with`] for a
    /// follow-up that changes some.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation, and
    /// [`RunError::NotResumable`] when the previous turn is still running, was cancelled, never
    /// announced a session id, or ended without the delegate saying anything.
    pub fn follow_up(&self, run_id: &RunId, question: &str) -> Result<RunStatus, RunError> {
        self.follow_up_with(run_id, question, PermissionOptions::default())
    }

    /// Continue a consultation, with the isolation or permissions `overrides` names in place of
    /// the ones its newest turn ran with.
    ///
    /// What `overrides` leaves out keeps what the newest turn ran with, or what `start` recorded
    /// for a turn launched before agentmux kept that; the machine file's defaults are not
    /// consulted again.
    /// Its current limits are, before every turn, so a limit tightened since the consultation
    /// began refuses this turn rather than letting it run past the owner's new ceiling.
    /// What the turn runs with is recorded with the turn, and the next follow-up starts from it;
    /// the consultation's own record, and the transcript header rendered from it, never change.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`RunStore::follow_up`], [`RunError::Delegate`] with
    /// [`DelegateError::NotForVendor`] for an override of the other vendor, and with
    /// [`DelegateError::Permission`] when the turn would exceed the current limits, before the
    /// turn is claimed.
    pub fn follow_up_with(
        &self,
        run_id: &RunId,
        question: &str,
        overrides: PermissionOptions,
    ) -> Result<RunStatus, RunError> {
        // Asked before the lock, which would otherwise leave a lock file for a run that is not
        // there.
        self.meta(run_id)?;
        {
            // Read, resolved, held and claimed under one lock, so the turn a follow-up was admitted
            // against is the one it continues and the one whose choices it starts from: nobody can
            // claim the next index, settle the newest turn differently, or launch one with other
            // permissions in between.
            // Published and settled under that same hold, so the turn being continued is committed
            // before the next one is claimed, and a waiter pinned to it can never be handed the
            // follow-up instead.
            let run = self.lock(run_id)?;
            let meta = self.meta(run_id)?;
            let state = run.fold_and_publish(&meta)?.state;
            let session = Self::resumable_session(run_id, &state)?;
            let mut delegate =
                self.current_delegate(&meta, state.newest.map(|newest| newest.index))?;
            // A record from before isolation was pinned could only have run isolated.
            if delegate.isolation().is_none() {
                delegate = delegate.with_isolation(Isolation::Isolated);
            }
            // Re-read each turn rather than cached at `start`: a follow-up hours later should see
            // the configuration as it is now, not as it was.
            let config = crate::config::Config::load(&self.host_env, &meta.cwd)?;
            let delegate = delegate.with_options(overrides)?.held_to_limits(&config)?;
            let index = u32::try_from(state.transcript.turns.len()).unwrap_or(u32::MAX);
            let next = NextTurn {
                delegate: &delegate,
                index,
                question,
                resume: Some(&session),
            };
            run.launch_turn(&meta, &next, Resolution::Configured(&config))?;
        }
        self.status(run_id)
    }

    /// The delegate a consultation's next turn starts from: the consultation's own, with the
    /// isolation and permissions its turn at `newest` ran with.
    ///
    /// What `start` recorded stands for a run with no turn yet, and for a turn launched before
    /// agentmux kept that; see [`Delegate::with_settings_of`] for what a turn's record may change.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] or [`RunError::CorruptRecord`] when that turn's launch record
    /// cannot be read.
    pub(super) fn current_delegate(
        &self,
        meta: &Meta,
        newest: Option<u32>,
    ) -> Result<Delegate, RunError> {
        let Some(index) = newest else {
            return Ok(meta.delegate.clone());
        };
        let ran_as =
            read_json::<InvocationRecord>(&self.turn_dir(&meta.run_id, index).invocation())?
                .and_then(|record| record.delegate);
        Ok(match ran_as {
            Some(ran_as) => meta.delegate.clone().with_settings_of(&ran_as),
            None => meta.delegate.clone(),
        })
    }

    /// Who a settled turn ran as: what `status` and `list` say of the newest turn, said of any
    /// one.
    ///
    /// Read from that turn's own launch record and the consultation's, so a later turn neither
    /// changes nor fails the answer, which is what a wait pinned to an earlier turn reports.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] or [`RunError::CorruptRecord`] when that turn's launch record
    /// cannot be read.
    pub fn turn_delegate(&self, meta: &Meta, settled: &SettledTurn) -> Result<Delegate, RunError> {
        self.current_delegate(meta, Some(settled.turn_index()))
    }

    /// The session a follow-up would continue, or why there is none.
    ///
    /// The one predicate behind both `follow_up` and `status.resumable`, so what is advertised
    /// and what is accepted cannot drift apart.
    /// Every condition is evidence rather than a rule about failure categories: a session was
    /// announced, the delegate said something worth continuing, and the newest turn is finished
    /// and was not interrupted mid-thought.
    /// A consultation in which the delegate never said anything is not continued whatever its
    /// outcome; one that has, and whose newest turn was refused — a rate limit, say — is merely
    /// paused, and its session is still there to continue.
    /// A reply the CLI wrote out late is only ever recovered into the newest turn, so allowing a
    /// follow-up here cannot let one land above the turn that followed it.
    ///
    /// A session the delegate could not save is not continued either: the CLI would be asked to
    /// resume a conversation it never wrote down.
    pub(super) fn resumable_session(
        run_id: &RunId,
        state: &RunState,
    ) -> Result<SessionRef, NotResumable> {
        let outcome = state.transcript.outcome();
        if !outcome.is_terminal() {
            return Err(NotResumable::StillRunning {
                run_id: run_id.clone(),
            });
        }
        if outcome == Outcome::Cancelled {
            return Err(NotResumable::Cancelled {
                run_id: run_id.clone(),
            });
        }
        if let Some(store) = state.unsaved_session_store() {
            return Err(NotResumable::SessionNotSaved {
                run_id: run_id.clone(),
                store: store.to_path_buf(),
            });
        }
        // Settled, so the turn being continued is committed, with the reply its CLI wrote on the
        // way out, before another turn is claimed after it; and exited, because a turn a
        // cancellation froze is settled while a delegate that survived it may still be saving.
        if state
            .newest
            .is_some_and(|newest| !(newest.settled && newest.delegate_gone))
        {
            return Err(NotResumable::AwaitingDelegateExit {
                run_id: run_id.clone(),
            });
        }
        if !state.transcript.has_delegate_content() {
            return Err(NotResumable::NothingSaid {
                run_id: run_id.clone(),
                state: outcome.state(),
            });
        }
        state
            .session
            .clone()
            .ok_or_else(|| NotResumable::NoSession {
                run_id: run_id.clone(),
            })
    }

    /// Refuse a launch for the reasons that belong to this process rather than to any run.
    ///
    /// Both are settled before anything is written, because neither can change while the process
    /// lives: agentmux inside a delegate spawns nothing, and a narrowed store spawns nothing of a
    /// denied vendor.
    pub(super) fn refuse_to_spawn(&self, vendor: Vendor) -> Result<(), RunError> {
        if self.running_inside_a_delegate()? {
            return Err(RunError::Recursive);
        }
        if self.denied.contains(&vendor) {
            return Err(RunError::VendorDenied { vendor });
        }
        Ok(())
    }
}

impl LockedRun<'_> {
    /// Spawn one turn's delegate.
    ///
    /// The refusals are repeated here rather than left to `start` alone because this is the one
    /// place a child is actually spawned.
    /// `follow_up` and anything added later are then covered by construction rather than by
    /// remembering: a consultation begun from a terminal is not a way to drive a denied vendor
    /// from a server, because its follow-up spawns the same delegate again from here.
    ///
    /// An automatic resume is the one turn they do not apply to.
    /// It continues a consultation a caller's launch already admitted, and is made by that
    /// launch's own supervisor, whose environment carries the delegate marker it hands on, so the
    /// recursion guard would refuse it for the marker it set itself.
    /// Only [`super::auto_resume`] builds a [`Resolution::Inherited`], from a plan that admitted
    /// launch wrote.
    pub(super) fn launch_turn(
        &self,
        meta: &Meta,
        next: &NextTurn<'_>,
        resolution: Resolution<'_>,
    ) -> Result<(), RunError> {
        let NextTurn {
            delegate, index, ..
        } = *next;
        if let Resolution::Configured(_) = resolution {
            self.store.refuse_to_spawn(delegate.vendor())?;
        }
        // The run may have been swept or removed while this caller waited for the lock, and a
        // turn launched into a directory with no metadata would be a child nothing can reach.
        if !self.store.run_dir(&self.run_id).join("meta.json").is_file() {
            return Err(RunError::NotFound(self.run_id.clone()));
        }
        let turn = self.store.turn_dir(&self.run_id, index);
        claim_turn(&turn, index, &self.run_id)?;
        // A claim that does not end in a running child is given back, so a refused follow-up
        // does not leave a phantom failed turn in the transcript.
        let launched = self.store.launch_into(&turn, meta, next, resolution);
        if launched.is_err() {
            std::fs::remove_dir_all(&turn.root)
                .inspect_err(|err| {
                    tracing::warn!(
                        ?err,
                        path = %turn.root.display(),
                        "cannot give back a turn claim"
                    );
                })
                .ok();
        }
        launched
    }
}

impl RunStore {
    /// Decide how a turn runs, and point `invocation` at the driver when it is driven.
    ///
    /// Decided per turn, from the delegate's app-server `settings` and this store: the
    /// environment, the refusals and the billing already in `invocation` are the same either way,
    /// and only what is run differs.
    pub(super) fn prepare_launch_mode(
        &self,
        turn: &TurnDir,
        meta: &Meta,
        settings: Option<AppServerSettings>,
        resume: Option<&SessionRef>,
        invocation: &mut crate::delegate::Invocation,
    ) -> Result<LaunchMode, RunError> {
        let (Some(driver), Some(settings), false) =
            (&self.codex_driver, settings, meta.force_codex_exec)
        else {
            return Ok(LaunchMode::DirectCli);
        };
        let plan_path = turn.app_server_plan_path();
        write_json(
            &plan_path,
            &crate::appserver::DriverPlan {
                settings,
                cwd: meta.cwd.clone(),
                resume_thread_id: resume.cloned(),
                billing: invocation.billing(),
                turn_dir: turn.root.clone(),
            },
        )?;
        invocation.set_driver_command(driver, crate::appserver::SUBCOMMAND, &plan_path);
        Ok(LaunchMode::AppServer)
    }

    /// Everything between claiming a turn directory and recording the running child in it.
    ///
    /// The turn's delegate was held to the machine file's limits by the caller that admitted it;
    /// [`Delegate::invocation`] holds it once more as it builds the launch.
    /// An automatic resume runs as the turn it continues ran, held to the limits that turn was
    /// admitted under, because its supervisor has no configuration to hold it to.
    pub(super) fn launch_into(
        &self,
        turn: &TurnDir,
        meta: &Meta,
        next: &NextTurn<'_>,
        resolution: Resolution<'_>,
    ) -> Result<(), RunError> {
        let NextTurn {
            delegate,
            index,
            question,
            resume,
        } = *next;
        std::fs::write(turn.question(), question)
            .map_err(RunError::io(format!("writing turn {index} question")))?;

        let last_message = turn.last_message();
        let plan = TurnPlan {
            question_path: &turn.question(),
            last_message_path: &last_message,
            resume,
            extra_env: &meta.env,
            billing: meta.billing,
        };
        let Resolved {
            mut invocation,
            app_server_settings,
            auto_resume_attempt,
        } = resolution.resolve(delegate, &plan, &self.host_env)?;
        // The store's own root, resolved, rather than whatever the host exported: an agentmux
        // started underneath this delegate has to open the same store to recognise the delegate
        // as its ancestor, and the platform default can differ between the two environments.
        invocation.set_state_dir_env(Self::STATE_DIR_ENV, &self.root);

        // Asked of the delegate's own CLI before the launch mode can put the driver in its place.
        let cli_version = self.launcher.version(&invocation, &meta.cwd);
        let launch_mode =
            self.prepare_launch_mode(turn, meta, app_server_settings, resume, &mut invocation)?;
        refuse_an_unverified_profile(launch_mode, delegate, cli_version.as_deref())?;

        // Asked of the environment the child gets, before the spawn: a delegate inherits whatever
        // confines this process, so a session store this process cannot write to is one the
        // delegate cannot save into, and a follow-up would resume a session that was never kept.
        // Both directories are as the child resolves them: a relative one from its own working
        // directory, not from this process's.
        let vendor = delegate.vendor();
        let config_dir = vendor
            .config_dir(invocation.env())
            .map(|dir| meta.cwd.join(dir));
        let unsaved_session_store = vendor
            .session_store(invocation.env())
            .map(|store| meta.cwd.join(store))
            .filter(|store| !can_create_files_in(store));
        if let Some(store) = &unsaved_session_store {
            tracing::warn!(
                run_id = %meta.run_id,
                turn = index,
                store = %store.display(),
                "the delegate cannot save its session there, so this consultation cannot be followed up",
            );
        }

        // Recorded before the spawn so a launch that fails still leaves evidence of what was
        // tried, and so a later fold can tell whether a resume opened the session it asked for
        // and whether the child could keep the session it opened.
        write_json(
            &turn.invocation(),
            &InvocationRecord {
                program: invocation.program().to_owned(),
                args: invocation.args().to_vec(),
                env_keys: invocation.env().keys().cloned().collect(),
                cwd: meta.cwd.clone(),
                resumed_from: resume.cloned(),
                unsaved_session_store,
                launch_mode,
                billing: Some(invocation.billing()),
                config_dir,
                cli_version,
                agentmux_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                delegate: Some(delegate.clone()),
                auto_resume_attempt,
            },
        )?;

        // Under `agentmux mcp` this is the only visible record of what was spawned, and the
        // argument vector is the thing most worth seeing when a delegate misbehaves.
        // Environment values are deliberately absent: they carry credentials.
        tracing::info!(
            run_id = %meta.run_id,
            turn = index,
            program = invocation.program(),
            args = ?invocation.args(),
            cwd = %meta.cwd.display(),
            resumed = resume.is_some(),
            automatic = auto_resume_attempt.is_some(),
            mode = ?launch_mode,
            billing = %invocation.billing(),
            "launching delegate",
        );

        if let Some(supervisor) = &self.supervisor {
            return self.launch_supervisor(turn, meta, index, &invocation, supervisor);
        }
        let launched = self.launcher.launch(&LaunchSpec {
            invocation: &invocation,
            cwd: &meta.cwd,
            question: &turn.question(),
            events: &turn.events(),
            stderr: &turn.stderr(),
        })?;
        let recorded = write_json(
            &turn.launch(),
            &LaunchRecord {
                launched,
                started_at: Utc::now(),
            },
        );
        if let Err(error) = recorded {
            // A child nothing records is a child nothing can ever stop; better to stop it now,
            // while the handle is still warm, than to leave it billing behind a failed write.
            self.launcher.terminate(launched);
            self.launcher.kill(launched);
            return Err(error);
        }
        tracing::debug!(run_id = %meta.run_id, turn = index, pid = launched.pid, "delegate running");
        Ok(())
    }

    /// Launch the turn's supervisor, which starts `delegate` itself, and record it.
    ///
    /// The plan names what to run and the launch it belongs to; the environment travels as the
    /// supervisor's own, so nothing secret is written down and nothing is resolved twice.
    /// The supervisor waits on the run's lock, which the caller holds until the record below is
    /// written, and starts nothing unless that record names it and its launch.
    /// Everything that fails from here on is a turn that failed, kept with its evidence.
    fn launch_supervisor(
        &self,
        turn: &TurnDir,
        meta: &Meta,
        index: u32,
        delegate: &crate::delegate::Invocation,
        supervisor: &Path,
    ) -> Result<(), RunError> {
        let launch_id = uuid::Uuid::new_v4().simple().to_string();
        let plan_path = turn.supervisor_plan_path();
        write_json(
            &plan_path,
            &SupervisorPlan {
                store_root: self.root.clone(),
                run_id: meta.run_id.clone(),
                turn_index: index,
                launch_id: launch_id.clone(),
                producer: ProducerCommand {
                    program: delegate.program().to_owned(),
                    args: delegate.args().to_vec(),
                    cwd: meta.cwd.clone(),
                    billing: delegate.billing(),
                },
                auto_resume: self.auto_resume.map(|policy| AutoResumePlan {
                    policy,
                    supervisor: supervisor.to_path_buf(),
                    codex_driver: self.codex_driver.clone(),
                }),
            },
        )?;
        let mut invocation = delegate.clone();
        invocation.set_driver_command(supervisor, super::SUPERVISE_SUBCOMMAND, &plan_path);
        // Its stdout and stderr are its own log, never the capture.
        let log = turn.supervisor_log();
        let launched = self.launcher.launch(&LaunchSpec {
            invocation: &invocation,
            cwd: &meta.cwd,
            question: &turn.question(),
            events: &log,
            stderr: &log,
        })?;
        let recorded = write_json(
            &turn.supervisor(),
            &SupervisorRecord {
                launch_id,
                launched,
                started_at: Utc::now(),
                phase: SupervisorPhase::Prepared,
            },
        );
        if let Err(error) = recorded {
            // Unrecorded, it would start nothing, but it is stopped rather than left to find out.
            self.launcher.terminate(launched);
            self.launcher.kill(launched);
            return Err(error);
        }
        tracing::debug!(
            run_id = %meta.run_id,
            turn = index,
            pid = launched.pid,
            "supervisor running"
        );
        Ok(())
    }
}

/// Refuse a `codex exec` turn that needs a permission profile from a codex whose `--version`,
/// `cli_version`, is not one verified to honour it.
///
/// `codex exec` reaches read-only with network only through a permission profile, which a codex
/// that does not honour it would replace with whatever its configuration selects.
/// The app-server sets the same policy as a field of the turn instead, and needs no profile.
fn refuse_an_unverified_profile(
    launch_mode: LaunchMode,
    delegate: &Delegate,
    cli_version: Option<&str>,
) -> Result<(), RunError> {
    let needs_profile = delegate
        .codex_permissions()
        .is_some_and(|permissions| permissions.needs_permission_profile());
    if launch_mode == LaunchMode::DirectCli
        && needs_profile
        && !codex_supports_permission_profiles(cli_version)
    {
        return Err(DelegateError::Permission(
            crate::config::PermissionRefusal::PermissionProfileUnsupported {
                version: cli_version.map(ToOwned::to_owned),
            },
        )
        .into());
    }
    Ok(())
}

/// Claim a turn index by creating its directory.
///
/// Creating the directory *is* the claim, and it has to fail if someone else already made it:
/// two callers that both fold a one-turn consultation both choose index 1, and with a
/// forgiving create they would both launch a paid child into the same capture file.
///
/// A directory that exists but holds neither a launch nor an exit record was claimed by a
/// caller that died before recording its child.
/// Under the lock nobody else can be between those two steps, so such a claim is abandoned
/// and is taken over rather than left to block every follow-up for good; its files are set
/// aside, because a child spawned in that window may still be writing to them.
pub(super) fn claim_turn(turn: &TurnDir, index: u32, run_id: &RunId) -> Result<(), RunError> {
    let claim = |source: std::io::Error| RunError::Io {
        context: format!("reserving turn {index}"),
        source,
    };
    match reserve_dir(&turn.root) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            let recorded = read_json::<LaunchRecord>(&turn.launch())?.is_some()
                || read_json::<ExitRecord>(&turn.exit())?.is_some()
                || read_json::<SupervisorRecord>(&turn.supervisor())?.is_some();
            if recorded {
                return Err(RunError::TurnAlreadyClaimed {
                    run_id: run_id.clone(),
                    turn: index.saturating_add(1),
                });
            }
            // Set aside rather than deleted: the caller that died may have spawned its child
            // first, and that child is still writing into these files.
            // A name no fold walks keeps them out of the consultation and keeps them for a human.
            let aside = turn
                .root
                .with_extension(format!("abandoned.{}", uuid::Uuid::new_v4().simple()));
            tracing::warn!(%run_id, turn = index, aside = %aside.display(), "taking over an abandoned turn claim");
            std::fs::rename(&turn.root, &aside).map_err(claim)?;
            reserve_dir(&turn.root).map_err(claim)
        }
        Err(source) => Err(claim(source)),
    }
}

/// Whether this process can create files where a delegate would save its session.
///
/// Asked the way the CLI will: by creating a file in the store when it is there, and the store
/// itself when the CLI has not made it yet — a sandbox that grants the store alone must pass,
/// and probing the directory above it would refuse exactly that grant. A store whose parent does
/// not exist says nothing about confinement, because the CLI is simply not set up there, and
/// neither does any failure other than a permission or read-only refusal, so only those count as
/// "no". Whatever the probe creates is removed at once, and its name says who left it should
/// that ever fail.
pub(super) fn can_create_files_in(store: &Path) -> bool {
    let attempt = if store.is_dir() {
        let probe = store.join(format!(
            ".agentmux-write-probe-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map(|file| {
                drop(file);
                std::fs::remove_file(&probe)
                    .inspect_err(|err| {
                        tracing::warn!(
                            ?err,
                            path = %probe.display(),
                            "cannot remove a write probe"
                        );
                    })
                    .ok();
            })
    } else {
        std::fs::create_dir(store).map(|()| {
            std::fs::remove_dir(store)
                .inspect_err(|err| {
                    tracing::warn!(?err, path = %store.display(), "cannot remove a write probe");
                })
                .ok();
        })
    };
    match attempt {
        Ok(()) => true,
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
        ),
    }
}
