# 0006. A server denies its own harness's vendor, and the store enforces it

Status: Accepted

## Context

Each harness already spawns same-vendor subagents natively, in-process and under its own
supervision, which is cheaper and better managed than anything agentmux can start for it. An MCP
server registered in Claude Code should therefore offer only `codex`, and one registered in Codex
only `claude`. A denied vendor still advertised would cost a calling model a call to discover, and a
refusal must hold even for a caller that names it anyway.

## Decision

`agentmux mcp --deny <vendor>` — `claude-code` is an accepted spelling of `claude` — sets
`RunStore::with_denied_vendors`. The store is the gate: `RunStore::refuse_to_spawn` checks the
denied set beside the recursion refusal ([0007](0007-recursion-guard.md)), and it is called from
`LockedRun::launch_turn`, the one place `start` and `follow_up` both pass through. A consultation
begun in a terminal is therefore not a way to drive a denied vendor from a server, because its
follow-up spawns the same delegate again from there.

The MCP layer only advertises the narrower choice: `narrow_delegate_choice`
(`crates/agentmux-mcp/src/lib.rs`) rewrites the `delegate` enum of every tool that requires one. A
schema whose shape has moved leaves the list wide rather than hiding a vendor that works — the
refusal is the gate, the schema is the saved call.

`quota` keeps both vendors, because vendor denial restricts consultations, not usage probes. A probe
may start a CLI process ([0004](0004-quota-from-vendor-sources.md)) but does not ask a model.
Denying every vendor is refused at startup (`crates/agentmux-cli/src/main.rs`), where a host
surfaces it, rather than at every call, where a host swallows it.

## Consequences

- The flag is a server flag only; `agentmux ask` in a terminal still reaches either vendor.
- A server that is how an operator reaches a second account of their own vendor must leave the flag
  off, because it refuses the vendor as a whole, not one account.
