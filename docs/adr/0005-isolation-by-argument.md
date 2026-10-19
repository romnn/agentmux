# 0005. Isolation is imposed by argument, never by environment

Status: Accepted

## Context

A delegate that loads the machine's settings, hooks and MCP servers can be reopened by a blocking
`Stop` hook that blanks its result, can re-enter agentmux through a configured MCP server and
recurse, and gives an answer that depends on the machine it ran on. Both CLIs read many environment
variables, so a control that an environment variable could switch would be switched by whatever
environment reached the child.

## Decision

Isolation is the default (`Isolation::Isolated` in `crates/agentmux/src/delegate.rs`) and is imposed
by command-line arguments only:

- Claude (`claude_args`): `--setting-sources ""`, which loads no user, project or local settings,
  and `--strict-mcp-config` with an empty `--mcp-config`.
- Codex (`codex_args`): `--ignore-user-config`, the control that actually detaches configured MCP
  servers, plus `-c features.hooks=false` as insurance against a hook arriving by another route.

These flags are what suppress settings, hooks and MCP servers; no environment variable can switch
them on or off. `Isolation::Inherit` drops exactly those flags and nothing else. Plan mode
(`--permission-mode plan`), the read-only tool list and the Codex sandbox are a separate axis that
stays either way: what the delegate may do is a different question from whose configuration it
loads.

## Consequences

- The only ways to change isolation are explicit: an account's `inherit_settings`, the caller's
  `inherit_settings` (`--inherit-settings`, or `--isolated` to override an account that inherits).
- `-c mcp_servers={}` looks like it should detach Codex's MCP servers and does not; only
  `--ignore-user-config` does, which is why it is the Codex control.
- `--setting-sources ""` does not suppress `CLAUDE.md` discovery, so a project's own instruction
  file still reaches a Claude delegate.
- Because `codex app-server` has no `--ignore-user-config`, only an inheriting Codex turn can run
  over it ([0018](0018-codex-app-server-driver.md)).
- Account preferences remain subject to the project-selection restrictions in
  [0011](0011-machine-files-define-project-files-select.md).
