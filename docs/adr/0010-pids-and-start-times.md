# 0010. A pid on disk is trusted only with its start time

Status: Accepted

## Context

A delegate outlives the agentmux that started it, and a later agentmux — a restarted MCP server, a
terminal — has only the pid recorded on disk to find it by. A pid is recycled, and a recycled pid
that happens to lead a process group of its own passes every other test, so signalling or waiting on
it could reach an unrelated process.

## Decision

**A pid on disk is trusted only with the start time recorded beside it.** `Launched::started`
(`crates/agentmux/src/launch.rs`) comes from `sysinfo` at spawn. Without a retained child handle,
liveness after a restart requires the recorded start time to match, and a pid without one is never
taken to be the child; the process that spawned the child can still establish liveness through the
handle it retained.

A supervised turn records two processes, each with its own start time: the supervisor in
`supervisor.json` and the delegate it started in `launch.json`
([0025](0025-eager-settlement-and-terminal-waiting.md)). Each is checked on its own record, since
either may outlive the other; the supervisor holds the delegate's handle, so it alone collects the
delegate's exit status, and other observers leave a departure to it while it lives.

Fresh process observations never revise a settled transcript outcome
([0009](0009-append-only-transcript.md)). Reaping and departure checks may still run for cleanup,
resumability and safe removal.

## Consequences

- Every reader that signals, waits on or reports a delegate goes through the launcher's liveness
  check, never a bare pid.
- On Windows, every launched process is created detached and in a process group of its own, and
  launcher signalling reaches only the delegate process. Driven Codex turns additionally
  use a kill-on-close job to contain descendants ([0018](0018-codex-app-server-driver.md)); direct
  CLI launches have no equivalent containment.
