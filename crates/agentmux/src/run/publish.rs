//! Rendering a consultation and publishing the render.

use crate::transcript::render_turn;

use super::error::RunError;
use super::lock::LockedRun;
use super::records::{Meta, SettledRecord, write_json};
use super::settle::Finality;
use super::status::describe_delegate;
use super::{RunState, RunStore};

impl RunStore {
    /// Render the whole consultation as Markdown, and where each turn's section lies in it.
    ///
    /// The result is **append-only for the life of the consultation**, which is the contract
    /// [`RunStore::view`] pages by byte offset and `tail` hands back as a cursor.
    /// Two things make that true, and both are easy to break:
    ///
    /// - this header is built only from values fixed when the consultation was created, so
    ///   nothing here may ever carry state, elapsed time or a message count;
    /// - each turn's body grows only at its end, which
    ///   [`crate::transcript::render_turn`] documents and enforces, and the fold behind it never
    ///   reads past a turn's settlement, which [`super::settle`] documents and enforces.
    ///
    /// Adding a mutable field to the header would silently invalidate every outstanding cursor.
    pub(super) fn render(meta: &Meta, state: &RunState) -> Rendered {
        use std::fmt::Write as _;
        let mut text = String::new();
        let _ = write!(
            text,
            "# Consultation {}\n\n\
             - delegate: {}\n\
             - working directory: `{}`\n\
             - started: {}\n",
            meta.run_id,
            describe_delegate(&meta.delegate, meta.rewritten_from.as_ref()),
            meta.cwd.display(),
            meta.created_at.to_rfc3339(),
        );
        // Each written only when there is one, so a transcript rendered before either existed
        // renders byte for byte as it did.
        if let Some(label) = &meta.label {
            let _ = writeln!(text, "- label: {label}");
        }
        if let Some(continues) = &meta.continues {
            let _ = writeln!(text, "- continues: {continues}");
        }
        let offset = |text: &str| u64::try_from(text.len()).unwrap_or(u64::MAX);
        let mut spans = Vec::with_capacity(state.transcript.turns.len());
        for turn in &state.transcript.turns {
            let start = offset(&text);
            text.push_str(&render_turn(turn));
            spans.push(TurnSpan {
                index: turn.index,
                start,
                end: offset(&text),
            });
        }
        Rendered { text, spans }
    }
}

/// A rendered consultation, with where each turn's section lies in it.
#[derive(Debug)]
pub(super) struct Rendered {
    pub(super) text: String,
    pub(super) spans: Vec<TurnSpan>,
}

/// Where one turn's section lies in a rendered consultation, in bytes.
#[derive(Debug, Clone, Copy)]
pub(super) struct TurnSpan {
    pub(super) index: u32,
    pub(super) start: u64,
    /// Exclusive.
    pub(super) end: u64,
}

impl LockedRun<'_> {
    /// Publish the rendered transcript, replacing it atomically.
    ///
    /// A reader outside agentmux — a human with an editor, another agent with `cat` — must
    /// never see a half-written file.
    /// Publication is ordered by the lock: a caller folds and publishes while holding it, so a
    /// render it publishes is never older than one published before it.
    /// A published file the render is a prefix of is still left alone: it was published by
    /// something that folded more, such as an older agentmux that does not fold under the lock
    /// and is still serving while a newer one runs, and the render is append-only, so the
    /// prefix is simply older.
    pub(super) fn publish_transcript(&self, text: &str) -> Result<(), RunError> {
        let path = self.store.transcript_path(&self.run_id);
        // The published length is checked before the published text is read: a render that has
        // grown — every poll of a live consultation — never needs the old file read at all, and
        // one that has not changed — every read of a finished one — is not rewritten.
        let published_len = std::fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if published_len >= u64::try_from(text.len()).unwrap_or(u64::MAX)
            && std::fs::read_to_string(&path).is_ok_and(|published| published.starts_with(text))
        {
            return Ok(());
        }
        crate::atomic_file::write_whole(&path, text.as_bytes())
            .map_err(RunError::io("replacing the rendered transcript"))
    }

    /// Fold the consultation, publish its render, and then commit the settlement of every turn
    /// that is ready, all under the one hold of the lock.
    ///
    /// The order is the commitment: a turn's `settled.json` is written only after the transcript
    /// holding its final section is in place, so a reader that finds the marker finds the bytes
    /// it vouches for.
    /// A crash between the two leaves the transcript published and no marker, and the next fold
    /// renders the same frozen input and writes it.
    /// A marker is never rewritten.
    /// The newest turn's `auto-resume.json`, when it gets one, is written just before its
    /// marker.
    pub(super) fn fold_and_publish(&self, meta: &Meta) -> Result<Published, RunError> {
        let state = self.fold_run(meta)?;
        let rendered = RunStore::render(meta, &state);
        self.publish_transcript(&rendered.text)?;
        for span in &rendered.spans {
            let (Some(Finality::Ready(frozen)), Some(turn)) = (
                state.finality.get(&span.index),
                state
                    .transcript
                    .turns
                    .iter()
                    .find(|turn| turn.index == span.index),
            ) else {
                continue;
            };
            let marker = self.store.turn_dir(&self.run_id, span.index).settled();
            // Every writer holds the lock, so nothing can create the marker between this check
            // and the write.
            if marker.exists() {
                continue;
            }
            // Decided before the marker is written, so a reader that finds the settlement finds
            // whether agentmux resumes the consultation after it too.
            if state
                .newest
                .is_some_and(|newest| newest.index == span.index)
            {
                self.schedule_auto_resume(&state, span.index, &turn.outcome)?;
            }
            let record = SettledRecord {
                version: SettledRecord::VERSION,
                turn_index: span.index,
                outcome: turn.outcome.clone(),
                settled_by: frozen.settled_by,
                events_len: frozen.events_len,
                recovered_bytes: frozen.recovered_bytes,
                transcript_start: span.start,
                transcript_end: span.end,
            };
            write_json(&marker, &record)?;
            tracing::debug!(run_id = %self.run_id, turn = span.index, "turn settled");
        }
        Ok(Published { state, rendered })
    }
}

/// What [`LockedRun::fold_and_publish`] folded and published.
#[derive(Debug)]
pub(super) struct Published {
    pub(super) state: RunState,
    pub(super) rendered: Rendered,
}
