# 0025. A supervisor settles each turn as its delegate leaves, and waits return only then

Status: Accepted

## Context

A turn is over when its stream says so, but agentmux used to learn that only when somebody asked:
the transcript was rendered and the turn settled on read. A delegate that finished while no call
was in flight left nothing on disk to say so, so a file watch saw nothing and an idle caller was
told nothing. The waits a caller had were bounded observations: `result` with `wait_seconds`
returned at agentmux's 540-second cap with the turn still running, and Claude Code, which reports
a backgrounded call as done when the call returns, announced a consultation as completed while it
was still working.

A host cannot be made to wake an idle agent from inside agentmux. What agentmux can guarantee is
that a turn's end is on disk the moment its delegate has gone, and that a caller who waits is
answered exactly then and never earlier.

## Decision

**Every turn has a supervisor.** A store opened with `RunStore::with_supervisor` — the binary
always opens one so — launches `agentmux supervise <plan>` (`crates/agentmux/src/run/supervise.rs`)
in the delegate's place, detached as a delegate is. The supervisor starts the delegate itself —
the vendor's CLI, or the app-server driver ([0018](0018-codex-app-server-driver.md)), which is
unchanged — holds its handle, waits for it to go, and settles the turn. A library embedder that
names no supervisor launches delegates directly, and their turns settle on read.

- The initiating process resolves everything, writes `invocation.json` as before, and writes
  `supervisor-plan.json`: the store, the turn, a launch id, and the delegate's program, arguments
  and working directory. The resolved environment travels as the supervisor's own environment and
  is handed on unchanged, so no credential is written down and nothing is resolved twice.
- `supervisor.json` records the supervisor's own pid and start time
  ([0010](0010-pids-and-start-times.md)), the launch id and a phase, before the lock is released.
  `launch.json` names the delegate, as before. The supervisor starts nothing unless the record
  names its launch and its own pid, and spawns and records the delegate under one hold of the run
  lock, moving from `prepared` to `spawning` just before.
- The delegate runs in a process group of its own, so a supervisor's death does not take it
  along. While the supervisor lives, other observers leave a departure to it, because only it can
  record the exit status; once it has gone, the next observer settles the turn in its place. A
  supervisor gone never means a turn done.
- A delegate the supervisor cannot start is a failed turn (`ExitRecord::NotStarted`), kept with
  the reason. A start that fails before the turn is recorded still leaves no run behind.
- A turn that failed because the model was overloaded is not the end of its supervisor: it waits
  and resumes the consultation as the next turn, which its plan's `auto_resume` says how to run
  ([0027](0027-automatic-resume-after-overload.md)).

**One step settles a turn.** `RunStore::finalize_turn` (`crates/agentmux/src/run/settle.rs`)
takes the run lock, adopts a `settled.json` already there, or folds the consultation from its
frozen inputs, publishes the transcript, and then writes the turn's `settled.json`. Every fold
that publishes — every read, the supervisor, a waiter, `cancel`, a follow-up's admission — commits
the turns that are ready the same way, which is also how an older store and a turn whose
supervisor died are settled. A turn is ready once its outcome is terminal and either a record
settled it or its delegate has gone: both CLIs save the session, and Codex writes the reply a lost
one is recovered from, on their way out. A cancellation freezes the turn whether or not its
delegate left, even one whose stream ended before the delegate could be stopped. A follow-up asks
for both — the turn settled and its delegate gone, from the one observation the fold made — so a
cancellation the delegate survived is settled but not continued.

**`settled.json` version 2** is written last, whole, under the lock, and never rewritten:

```json
{
  "version": 2,
  "turn": 1,
  "outcome": { "state": "completed", "usage": {} },
  "settled_by": "terminal_event",
  "events_len": 5641,
  "recovered_bytes": null,
  "transcript_start": 275,
  "transcript_end": 411
}
```

`turn` is the turn's number, counted from 1 as its directory `turns/0001` and the transcript's
headings count it; `outcome` is the serialized `Outcome`, never `running`; `settled_by` is `terminal_event` or
`exit_record`; `events_len` and `recovered_bytes` are the capture prefix and recovered reply the
turn was folded from, and from then on its frozen input
([0009](0009-append-only-transcript.md)); `transcript_start` and `transcript_end` bound the turn's
section of `transcript.md`, and everything before `transcript_end` is final. A reader
(`SettledTurn`) refuses another version, a running outcome, a reversed range, or a capture or
recovered reply shorter than it commits. Every read of a turn checks its marker before folding
from it; the transcript, a cache a fold writes again, is checked where a page of it is read.

**Waits are pinned and terminal-only.** A wait chooses its turn once, by one-based number or as
the newest turn at the start, and never moves to a follow-up begun meanwhile. It polls the marker
— a tenth of a second backing off to half of one, so it returns within half a second of the
settlement — and settles the turn itself only where nothing else will. It chooses its turn from which
records exist, reading none of them, so a damaged record elsewhere stops nothing until the wait
looks at its own turn. It never waits for the run lock: settling a turn itself is one attempt that
another holder of the lock turns into "not yet", so the polling loop keeps its one cadence and its
one deadline, and stays cancellable. A marker whose transcript has gone is settled already, and
the wait writes the transcript again under the lock before returning it. Under `--any` the first turn found settled, in the order given, ends the wait before
any later one is looked at. The bounded
`wait_until_terminal` behind `ask`, `result` and `follow_up` is pinned the same way and stays an
observation of the stream.

- The MCP `wait` tool returns only a settled turn and its transcript, through `transcript_end`,
  with no deadline of its own, under a header built from the settlement, the consultation's
  creation-time record and the waited turn's own launch record, whose delegate line names what
  that turn ran with ([0026](0026-delegate-permissions.md)), so a later turn neither delays nor
  fails it. It sends `notifications/progress` every 30 seconds to a request that
  carries a progress token, which keeps Claude Code's idle timer from ending the call; a host that
  bounds tool calls regardless, as Codex does with `tool_timeout_sec`, still ends it. Cancelling
  the request ends the wait, never the consultation.
- `agentmux wait <run>... [--turn N] [--any | --all] [--timeout D]` blocks with no deadline by
  default and exits 0 completed, 10 failed, 11 cancelled, 124 timed out, 1 for an agentmux
  failure and 2 for arguments naming nothing. Under `--all`, a failure outweighs a cancellation.

## Consequences

- A delegate that finishes with nobody asking leaves its final transcript and `settled.json` on
  disk within moments, which a file watch or a background `agentmux wait` sees.
- Claude Code: launch through `start`, then either call `wait`, or run `agentmux wait` with
  `run_in_background`, whose exit is the completion notice. Codex: run `agentmux wait` in a shell
  and wait for that command.
- A supervisor that dies between spawning its delegate and recording it leaves a turn nothing can
  tell is running. Such a turn is neither settled, cancelled, followed up nor removed — a
  cancellation recorded over it would read as the end of a turn that may still be billing — and
  a wait or a `cancel` on it fails with `RunError::UncertainStart`; closing the window needs a launch that creates and records a
  process atomically, which an ordinary spawn does not.
- `RunStore::remove` and the sweep also refuse a run while a turn's supervisor is alive, or one
  died while spawning ([0020](0020-run-state-locking.md)).
- Settlement is crash-safe in the order it writes files, not durable across power loss: nothing is
  synced.
- On Windows the supervisor and every delegate are created detached, in a process group of their
  own; descendants of a directly launched CLI are still not contained. `CreateProcess` hands a
  child every inheritable handle of its parent whatever its stdio, so every spawn first clears the
  inherit flag on the spawning process's own standard handles
  (`launch::keep_std_handles_from_children`): otherwise a supervisor started by `agentmux start`
  under a caller capturing its output, or by an MCP server, would hold the caller's pipes open
  for the whole turn.
