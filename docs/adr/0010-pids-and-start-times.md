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

Fresh process observations never revise a settled transcript outcome
([0009](0009-append-only-transcript.md)). Reaping and departure checks may still run for cleanup,
resumability and safe removal.

## Consequences

- Every reader that signals, waits on or reports a delegate goes through the launcher's liveness
  check, never a bare pid.
- On Windows, launcher signalling reaches only the delegate process. Driven Codex turns additionally
  use a kill-on-close job to contain descendants ([0018](0018-codex-app-server-driver.md)); direct
  CLI launches have no equivalent containment.
