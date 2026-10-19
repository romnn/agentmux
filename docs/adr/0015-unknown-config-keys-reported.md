# 0015. An unknown configuration key is reported, never refused

Status: Accepted

## Context

One machine file is read by every agentmux on the machine, and the ones that matter are the
long-running MCP servers started before a key was added. Refusing the file would take them out of
service over a table they have no use for.

## Decision

No struct in `config` sets `deny_unknown_fields`. `Config::unknown_keys` finds the keys a build does
not read by serialising the parsed config back and diffing it against the file, so the answer cannot
drift from the types: a field added to a struct is known the moment it exists.

It runs before `canonicalise_env_names`, which on Windows rewrites the keys it would be compared
against. A key whose value is an empty table or array is passed over, because serialising drops an
empty `request_env` and the file would be told its own key is unknown.

## Consequences

- A misspelling such as `configdir` for `config_dir` is ignored and reported. If the remaining
  account configuration is otherwise valid, the missing setting can change authentication,
  including falling back to the CLI's own login; if nothing understood remains, empty-account
  validation rejects the file. That is why every reader says the ignored keys out loud: a
  `tracing::warn!` at each load, an `ignored` line in `agentmux accounts`, and an `unknown` array in
  its `--json`.
- A key newer than the running agentmux and a misspelling look the same; the operator reads the
  `ignored` lines to tell them apart.
