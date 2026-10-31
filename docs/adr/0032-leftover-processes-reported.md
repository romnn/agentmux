# 0032. A delegate is told its leftovers may be stopped, and what a driven turn left running is reported

Status: Accepted

## Context

A delegate can start a long job in the background — `nohup … &` — and end its turn. What it
leaves running does not outlive the turn for long: the app-server driver sweeps every descendant
still alive once its server has exited ([0018](0018-codex-app-server-driver.md)), `codex exec`
kills its unified-exec process groups as it shuts down, and Claude Code stops the background tasks
it tracks. The job's work is then incomplete, and nothing the delegate said, nothing in the
transcript, `status` or `settled.json` says so; an orchestrator waits for output that will never
come.

Only the delegate can avoid starting such a job, and only agentmux's driver sees what was still
running when the server had gone. What the driver sees is not all the turn's: codex starts its MCP
servers as direct children of the server, at any time during the turn (its MCP clients start
asynchronously, possibly after `turn/start`), and runs each command in a shell that is a direct
child of the server too, which codex stops itself on shutdown. So a start-time cutoff cannot tell
the turn's work from codex's own processes, and the parent the process table records can.

## Decision

**The first question of every consultation carries a notice.** `start`, and `continue_fresh`,
which begins its consultation through the same step, append to the caller's question, after a
blank line and a `---` rule, a short vendor-neutral notice (`run::TURN_END_NOTICE`): processes
still running when the turn ends may be stopped, so run needed work in the foreground or keep
polling it until it exits, do not leave it running with `nohup` or `&`, and say plainly if work is
unfinished. It is written into `question.md`, which is the child's stdin in every launch mode, so
the transcript shows exactly what was delivered, and the question's first line, which `list`
shows, is unchanged. Follow-ups and the automatic "continue" resume do not repeat it; their
session already holds it. It is never sent as `developer_instructions`, which would replace the
user's own configured value.

**The app-server driver reports what the turn left running, on Unix.** In `stop_server`
(`crates/agentmux/src/appserver/driver.rs`):

- Before closing the server's input, it reads its descendants with each one's parent, so the
  server's direct children are known.
- The cohort is every process found alive — pid and start time matching, not a zombie — by the
  sweep's reads after the server has exited, less the server itself and less the processes that
  were its direct children in that read; a process the sweep meets later, forked meanwhile, joins
  it too (`appserver::leftovers::Cohort`).
- Once the sweep ends, nothing left or its deadline passed after `SIGKILL`, the driver waits
  briefly (`REPORT_SETTLE`, part of `STOP_BUDGET`) and looks once more: each member is
  `no_longer_running` or `still_running`. Who stopped one that is gone is not claimed.
- Each process is kept by pid, start time and its process name, escaped and bounded to 64
  characters, never by its arguments, which can carry secrets; at most 50 are named, and the rest
  are counted by fate under `omitted`.
- When the cohort is not empty, the driver writes the record once, atomically, to the turn's
  `left-running.json` (`left_running::LeftRunning`, `version` 1), before it exits, on every path
  that started the server. It writes nothing for a cancelled turn, nor once the `cancelling` marker
  exists: `cancel` creates the marker only while the turn has not ended, before it records the
  cancellation, so a turn whose record exists never gains an `ExitRecord::Cancelled`.

**The fold reads it like the recovered reply.** `Turn::left_running` is read only for a turn
whose outcome is terminal and whose delegate has gone, and never for a turn with an
`ExitRecord::Cancelled`, even one whose terminal event supplied the outcome: a cancelled turn's
leftovers are the cancellation's doing. The driver writes the record before it exits and never
again, so every later fold of the turn, committed or not, reads the same file, and `settled.json`
needs nothing new ([0009](0009-append-only-transcript.md)). A record that cannot be read, does not
parse or has another version is treated as absent and logged at debug; it never fails a fold.

**It is said after everything else the turn renders.** The transcript's epilogue, a warning note
naming the processes, how many still ran and that work in them may be incomplete, renders after
the footer and after a recovered reply, and only once the turn is terminal: it exists only once
the delegate has gone, which is after everything else in the turn is final. The terminal stream
event is never delayed for it, because outcome, steering admission and cancellation all depend on
that event. `RunStatus::left_running` carries every turn's report, serialised for `--json`, and
`Warning::LeftRunning` puts one `warning:` line per turn near the top of the MCP and CLI status.

## Consequences

- `exec` and Claude turns get the notice only: no record is written and their sweep is unchanged,
  though both CLIs stop much of what they leave themselves.
- On Windows the job object stops what the turn left and nothing is enumerated, so no record is
  written.
- On macOS the driver cannot adopt orphans, so a process started and orphaned between two of its
  once-a-second records — a backgrounded job whose shell exits at once — is neither stopped nor
  reported.
- A process whose parent the pre-EOF read records as the server is never reported, whatever it
  runs; when the server crashes, its children are reported only once the system has moved them to
  the driver, which a read in the moment of the crash may not yet see.
- A delegate may still ignore the notice; the report is what tells the caller then.
