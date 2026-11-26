# 34. A Claude turn is driven over stream-json input, so it takes a steer until its result

**Status:** accepted · **Date:** 2026-11-26

## Context

`steer` reached only Codex turns: [0018](0018-codex-app-server-driver.md) drives those over
`codex app-server`, and recorded that `claude --print` offers no channel into a turn that has
started.
That holds for `claude --print` given its question as text, which is how every Claude turn was
launched.

Claude Code also reads its input as stream-json lines, for as long as stdin is open
(`--input-format stream-json`).
Probes of claude 2.1.332 established what that channel does:

- A user line sent with `"priority":"next"` while a tool runs is taken once the tool has finished,
  before the next model call, and no tool is cancelled.
- With `--replay-user-messages`, each user message the CLI takes is echoed on stdout as a `user`
  event with `isReplay: true` and the `uuid` its sender gave it, at the moment it is taken.
- Closing stdin after the `result` makes the CLI exit 0.
- A message sent before the `result` but taken after it, like a background task that finishes
  after it, is run as a further CLI turn in the same process, with a `result` of its own, even
  after stdin has closed.
- Each `result.usage` covers its own CLI turn, while `total_cost_usd` is the process's running
  total.
- `--resume` works with stream-json input.

As for Codex, a pipe needs a writer for as long as the turn runs, and a consultation must survive
the CLI or MCP server that started it.

## Decision

**The driver is the delegate, as for Codex.**
`agentmux claude-stream <plan>` (`claude_stream::drive`) is launched exactly where
`claude --print` was: by the same launcher, into the same turn directory, with the same
environment and under the turn's supervisor.
It starts nothing until the turn's launch record names it (`drivers::confirm_launch`, which the
app-server driver shares), reads the question on stdin, and runs `claude` with the arguments
`--print` would have had plus `--input-format stream-json --replay-user-messages`: in the driver's
process group on Unix, and in the kill-on-close job on Windows (`drivers::contain_descendants`).
A store drives every Claude turn once it is given the driver (`RunStore::with_claude_driver`),
isolated or not, and the binary always gives it, whatever `--codex-launch` says; isolation, the
billing scrub and the permission flags are unchanged.
The turn's invocation record says `claude_stream` (`LaunchMode::ClaudeStream`).

**Input is one message at a time.**
The question is the first line, with a fresh `uuid`; each steer is one line with its own `uuid`
and `"priority":"next"`.
The mailbox of [0018](0018-codex-app-server-driver.md) is served from Claude's `system`/`init`
until its first `result`, and the next request is claimed only once the one before has been
written, so a request not yet claimed can still be withdrawn.
A claim waits behind a marker the driver queues after what Claude has already written, so a
`result` it has read closes the input before another request is claimed, while the driver goes on
handling one line at a time and looking for a cancellation between them.
The driver and the fold recognise those two lines the same way, from a line's `type`, and a
`system` line's `subtype`, alone (`stream::claude::InputBoundary`), so a `result` whose body
drifted closes the input for `status` as it does for the driver; the driver reads only those
fields and the replay's, and skips the rest of each line unread.
At the first `result` the driver stops claiming, rejects the requests still waiting, and closes
stdin once the write in progress is through; a write still in progress ten seconds later is not
waited for and Claude is stopped, while that message keeps its claim: a replay read before the
turn ends accepts it, a failed write rejects it, and otherwise it is unconfirmed.

**A steer is accepted once Claude's replay of it is in the capture.**
Every line Claude writes goes into the capture as it arrives, but for two replays: the question's
is dropped, and a steer's is replaced by `{"type":"agentmux.steer","text":…}`, the capture event
the app-server driver writes, after which the steer's reply is `Accepted`.
Once Claude has exited and its output has been drained, the driver writes
`{"type":"agentmux.end","status":…}` with Claude's exit status, and answers every steer written
but never replayed as unconfirmed.
The drain ends at a marker the driver queues behind what it had read when the drain window
closed, as the app-server driver's does, so a process that inherited Claude's stdout and keeps
writing to it cannot hold the turn open.
A caller that has heard nothing 30 seconds after the driver claimed its steer is told Claude did
not confirm delivery and that it may still arrive, so sending it again may deliver it twice.

**The fold reads the launch mode.**
`fold_turn` reads the turn's invocation record before it folds, and `stream::fold` takes its
mode.
Under `ClaudeStream` a `result` ends nothing and `agentmux.end` does, and only there are the
`agentmux.*` lines read; in any other capture they are drift.
What a `result` concludes of the CLI turn before it, whether a hook reopened it and what the
delegate was doing, is reset at each `result`, so a further CLI turn is judged on its own.
The outcome is the last `result`'s, with its classification, except that the turn fails when the
CLI exited uncleanly after a successful `result`, and when an `agentmux.steer` or assistant output
follows the last `result` with no `result` after it, because neither is an answered turn.
A capture with no `result` is left to the turn's exit record, as a direct launch's is, so the
diagnostics are the same; the driver exits with Claude's exit code for that.
Tokens are summed across the `result`s and the latest cost is kept, whose scope is its release's
([0031](0031-spend-derived-from-vendor-reports.md)).
The turn takes a steer from `init` until its first `result` (`TurnInput::Open`), which
`RunStore::accepts_steers` reads for both `status` and `steer`; after it a steer is refused as
finishing.
A direct launch's capture, and a turn recorded before this decision, fold as before.

**A cancellation stops Claude, and what Claude runs, from inside.**
Once the driver sees the `cancelling` marker, it rejects the requests still waiting, stops
claiming, asks Claude to stop, makes it after three seconds, waits for it to exit, and exits
without writing `agentmux.end`, so the store records the cancellation as it does for
`claude --print`.
Whenever the driver stops Claude, on a cancellation or otherwise, it first reads Claude's
descendants, and once Claude has gone it stops those still alive, recognised by pid and start
time, with the app-server driver's sweep (`drivers::sweep`): politely, then by force, within
three seconds.
Each pass of the sweep also stops every other member of the process group the driver leads, the
launch group, which holds a helper whose parent has exited though it is below nobody; the
store's group signal no longer reaches that group once the driver has gone.
`RunStore::stop_delegate` gives each driver its own budget (`claude_stream::STOP_BUDGET` here)
and five seconds more before it signals anything.
What the sweep stops is not reported, as nothing is for a direct launch
([0032](0032-leftover-processes-reported.md)).

## Consequences

- This supersedes the statement in [0018](0018-codex-app-server-driver.md) that `claude --print`
  offers no channel into a turn that has started: given its input as stream-json, it does.
- Every Claude turn the `agentmux` command launches takes a steer until its `result`.
  A Claude turn an older agentmux launched, or a store with no Claude driver, still runs as
  `claude --print` and refuses a steer with `UnsteerableReason::ClaudePrint`.
- A steer Claude takes after its `result` is run as a further turn of the CLI, whose answer is
  the turn's.
- A process Claude starts in a process group of its own after the driver has read its
  descendants, while it is stopping, is out of the sweep's reach once Claude has gone.
- agentmux v0.0.13 and older reject the `claude_stream` launch mode as a corrupt record, so they
  cannot read a consultation with a driven Claude turn, which narrows what
  [0030](0030-refuse-to-launch-from-a-replaced-executable.md) promises across an upgrade: restart
  every MCP server started before it.
  Reading the capture as `exec` instead would end the turn at its first `result` and lose what
  Claude answered after it.
- A driver that dies on its own leaves Claude running in its process group, while the store,
  which tracks the driver, takes the turn as over.
- A Claude stopped because it would not read its input after its `result` ends its turn failed,
  even when that `result` was a complete answer.
