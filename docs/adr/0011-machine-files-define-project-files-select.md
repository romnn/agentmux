# 0011. Only a machine file defines; a project file selects an account and switches nothing on

Status: Accepted

## Context

agentmux reads two kinds of `agentmux.toml`. A machine file belongs to the operator. A project file
arrives with a `git clone`, written by whoever wrote the repository. Were a project file able to
name a `base_url` and an `api_key_env`, cloning a repository and asking for one second opinion would
send the operator's key to its author: a credential-exfiltration primitive. The same holds, less
directly, for a file that could choose which model answers, switch on API pricing, turn off
isolation, or open the request environment.

## Decision

**Only a machine file may define anything.** Accounts, `[launch]`, model rewrites
([0012](0012-model-rewrites.md)), default efforts ([0013](0013-default-efforts.md)) and the
`retention` window come from a machine file alone. The window governs one store shared by every
project, so a checkout that set it would decide when other projects' consultations are deleted. A
project `agentmux.toml` is found by walking up from the delegate's working directory to the root of
its repository (the nearest `.git`); outside a repository, discovery runs only within the home
directory and excludes home itself. A project file may only select an account with
`[defaults.<vendor>]`: nonempty understood account definitions, launch settings, model rewrites,
effort defaults and a retention are refused (`ConfigError::ProjectFileOversteps`), and unknown keys
follow [0015](0015-unknown-config-keys-reported.md).

Machine files live under the home directory only: `XDG_CONFIG_HOME` and `APPDATA` count only when
they point there, and `AGENTMUX_CONFIG` (which must be absolute) is the one deliberate escape hatch.
A container that points the config base at a workspace therefore cannot promote a checkout to a
machine file. On Unix, both roles require a regular file owned by the current user and not writable
by group or others (`refuse_if_untrusted` in `crates/agentmux/src/config.rs`); other platforms do
not currently enforce equivalent ownership or permission checks.

**A project file's selection switches nothing on.** An account a project file chose runs isolated,
never on API pricing, and without its own `request_env`, whatever the machine file configures for
it. Only an explicit `inherit_settings` from the caller overrides the isolation.
`delegate::AccountPolicy` is the one place that is decided, for every reader — isolation, billing,
request environment.

A project default that changes the machine default marks that alias project-selected, even when a
caller names it explicitly, because a follow-up names the default it resolved and the checkout's own
instructions could name it too. A project default that restates the machine default retains machine
provenance.

## Consequences

- A project file is safe to commit: it says which of the operator's accounts pays, never that a key
  reaches it.
- An account that opts into API pricing in the machine file still runs under the subscription rule
  when a project file selects it, and one that names a key is refused
  ([0017](0017-billing-env-scrub.md)).
- At `start`, the account choice is pinned as a configured alias or the CLI's own login. Later
  defaults cannot move the consultation, but account definitions and the launch environment are
  reloaded for each turn: the alias is frozen, its credentials are not snapshotted.
