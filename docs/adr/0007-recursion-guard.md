# 0007. agentmux refuses to run inside its own delegate

Status: Accepted

## Context

An isolated delegate does not inherit the configured agentmux MCP server
([0005](0005-isolation-by-argument.md)), but an inheriting one loads the operator's own MCP servers,
and agentmux may be among them. A delegate that starts another delegate recurses, and every level is
paid for.

An environment marker alone does not see every nesting: Codex starts its MCP servers with an
allowlist environment of its own, in a fresh process group, and that environment drops custom
variables such as the marker.

## Decision

Every delegate carries `AGENTMUX_DELEGATE=1` (`delegate::DELEGATE_MARKER`; an exported but empty
value counts as unset). agentmux refuses to launch a delegate — or to serve MCP at all — when it
finds itself inside one: `RunStore::running_inside_a_delegate` (`crates/agentmux/src/run/spawn.rs`)
answers, `RunStore::refuse_to_spawn` turns a yes into `RunError::Recursive`, and
`crates/agentmux-cli/src/main.rs` refuses to serve.

The marker is not the only witness. `running_inside_a_delegate` also walks the process tree
(`launch::ancestors`, through `sysinfo`): the bounded ancestor walk rejects a live ancestor whose
recorded start time matches and whose turn has no exit record.

The store names its own resolved root to every delegate as `AGENTMUX_STATE_DIR`
(`RunStore::STATE_DIR_ENV`), so a nested agentmux looks in the same store whatever the platform
default would have been.

A successful check is cached for the lifetime of that `RunStore` instance. Errors listing the runs
directory or reading discovered records abort the check rather than answer "not inside", because a
guard that read an unreadable record as "no delegate here" would be blind in exactly the case it
exists for.

## Consequences

- Codex's MCP-server allowlist drops `AGENTMUX_STATE_DIR` too. When a Codex MCP-server process loses
  both variables, it has no marker and may look in the wrong store, so recursion can go undetected
  with a non-default state directory unless that server is configured to open the same store. That
  is a cost hazard, not an identity one; it is documented, not solved.
- Directory discovery currently suppresses some filesystem errors (a turn or run directory that
  cannot be examined is skipped), so the ancestor witness is not fully fail-closed. That is an
  implementation gap, not the intended decision.
