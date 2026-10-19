# 0009. The rendered transcript is append-only, and a settled turn's fold input is frozen

Status: Accepted

## Context

Several readers observe a consultation — `status`, `tail`, `result`, a restarted server — and each
folds the turn's capture into the rendered transcript, which `tail` pages by byte offset. Both CLIs
outlive their closing event, and a descendant of the child may keep writing to the capture files
after the child is gone. A reader that re-read those files later could render a different transcript
from one it already published, and a `tail` cursor would then point into text that changed.

## Decision

The rendered transcript is append-only for the life of a consultation, and a turn's fold input is
frozen once something settles it:

- A terminal stream event ends the fold. A terminal stream outcome takes precedence over an exit
  record; process observation settles only a turn that neither has settled.
- When no terminal stream event has settled the turn, an observed departure records the capture
  length, exit status and stderr tail once (`ExitRecord::Exited` in
  `crates/agentmux/src/run/records.rs`). Later folds reread only the recorded capture prefix and use
  the recorded settlement evidence.
- Cancellation marks intent under the lock, attempts to stop the child with bounded waits, then
  records the capture boundary under the lock (`ExitRecord::Cancelled`). A cancellation record
  freezes the capture; it does not prove the child has departed.
- A Codex reply recovered from `last-message.md` is only ever added to the newest turn and is
  persisted in `recovered.md` before it is published, so it can neither land above a later turn nor
  go missing once rendered. Text that cannot be kept is not rendered.
- Whatever record settles a turn, the turn is re-folded from exactly the length that record froze,
  so two observers publish the same bytes.
- The consultation header contains only creation-time values; mutable status never enters it.
- `RunStore::remove` and `RunStore::sweep` refuse a run while a recorded child may still be alive,
  including a child with a cancellation record. Unknown liveness blocks deletion.
- A follow-up is refused (`NotResumable::AwaitingDelegateExit`) until the newest turn's delegate has
  left, because both CLIs save the session it resumes on their way out.

## Consequences

- `every_fixture_renders_monotonically_over_its_prefixes` (`crates/agentmux/tests/capture.rs`)
  checks every fixture at every line cut.
- Later renders may only append bytes. Recovered text must be persisted before publication and
  appended after the newest turn's footer; mutable observations such as the stderr snapshot
  `RunStatus::delegate_stderr` remain outside the transcript.
