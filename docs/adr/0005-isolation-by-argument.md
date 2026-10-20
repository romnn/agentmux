# 0005. Isolation is imposed by argument, never by environment

Status: Accepted

## Context

A delegate that loads the machine's settings, hooks and MCP servers can be reopened by a blocking
`Stop` hook that blanks its result, can re-enter agentmux through a configured MCP server and
recurse, and gives an answer that depends on the machine it ran on. Both CLIs read many environment
variables, so a control that an environment variable could switch would be switched by whatever
environment reached the child.

## Decision

Isolation (`Isolation::Isolated` in `crates/agentmux/src/delegate.rs`, spelled
`ignore_user_config = true`) is imposed by command-line arguments only:

- Claude (`claude_args`): `--setting-sources ""`, which loads no user, project or local settings,
  and `--strict-mcp-config` with an empty `--mcp-config`.
- Codex (`codex_args`): `--ignore-user-config`, the control that actually detaches configured MCP
  servers, plus `-c features.hooks=false` as insurance against a hook arriving by another route.

These flags are what suppress settings, hooks and MCP servers; no environment variable can switch
them on or off. `Isolation::Inherit` drops exactly those flags and nothing else. The sandbox, the
permission mode and the tool lists are a separate axis passed on every launch either way
([0026](0026-delegate-permissions.md)): what the delegate may do is a different question from whose
configuration it loads.

Isolation is not the default. A consultation loads the user's configuration unless the caller, the
account's `inherit_settings`, or the vendor's `ignore_user_config` default in the machine file says
otherwise, and `allow_user_config = false` among a vendor's limits rules loading it out
([0026](0026-delegate-permissions.md)).

## Consequences

- The ways to change isolation are explicit: the caller's `ignore_user_config` (or its inverse
  `inherit_settings`, `--inherit-settings`, `--isolated`), an account's `inherit_settings`, and the
  vendor's default in the machine file.
- The three guarantees isolation buys — no hook reopens a finished turn, no configured MCP server
  is reachable, the answer does not depend on the machine's configuration file — hold for isolated consultations only.
  A consultation that loads the configuration says so in its delegate line (`settings=inherited`),
  and a reopened turn is marked as expected rather than suspect.
- `--ignore-user-config` does not stop Codex loading the exec-policy rules in `$CODEX_HOME/rules`,
  whose allow-rules run matching commands outside the sandbox
  ([0026](0026-delegate-permissions.md)).
- `-c mcp_servers={}` looks like it should detach Codex's MCP servers and does not; only
  `--ignore-user-config` does, which is why it is the Codex control.
- `--setting-sources ""` does not suppress `CLAUDE.md` discovery, so a project's own instruction
  file still reaches a Claude delegate.
- Because `codex app-server` has no `--ignore-user-config`, only a Codex turn that loads the
  configuration can run over it ([0018](0018-codex-app-server-driver.md)).
- Account preferences remain subject to the project-selection restrictions in
  [0011](0011-machine-files-define-project-files-select.md): an account a checkout selects runs
  isolated unless the caller explicitly asks to load the configuration.
