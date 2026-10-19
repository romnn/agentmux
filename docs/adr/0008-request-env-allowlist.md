# 0008. Per-request environment overrides are checked against an operator-written allowlist

Status: Accepted

## Context

A call may set environment variables for its delegate (`--env KEY=VALUE`, the MCP tools' `env`), so
tooling an inheriting delegate loads can read configuration of its own. A request arrives from a
delegating agent acting on text it was given. No list of names dangerous enough to refuse stays
complete: `PATH` chooses which binary runs, `NODE_OPTIONS` and `LD_PRELOAD` run code inside the CLI,
and the next runtime will read one more.

## Decision

Per-request environment overrides are checked against an operator-written allowlist, never against a
denylist agentmux keeps. Only the names listed under `request_env` in `agentmux.toml` may be set per
call, and by default none (`apply_request_env` in `crates/agentmux/src/delegate.rs`). The
machine-wide `request_env` always applies. The account's list adds names only when its selection
provenance permits it: not when a project file chose the account
([0011](0011-machine-files-define-project-files-select.md)).

Every environment name agentmux handles passes through `config::canonical_env_name` on the way in
(upper-cased on Windows, exact elsewhere), so every comparison is a plain one.

## Consequences

- An operator who wants a call to set a name lists it in a file no request can reach.
- Under the subscription policy, billing-sensitive names are refused even when allowlisted. Under an
  effective API opt-in, they still require allowlisting ([0017](0017-billing-env-scrub.md)).
- On Windows `path` and `PATH` are one name everywhere agentmux looks.
