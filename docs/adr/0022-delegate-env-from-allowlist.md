# 0022. A delegate's environment is built from empty by allowlist

Status: Accepted

## Context

agentmux is usually launched by a coding-agent session, so the environment it inherits is full of
that session's identity: its configuration directory, session ids, hook and IDE wiring, keys the
user exported for other work. A delegate that inherited it would run as a nested session wearing
its parent's identity, and could spend a key nobody meant it to use. No denylist of such names stays
complete, because every release of either CLI and every tool around them reads one more.

## Decision

**Every child starts from an empty environment and receives only names agentmux or the operator
listed.** `delegate::base_env` (`crates/agentmux/src/delegate.rs`) copies `BASE_ALLOWLIST` (path,
home, user, shell, locale, time zone, temporary directory, proxy and CA settings) and, on Windows,
`PLATFORM_ALLOWLIST` from the host, then marks the child non-interactive (`TERM=dumb`, `NO_COLOR`,
`CI`). Every spawn clears the inherited environment before applying the built one: the direct CLI
launch, the app-server driver and the quota probes.

The layers above the base each add names someone chose:

- The machine's `[launch]` layer sets literal values (`env`) and forwards named host variables
  (`env_passthrough`).
- A vendor's credential variables (`CLAUDE_CREDENTIAL_ALLOWLIST`, `CODEX_CREDENTIAL_ALLOWLIST`) are
  forwarded only when no account is named, and only those that are not billing names; a named
  account withholds them so it authenticates as itself.
- The account's own layer applies its configuration directory, `env` and `env_passthrough`.
- A request's `env` may set only names the operator allowlisted
  ([0008](0008-request-env-allowlist.md)).

The billing scrub runs once the identity layers are applied and again at the spawn
([0017](0017-billing-env-scrub.md)), and the recursion marker is set last, so no layer can change it
([0007](0007-recursion-guard.md)).

`CLAUDE_CONFIG_DIR` is not forwarded even when no account is named: a Claude Code host sets it to
name its own login, so forwarding it would make the delegate a second session on that login.
`CODEX_HOME` is forwarded, because a Codex session does not set it for itself and it is the only
way a relocated Codex login is found.

## Consequences

- A variable a delegate's tooling needs reaches it only once the operator names it in a machine
  file: `env_passthrough` on `[launch]` or the account, or `request_env` for a per-call value.
- A new variable either CLI starts reading is absent until someone adds it, which fails visibly
  instead of leaking silently.
- A delegate and a quota probe for the same account are given the same network settings, because
  both start from `base_env`.
