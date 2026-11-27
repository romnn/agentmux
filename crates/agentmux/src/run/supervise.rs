//! The per-turn supervisor: `agentmux supervise <plan>`, which starts a turn's delegate, waits for
//! it to go, and settles the turn the moment it has.
//!
//! Changes when how a turn is started or watched changes.
//!
//! A turn settles when something folds it after its delegate has gone.
//! Without a supervisor that something is whoever reads the consultation next, so a delegate that
//! finishes while nobody is asking leaves its transcript unpublished until somebody does.
//! The supervisor is that reader, there from the start: a detached process of its own, launched
//! by the run store in the delegate's place, holding the delegate's handle, so it collects the
//! exit status no later observer can and commits the turn's `settled.json` within moments of the
//! delegate leaving.
//!
//! ```text
//! CLI or MCP server
//! └─ agentmux supervise supervisor-plan.json      supervisor.json, supervisor.log
//!    └─ claude --print | codex exec               launch.json, events.jsonl, stderr.log
//!       or agentmux codex-app-server app-server.json
//!          └─ codex app-server
//! ```
//!
//! Nothing depends on the supervisor surviving.
//! The delegate runs in a process group of its own, so a supervisor that dies leaves it running,
//! recorded, and settled by the next observer once it has gone; a supervisor gone does not mean
//! a turn done.
//! The one gap is the instant between spawning the delegate and recording it, which no ordinary
//! spawn closes: a supervisor found gone there leaves a turn that is neither settled nor
//! replaced, and says so.
//!
//! A turn that failed because the model was overloaded is not the end of its supervisor's work:
//! it waits, and resumes the consultation as the next turn, which gets a supervisor of its own.
//!
//! A Claude turn reading stream-json input has its supervisor hold its input too: the write end
//! of the pipe that is Claude's stdin, through which it writes the question and each steer
//! ([`super::claude_input`]).
//! Nothing about the turn's completion depends on it either: a supervisor that dies closes
//! Claude's input, and Claude finishes what it has.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::delegate::Invocation;
use crate::launch::{LaunchInput, LaunchSpec, Launched, ProcessLauncher};

use super::auto_resume::Resumption;
use super::claude_input::{self, ClaudeInput};
use super::error::RunError;
use super::layout::TurnDir;
use super::records::{
    ExitRecord, InvocationRecord, LaunchMode, LaunchRecord, SupervisorPhase, SupervisorPlan,
    SupervisorRecord, read_complete_records, read_json, write_json, write_json_once,
};
use super::{RunStore, is_this_process};

/// The subcommand of the `agentmux` executable that runs [`supervise`].
///
/// Hidden from its help: nothing but the run store launches it.
pub const SUBCOMMAND: &str = "supervise";

/// How often a supervisor looks at its delegate, and for a cancellation.
const POLL: Duration = Duration::from_millis(100);

/// Supervise the turn a plan describes, as the `supervise` subcommand.
///
/// `host_env` is the environment this process was started with, which is the delegate's own:
/// the run store launches the supervisor with the environment it built for the delegate, and the
/// supervisor hands it on unchanged, to an automatic resume's turn too.
/// Returns the process's exit status: zero once the turn is settled, and any automatic resume it
/// called for launched or given up, non-zero when the plan could not be read or does not belong
/// to the turn it names, or when the turn could not be settled, which the next observer then does.
pub async fn supervise(
    plan_path: &Path,
    host_env: std::collections::BTreeMap<String, String>,
) -> i32 {
    let plan = match read_json::<SupervisorPlan>(plan_path) {
        Ok(Some(plan)) => plan,
        Ok(None) => {
            eprintln!(
                "agentmux: there is no supervisor plan at {}",
                plan_path.display()
            );
            return 2;
        }
        // Plans are written whole, so one this build cannot parse was most likely written by
        // another: an agentmux process that outlived an upgrade launches the new build with its
        // old plan.
        // A plan edited by hand or damaged is the other explanation.
        Err(err @ RunError::CorruptRecord { .. }) => {
            eprint!(
                "{}",
                indoc::formatdoc! {"
                    agentmux: {err}
                    This supervisor is agentmux {version}, so the plan was most likely written \
                    by an agentmux process of another version that outlived an upgrade. Restart \
                    the agentmux MCP server, or whichever agentmux process launched it.
                ",
                    version = env!("CARGO_PKG_VERSION"),
                }
            );
            return 2;
        }
        Err(err) => {
            eprintln!("agentmux: {err}");
            return 2;
        }
    };
    let mut store =
        match RunStore::open(&plan.store_root, Arc::new(ProcessLauncher::new()), host_env) {
            Ok(store) => store,
            Err(err) => {
                eprintln!("agentmux: {err}");
                return 2;
            }
        };
    // An automatic resume runs as the store that launched this turn would have run it:
    // supervised, by the same executables, under the same policy.
    store = match &plan.auto_resume {
        Some(resume) => {
            let configured = store
                .with_supervisor(&resume.supervisor)
                .with_auto_resume(Some(resume.policy));
            match &resume.codex_driver {
                Some(driver) => configured.with_codex_app_server(driver),
                None => configured,
            }
        }
        None => store.with_auto_resume(None),
    };
    match store.supervise_turn(&plan).await {
        Ok(Supervised::Settled) => match store.resume_when_due(&plan).await {
            Ok(Resumption::Resumed) => {
                eprintln!("agentmux: the model was overloaded; resumed the consultation");
                0
            }
            Ok(Resumption::NotScheduled) => 0,
            Ok(Resumption::Cancelled) => {
                eprintln!("agentmux: the automatic resume was cancelled");
                0
            }
            Ok(Resumption::Superseded) => {
                eprintln!("agentmux: a follow-up took the place of the automatic resume");
                0
            }
            Err(err) => {
                eprintln!("agentmux: the consultation was not resumed automatically: {err}");
                1
            }
        },
        Ok(Supervised::Refused(reason)) => {
            eprintln!("agentmux: {reason}, so no delegate was started");
            2
        }
        Ok(Supervised::Unsettled) => {
            eprintln!("agentmux: the turn did not settle; the next read of the consultation does");
            1
        }
        Err(err) => {
            eprintln!("agentmux: {err}");
            1
        }
    }
}

/// What a Claude turn reading stream-json input needs before Claude is spawned.
struct StreamInputSetup {
    /// The read end of the pipe, Claude's stdin.
    claude_stdin: std::io::PipeReader,
    /// The write end, which this supervisor keeps.
    pipe: std::io::PipeWriter,
    question: String,
    /// The id the question is sent under: the turn's launch id.
    question_id: uuid::Uuid,
}

/// Make everything a Claude turn reading stream-json input needs before Claude is spawned, or say
/// why it cannot be made.
fn stream_input_setup(plan: &SupervisorPlan, turn: &TurnDir) -> Result<StreamInputSetup, String> {
    let question_id = uuid::Uuid::try_parse(&plan.launch_id)
        .map_err(|err| format!("its launch id is not a UUID: {err}"))?;
    let question = std::fs::read_to_string(turn.question())
        .map_err(|err| format!("its question could not be read: {err}"))?;
    let (claude_stdin, pipe) = std::io::pipe()
        .map_err(|err| format!("the pipe for its input could not be made: {err}"))?;
    Ok(StreamInputSetup {
        claude_stdin,
        pipe,
        question,
        question_id,
    })
}

/// How a supervision ended.
enum Supervised {
    /// The turn is settled.
    Settled,
    /// The turn is not this supervisor's to run.
    Refused(String),
    /// The turn could not be settled, which only a broken invariant leaves, so it is left to the
    /// next observer.
    Unsettled,
}

/// What admission decided.
enum Admission {
    /// The delegate is running and recorded, with its input when this supervisor holds it.
    Running {
        delegate: Launched,
        input: Option<Box<ClaudeInput>>,
    },
    /// Nothing will run, and a record says why, so the turn settles from it.
    Recorded,
    /// The turn is not this supervisor's.
    Refused(String),
}

impl RunStore {
    async fn supervise_turn(&self, plan: &SupervisorPlan) -> Result<Supervised, RunError> {
        let turn = self.turn_dir(&plan.run_id, plan.turn_index);
        let (delegate, mut input) = match self.admit(plan, &turn)? {
            Admission::Running { delegate, input } => (delegate, input),
            Admission::Recorded => return self.settle_supervised(plan),
            Admission::Refused(reason) => return Ok(Supervised::Refused(reason)),
        };

        // Watch until the delegate has gone, or a cancellation has been carried out, serving its
        // input between looks.
        // A cancellation is carried out here as well as by the caller that asked for it, so one
        // that dies part-way still ends the turn; both stops check the delegate's identity.
        while !self.child_has_gone(delegate) {
            if turn.cancelling().exists() {
                if let Some(input) = &mut input {
                    input.close(self, &plan.run_id, claude_input::CANCELLING);
                }
                self.stop_delegate(&turn, delegate).await?;
                self.record_cancellation(&plan.run_id, &turn)?;
                break;
            }
            if let Some(input) = &mut input {
                input.step(self, &plan.run_id);
            }
            tokio::time::sleep(POLL).await;
        }
        if let Some(input) = &mut input {
            input.close(self, &plan.run_id, claude_input::EXITED);
        }
        self.settle_supervised(plan)
    }

    /// Check that the turn is this supervisor's, then start and record its delegate, all under
    /// the run's lock.
    ///
    /// The caller that launched this supervisor holds the lock until it has recorded it, so the
    /// record is there, or that caller died before writing it and the turn is not this one's.
    /// The delegate is spawned and recorded under one hold, so no observer that takes the lock
    /// ever finds it spawned and unrecorded.
    fn admit(&self, plan: &SupervisorPlan, turn: &TurnDir) -> Result<Admission, RunError> {
        let _run = self.lock(&plan.run_id)?;
        let Some(mut record) =
            read_json::<SupervisorRecord>(&turn.supervisor())?.filter(|record| {
                record.launch_id == plan.launch_id && is_this_process(record.launched)
            })
        else {
            return Ok(Admission::Refused(format!(
                "the turn at {} does not name this supervisor's launch",
                turn.root.display()
            )));
        };
        if record.phase != SupervisorPhase::Prepared || turn.launch().exists() {
            return Ok(Admission::Refused(format!(
                "the turn at {} has already been started",
                turn.root.display()
            )));
        }
        if turn.exit().exists() {
            return Ok(Admission::Recorded);
        }
        // Cancelled before anything ran: the turn ends here, with nothing in its capture.
        if turn.cancelling().exists() {
            let (_, events_len) = read_complete_records(&turn.events(), None)?;
            write_json_once(&turn.exit(), &ExitRecord::Cancelled { events_len })?;
            return Ok(Admission::Recorded);
        }

        // A Claude turn reading stream-json input is given its input through a pipe this
        // supervisor holds, made with everything else that input needs before anything is
        // spawned, so a failure here is a turn that never started.
        let stream_input = match read_json::<InvocationRecord>(&turn.invocation())? {
            Some(invocation) if invocation.launch_mode == LaunchMode::ClaudeStreamInput => {
                match stream_input_setup(plan, turn) {
                    Ok(setup) => Some(setup),
                    Err(reason) => {
                        write_json_once(&turn.exit(), &ExitRecord::NotStarted { reason })?;
                        return Ok(Admission::Recorded);
                    }
                }
            }
            Some(_) | None => None,
        };

        record.phase = SupervisorPhase::Spawning;
        write_json(&turn.supervisor(), &record)?;
        let invocation = Invocation::prepared(
            plan.producer.program.clone(),
            plan.producer.args.clone(),
            self.host_env.clone(),
            plan.producer.billing,
        );
        let question_file = turn.question();
        let launched = match self.launcher.launch(&LaunchSpec {
            invocation: &invocation,
            cwd: &plan.producer.cwd,
            input: match &stream_input {
                Some(setup) => LaunchInput::Pipe(&setup.claude_stdin),
                None => LaunchInput::Question(&question_file),
            },
            events: &turn.events(),
            stderr: &turn.stderr(),
        }) {
            Ok(launched) => launched,
            Err(err) => {
                write_json_once(
                    &turn.exit(),
                    &ExitRecord::NotStarted {
                        reason: err.to_string(),
                    },
                )?;
                return Ok(Admission::Recorded);
            }
        };
        let recorded = write_json(
            &turn.launch(),
            &LaunchRecord {
                launched,
                started_at: Utc::now(),
            },
        );
        if let Err(err) = recorded {
            // A delegate nothing records is one nothing can ever stop, so it is stopped now, while
            // its handle is warm, and the turn is recorded as one that never ran.
            self.launcher.terminate(launched);
            self.launcher.kill(launched);
            write_json_once(
                &turn.exit(),
                &ExitRecord::NotStarted {
                    reason: format!("its launch could not be recorded: {err}"),
                },
            )?;
            return Ok(Admission::Recorded);
        }
        // Claude holds the read end now, so once it has gone a write fails rather than waits.
        let input = stream_input.map(|setup| {
            let StreamInputSetup {
                claude_stdin,
                pipe,
                question,
                question_id,
            } = setup;
            drop(claude_stdin);
            Box::new(ClaudeInput::start(pipe, &question, question_id, turn))
        });
        Ok(Admission::Running {
            delegate: launched,
            input,
        })
    }

    /// Settle the turn, once nothing is left to change it.
    ///
    /// Its delegate has gone, which this supervisor saw through the handle it holds and the fold
    /// reads back the same way, or a record says it never ran or was cancelled, and a
    /// cancellation is committed whether or not the delegate left: either way one finalization
    /// settles it.
    fn settle_supervised(&self, plan: &SupervisorPlan) -> Result<Supervised, RunError> {
        Ok(
            match self.finalize_turn_index(&plan.run_id, plan.turn_index)? {
                Some(_) => Supervised::Settled,
                None => Supervised::Unsettled,
            },
        )
    }
}
