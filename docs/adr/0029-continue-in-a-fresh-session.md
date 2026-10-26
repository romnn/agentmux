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
it points at is published, and while agentmux has scheduled an automatic resume of it
([0027](0027-automatic-resume-after-overload.md)), which would run the old session beside the new
one. It holds the old run's lock ([0020](0020-run-state-locking.md)) from that check until the
successor is linked, so nothing can remove the old run or begin a turn of it in between.

The new consultation is admitted as `start` admits one, but as `Admission::Continuing`
(`crates/agentmux/src/run/spawn.rs`): it runs as the old newest turn ran — delegate, account,
effort, isolation and permissions, a turn recorded before isolation was kept counting as isolated
— and the machine file's defaults are not filled in again, as a follow-up's are not
([0026](0026-delegate-permissions.md)). Only the model rules and the limits apply afresh: the model
is the one the old consultation asked for, resolved under this machine's current rules, and a
permission beyond the current limits is refused. The working directory, the environment the caller
asked for, pricing, retention, `codex exec` choice and current label
([0028](0028-labels-belong-to-turns.md)) carry over. The first question names the old consultation
and why it could not go on, gives its transcript's path, starting with its last turns, says the
working directory may hold its uncommitted work, and ends with the caller's question, or with
carrying on.

The transcript is pointed at, not copied into the question: a long review's transcript can be
larger than a prompt should be, and the delegate reads what it needs. A Claude delegate reads only
its working directory unless granted more, so every turn of a consultation that continues another
is launched with `--add-dir` for the old consultation's directory; Codex's sandboxes read anywhere.

The new consultation's `meta.json` records `continues`, which its transcript header shows, and the
old one's `continued-by.json` names the newest consultation that continued it. `status` reports
both, and `content_flagged_in_a_row`, the newest turns refused in a row. From
`CONTENT_FLAGGED_STREAK`, two, every reader suggests `continue_fresh` in place of a follow-up: a
heuristic, since a count cannot say why the filter refused. Once something has continued the
consultation, every reader points at the successor instead.

## Consequences

- The old consultation is unchanged and can still be followed up; continuing it afresh only adds
  a pointer to its successor.
- The new consultation depends on the old one's directory: if the old consultation is pruned or
  swept first, the brief names a transcript that is no longer there.
- Continuing the same consultation twice starts two successors, and the old one names the newer.
