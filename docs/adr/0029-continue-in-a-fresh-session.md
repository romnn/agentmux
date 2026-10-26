# 0029. A session that cannot go on is continued by a new consultation that reads the old one

Status: Accepted

## Context

A provider's guardrail can refuse a turn, which fails as `content_flagged`. A rephrased follow-up
usually gets past a refusal of the wording, but sometimes the session's own context is what the
filter refuses, and every later turn in that session fails however neutral its question. The only
way forward is a new consultation, and handing the work over by hand loses things: the old
transcript's path, the uncommitted work in the working tree, what had been decided.

## Decision

**`continue_fresh` starts a new consultation from the old one's settled state, whose first
question points the delegate at the old transcript and working tree, and the two records link each
other.** `RunStore::continue_fresh` (`crates/agentmux/src/run/continue_fresh.rs`) refuses with
`RunError::StillRunning` until the old consultation's newest turn has settled, so the transcript
it points at is published. It starts the new consultation through `start`, as the delegate the old
newest turn ran as, with the model the old consultation asked for, so this machine's model rules
and limits apply to it as to any new consultation; its working directory, environment, pricing,
retention, `codex exec` choice and current label carry over. The first question names the old
consultation and why it could not go on, gives its transcript's path, says the working directory
may hold its uncommitted work, and ends with the caller's question, or with carrying on.

The transcript is pointed at, not copied into the question: a long review's transcript can be
larger than a prompt should be, and the delegate reads what it needs. A Claude delegate reads only
its working directory unless granted more, so every turn of a consultation that continues another
is launched with `--add-dir` for the old consultation's directory; Codex's sandboxes read anywhere.

The new consultation's `meta.json` records `continues`, which its transcript header shows, and the
old one's `continued-by.json` names the newest consultation that continued it, written under the
old run's lock ([0020](0020-run-state-locking.md)). `status` reports both, and
`content_flagged_in_a_row`, the newest turns refused in a row. From
`CONTENT_FLAGGED_STREAK`, two, every reader suggests `continue_fresh` in place of a follow-up,
until something has continued the consultation.

## Consequences

- The old consultation is unchanged and can still be followed up; continuing it afresh only adds
  a pointer to its successor.
- The new consultation depends on the old one's directory: if the old consultation is pruned or
  swept first, the brief names a transcript that is no longer there.
- Continuing the same consultation twice starts two successors, and the old one names the newer.
