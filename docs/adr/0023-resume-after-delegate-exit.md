# 0023. A follow-up resumes a session only once its delegate has exited

Status: Accepted

## Context

A turn is over the moment its event stream says so, but the process behind it is not. Both CLIs
save the session on their way out, and Codex writes its closing message file after the turn's last
event. A follow-up that resumed as soon as the stream finished could reopen a session missing the
end of the conversation, or run a second process beside the first while it is still writing that
session.

## Decision

**A follow-up is admitted only on evidence that the newest turn's delegate has exited.**
`RunStore::delegate_has_exited` (`crates/agentmux/src/run/mod.rs`) accepts as evidence an `Exited`
or `NotStarted` exit record, an unsupervised turn with no launch record, a child the launcher
reaps, or a recorded child the launcher reports `Gone`. A supervised turn whose delegate is not
recorded yet has not exited: its supervisor is still starting it. A child whose identity cannot be
checked has not exited ([0010](0010-pids-and-start-times.md)): it may be the delegate still saving.

`resumable_session` (`crates/agentmux/src/run/spawn.rs`) is the one predicate behind both
`follow_up` and `status.resumable`. `follow_up` asks it after publishing the consultation and
committing every settled turn under the admission lock, and the evidence it reads is that commit:
the newest turn is settled, which the fold decides from one observation of its delegate, so the
turn it continues has its `settled.json`, with any reply its CLI wrote on the way out, before the
next one is claimed ([0025](0025-eager-settlement-and-terminal-waiting.md)). It refuses with
`NotResumable::AwaitingDelegateExit` when the newest turn is terminal but not yet settled, after
the checks that the turn is finished, was not cancelled and saved its session. Only
the newest turn is asked, because its child is the one whose session a follow-up continues, and
asking costs a process lookup.

The same evidence decides when a Codex turn's rate-limit lookup is final: a rollout read before the
delegate exited may be incomplete, so a miss is cached only after it has
([0004](0004-quota-from-vendor-sources.md)).

## Consequences

- A caller that asks for a follow-up a moment after a turn ends may be told to ask again in a few
  seconds; `status` shows the turn as not yet resumable until then.
- A delegate that never exits after its turn — a hung save, a child that ignores its signals —
  keeps the consultation from continuing until it is cancelled or killed.
- Removing a run and the retention sweep ask the same launcher questions for a stricter purpose:
  whether any recorded child, a cancelled one included, is still alive
  ([0020](0020-run-state-locking.md)).
