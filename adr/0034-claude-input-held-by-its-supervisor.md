# 34. A Claude turn reads its input from its supervisor, so it takes a steer until its result

**Status:** accepted · **Date:** 2026-11-27

## Context

`steer` reached only Codex turns: [0018](0018-codex-app-server-driver.md) drives those over
`codex app-server`, and recorded that `claude --print` offers no channel into a turn that has
started. That holds for `claude --print` given its question as text, which is how every Claude turn
was launched.

Claude Code also reads its input as stream-json lines, for as long as stdin is open
(`--input-format stream-json`). Probes of claude 2.1.332 established what that channel does:

- A user line sent with `"priority":"next"` while a tool runs is taken once the tool has finished,
  before the next model call, and no tool is cancelled.
- With `--replay-user-messages`, each user message the CLI takes is echoed on stdout as a `user`
  event with `isReplay: true` and the `uuid` its sender gave it, at the moment it is taken.
- With stdout a regular file and stdin held open, every event reaches the file as it happens.
- Closing stdin after the `result` makes the CLI exit 0.
- A message sent before the `result` but taken after it, like a background task that finishes
  after it, is run as a further CLI turn in the same process, with an `init` and a `result` of its
  own, even after stdin has closed.
- Each `result.usage` covers its own CLI turn, while `total_cost_usd` is the process's running
  total.
- `--resume` works with stream-json input.

A consultation must survive the CLI or MCP server that started it, and the capture file is the
turn's state: the child writes its stream through a file agentmux hands it, so nothing is lost when
agentmux is not there (`crates/agentmux/src/launch.rs`).

## Decision

**Claude writes its own capture and stays the recorded delegate.** It is launched as
`claude --print` is, by the launcher, with stdout on `events.jsonl`, in a process group of its own,
and recorded in `launch.json`. Its liveness, exit status, cancellation and settlement are what they
are for any delegate ([0025](0025-eager-settlement-and-terminal-waiting.md)), and nothing but Claude
writes to its capture. Rewriting Claude's stream on its way to the file was rejected: a relay
between Claude and the capture has to decide when the stream is over, how much to buffer, and how
cancellation is ordered against queued output, and every answer to those cost a defect.

**The turn's supervisor holds Claude's input.** A store with a supervisor, which the `agentmux`
binary always has, launches every Claude turn, isolated or not, with the arguments `--print` would
have had plus `--input-format stream-json --replay-user-messages --permission-prompts none`: nobody
answers a permission prompt over the open input, so one is denied rather than waited for. The
turn's invocation record says `claude_stream_input` (`LaunchMode::ClaudeStreamInput`). Before the
supervisor enters `spawning` it makes the pipe, reads the question and parses the turn's launch id;
a failure there is a turn that never started (`ExitRecord::NotStarted`). It hands Claude the read
end as stdin (`LaunchInput::Pipe`) and keeps the write end (`run::claude_input`):

- A writer thread writes one line at a time: the question first, under the turn's launch id, then
  each steer, `"priority":"next"`, under the id of its request in the turn's mailbox, the mailbox
  of [0018](0018-codex-app-server-driver.md), whose request stems now end in a UUID. Never
  `"now"`, which ends the CLI turn under way. A request is claimed only when the writer is free,
  so one not yet claimed can still be withdrawn.
- The supervisor's watch loop goes on looking for a cancellation and for Claude's exit while a
  line is written, so a Claude that stops reading cannot hold up a cancellation the caller only
  marked. Between looks it reads the lines Claude has completed in its capture, as far as the
  capture reached when it began reading.
- A `result` closes the input: the writer drops the pipe once the line in progress is written, and
  the requests still waiting are rejected under the run's lock, which `steer` submits under. A
  cancellation and Claude's exit close it too. The supervisor and the fold read the input's
  boundaries the same way, from a line's `type` and a `system` line's `subtype` alone
  (`stream::claude::InputBoundary`).
- The supervisor answers a request only to reject it, when it was never written.

Without a supervisor, as for a library embedder, or where `claude` is found only as a Windows
`.cmd` shim, whose recorded process is the command interpreter that a cancellation would stop in
Claude's place, a Claude turn runs as `claude --print` with its question file as stdin and takes
no steer (`UnsteerableReason::ClaudePrint`).

**A steer is accepted when the caller's own fold shows Claude took it.** The fold of such a turn is
given the ids its input was sent under (`stream::StreamInput`): the launch id from
`supervisor.json`, and the ids of the requests in its mailbox, listed after the capture is read. An
echo of the question is left out, since the question renders already; an echo under a request's id
is `### user (steer)`, where Claude took it; any other echo is read for what it says, as any `user`
text is, and is never a steer. Ids are compared as UUIDs. `steer` then folds the turn under the run
lock, as every reader does and no further than a settled turn's frozen capture, until:

- the fold shows the steer, which is acceptance, so a steer reported taken is one the transcript
  shows;
- the supervisor rejected it;
- the turn ended without it, which is unconfirmed: the turn ended before Claude took it;
- thirty seconds passed since it was claimed, which is unconfirmed too: Claude takes a message
  only between model calls, so it may still arrive and should not be sent again.

The supervisor's death does not end the wait, since Claude may still take a line it was handed.
`status` offers a steer, and `steer` admits one, on one predicate (`RunStore::accepts_steers`): the
turn is not over, its input is open (`TurnInput::Open`, from `init` until the first `result`), no
cancellation is marked, and the supervisor and Claude are both positively alive. A refusal says
which: still starting, finishing, or out of reach because the supervisor has gone
(`UnsteerableReason::ClaudeInputLost`).

**No `result` ends the turn; its exit record does.** A `result` closes the input, and its outcome
becomes the turn's candidate (`Fold::last_result`), which Claude going on past it (an `init`, an
assistant message or a `user` event of the main conversation) or a `result` that cannot be read
clears. What a hook reopened, and what the delegate was doing, is reset at each `result`, so a
further CLI turn is judged on its own: the fold marks what Claude said after a reopening
(`MessageSource::AfterReopening`) only in the CLI turn that was reopened. A failure Claude went on
past stays in the transcript as a note. Tokens are summed across the `result`s and the latest cost
is kept, whose scope is its release's ([0031](0031-spend-derived-from-vendor-reports.md)). Once the
exit record is there (`RunStore::settle_outcome`):

- a cancellation of the running turn is the outcome, as is a launch that never happened;
- an exit known to be abnormal decides over a success, which Claude did not finish, but not over a
  failure Claude reported itself;
- after a clean exit, or one whose status nobody saw, the candidate stands;
- with no candidate, the exit record explains the turn as it does a direct launch's.

A turn recorded without a mode or as `exec`, and a Codex turn over the app-server, fold as before.

## Consequences

- This supersedes the statement in [0018](0018-codex-app-server-driver.md) that `claude --print`
  offers no channel into a turn that has started: given its input as stream-json, it does. It
  narrows [0009](0009-append-only-transcript.md) for these turns: no stream event is terminal, and
  the exit record settles the turn.
- Every Claude turn the `agentmux` command launches takes a steer until its `result`. A steer Claude
  takes after it is answered as a further CLI turn, and that answer is the turn's; so is the answer
  after a background task finishes, where the first `result` used to end the turn.
- A turn ends a moment after Claude exits rather than at its `result`, and later while a late steer
  or a background task keeps the CLI working. A Claude that never exits after its `result` leaves
  the turn finishing until it is cancelled.
- A steer sent during a long tool call is taken only when the tool returns, so a caller can be told
  it was not taken within thirty seconds although it arrives later.
- A supervisor that dies closes Claude's input; Claude finishes what it has, unsteered, and the
  next observer settles the turn.
- Cancellation, and what a cancellation reaches on Windows or outside Claude's process group, are as
  for any direct launch.
- agentmux v0.0.13 and older reject the `claude_stream_input` launch mode as a corrupt record, so
  they cannot read a consultation with such a turn, which narrows what
  [0030](0030-refuse-to-launch-from-a-replaced-executable.md) promises across an upgrade: restart
  every MCP server started before it. Reading the capture as `exec` instead would end the turn at
  its first `result` and lose what Claude answered after it.
