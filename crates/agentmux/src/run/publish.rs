//! Rendering a consultation and publishing the render.

use crate::transcript::render_turn;

use super::error::RunError;
use super::lock::LockedRun;
use super::records::Meta;
use super::status::describe_delegate;
use super::{RunState, RunStore};

impl RunStore {
    /// Render the whole consultation as Markdown.
    ///
    /// The result is **append-only for the life of the consultation**, which is the contract
    /// [`RunStore::view`] pages by byte offset and `tail` hands back as a cursor.
    /// Two things make that true, and both are easy to break:
    ///
    /// - this header is built only from values fixed when the consultation was created, so
    ///   nothing here may ever carry state, elapsed time or a message count;
    /// - each turn's body grows only at its end, which
    ///   [`crate::transcript::render_turn`] documents and enforces, and the fold behind it never
    ///   reads past a turn's settlement, which [`settle`] documents and enforces.
    ///
    /// Adding a mutable field to the header would silently invalidate every outstanding cursor.
    pub(super) fn render(meta: &Meta, state: &RunState) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = write!(
            out,
            "# Consultation {}\n\n\
             - delegate: {}\n\
             - working directory: `{}`\n\
             - started: {}\n",
            meta.run_id,
            describe_delegate(&meta.delegate, meta.rewritten_from.as_ref()),
            meta.cwd.display(),
            meta.created_at.to_rfc3339(),
        );
        for turn in &state.transcript.turns {
            out.push_str(&render_turn(turn));
        }
        out
    }
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
    pub(super) fn publish(&self, text: &str) -> Result<(), RunError> {
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
        let temp = path.with_extension(format!("md.{}.tmp", uuid::Uuid::new_v4().simple()));
        let write = || -> Result<(), RunError> {
            std::fs::write(&temp, text).map_err(RunError::io("writing the rendered transcript"))?;
            std::fs::rename(&temp, &path).map_err(RunError::io("replacing the rendered transcript"))
        };
        let result = write();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}
