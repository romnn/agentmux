//! Continuing a consultation in a new one, for a consultation whose own session cannot go on.

use std::path::Path;

use chrono::Utc;

use crate::delegate::Isolation;
use crate::transcript::{FailureKind, Outcome};

use super::RunStore;
use super::auto_resume::AutoResume;
use super::error::RunError;
use super::id::RunId;
use super::records::{ContinuedBy, write_json};
use super::spawn::{Admission, StartRequest};
use super::status::RunStatus;

/// How many turns in a row must have been refused by a content filter before every reader
/// suggests [`RunStore::continue_fresh`] instead of a rephrased follow-up.
///
/// A heuristic: one refusal is usually the prompt's wording, which a rephrased follow-up gets
/// round, and a second in a row suggests the session's own context is what the filter refuses.
pub const CONTENT_FLAGGED_STREAK: u32 = 2;

impl RunStore {
    /// Continue a consultation in a new one, run as its newest turn ran, whose first question
    /// points the delegate at the old transcript and adds `question`.
    ///
    /// The delegate, account, effort, permissions and isolation are the newest turn's, held to
    /// the machine file's current limits; the working directory, the environment the caller
    /// asked for, pricing, retention and label carry over.
    /// For a consultation whose session cannot go on, such as one whose context a content
    /// filter refuses turn after turn.
    /// The new consultation records which one it continues, and the old one which consultation
    /// continued it, so each `status` names the other.
    /// The model is the one the old consultation asked for, so this machine's model rules apply
    /// to it as to any new consultation.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::NotFound`] when there is no such consultation,
    /// [`RunError::StillRunning`] while its newest turn has not settled or agentmux has
    /// scheduled an automatic resume of it, and whatever [`RunStore::start`] refuses the new
    /// consultation for.
    pub fn continue_fresh(
        &self,
        run_id: &RunId,
        question: Option<&str>,
    ) -> Result<RunStatus, RunError> {
        let meta = self.meta(run_id)?;
        // Held until the successor is linked, so nothing can remove the consultation, or begin a
        // turn of it, between the reading the brief describes and the link that names the
        // successor.
        let run = self.lock(run_id)?;
        let state = run.fold_and_publish(&meta)?.state;
        let Some(newest) = state.newest.filter(|newest| newest.settled) else {
            return Err(RunError::StillRunning {
                run_id: run_id.clone(),
            });
        };
        // A resume agentmux has scheduled would run the old session beside the new one.
        if let Some(AutoResume::Scheduled { .. }) =
            self.auto_resume_at_index(run_id, newest.index)?
        {
            return Err(RunError::StillRunning {
                run_id: run_id.clone(),
            });
        }
        let mut delegate = self.current_delegate(&meta, Some(newest.index))?;
        // A record from before isolation was pinned could only have run isolated.
        if delegate.isolation().is_none() {
            delegate = delegate.with_isolation(Isolation::Isolated);
        }
        let asked_for = meta
            .rewritten_from
            .clone()
            .unwrap_or_else(|| delegate.model().clone());
        let request = StartRequest {
            delegate: delegate.with_model(asked_for),
            question: brief(
                run_id,
                &self.transcript_path(run_id),
                &meta.cwd,
                &state.transcript.outcome(),
                question,
            ),
            cwd: meta.cwd.clone(),
            retention: meta.retention,
            env: meta.env.clone(),
            billing: meta.billing,
            force_codex_exec: meta.force_codex_exec,
            label: self.label_of_turn(run_id, newest.index)?,
        };
        let status = self.begin(&request, Admission::Continuing(run_id))?;
        tracing::info!(%run_id, continued_by = %status.run_id, "continued a consultation afresh");
        write_json(
            &self.continued_by_path(run_id),
            &ContinuedBy {
                run_id: status.run_id.clone(),
                at: Utc::now(),
            },
        )?;
        drop(run);
        Ok(status)
    }
}

/// The first question of a consultation that continues `run_id`: where its transcript is, that
/// the working directory may hold its uncommitted work, and what to do now.
fn brief(
    run_id: &RunId,
    transcript: &Path,
    cwd: &Path,
    outcome: &Outcome,
    question: Option<&str>,
) -> String {
    let why = match outcome {
        Outcome::Failed {
            kind: FailureKind::ContentFlagged,
            ..
        } => "a content filter kept refusing its turns".to_owned(),
        Outcome::Failed { kind, .. } => format!("its last turn failed ({})", kind.label()),
        Outcome::Cancelled => "its last turn was cancelled".to_owned(),
        Outcome::Running | Outcome::Completed { .. } => {
            "its session was not to be continued".to_owned()
        }
    };
    let task = question
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map_or_else(
            || {
                "Carry on with the work that consultation was doing, from where it stopped."
                    .to_owned()
            },
            ToOwned::to_owned,
        );
    indoc::formatdoc! {"
        This continues agentmux consultation {run_id}, which could not go on in its own session: \
        {why}.

        Its transcript, with every question it was asked and everything found and decided, is at \
        {transcript}. Nothing of it is in this conversation: start with its last turns, and read \
        earlier ones as you need them.

        The working directory, {cwd}, may hold uncommitted work from that consultation. Look at it \
        first, with `git status` and `git diff` in a git checkout, and build on that work rather \
        than starting over.

        ## What to do now

        {task}
        ",
        transcript = transcript.display(),
        cwd = cwd.display(),
    }
}
