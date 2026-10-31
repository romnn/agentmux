# Architecture decision records

Each record states one design decision agentmux holds today and why. Read the record before
changing the area it governs. When a change alters a decision, update its record in the same change;
when it makes a new one, add a record with the next number.

ADRs own design decisions and their rationale. Implementation details and API contracts live beside
the code; repeatable procedures live in project skills (`.agents/skills/`). Other documents
summarize and link to those sources.

Records use a light format: a numbered title, `Status: Accepted`, then `## Context`, `## Decision`
and `## Consequences`. They describe the current decision, not its history. A type, function or file
a record names to show where the decision is enforced must be renamed there too.

## Identity, configuration and billing

- [0001](0001-opaque-models-and-efforts.md): Model ids and efforts are opaque, forwarded unchanged
  but for a machine-file rewrite.
- [0002](0002-opaque-accounts-and-quota-payloads.md): Account aliases and quota payloads are opaque;
  accounts are never ranked.
- [0008](0008-request-env-allowlist.md): Per-request environment overrides are checked against an
  operator-written allowlist.
- [0011](0011-machine-files-define-project-files-select.md): Only a machine file defines; a project
  file only selects an account.
- [0012](0012-model-rewrites.md): Model rewrites are one machine-file substitution, pinned at
  `start`.
- [0013](0013-default-efforts.md): A default effort follows the running model; omission preserves
  the CLI default.
- [0014](0014-typed-config-parsing.md): Configuration is parsed into its types as it is read.
- [0015](0015-unknown-config-keys-reported.md): An unknown configuration key is reported, never
  refused.
- [0017](0017-billing-env-scrub.md): Billing-sensitive environment variables require machine-file
  opt-in.
- [0022](0022-delegate-env-from-allowlist.md): A delegate's environment is built from empty by
  allowlist.
- [0024](0024-blocked-models.md): A blocked model is refused at `start`, naming what to ask for
  instead.

## Quota observations

- [0003](0003-failed-quota-probe-is-unavailable.md): A failed quota probe is `Unavailable`, never
  zero usage.
- [0004](0004-quota-from-vendor-sources.md): Quota comes free from each vendor's own records.

## Isolation and delegation

- [0005](0005-isolation-by-argument.md): Isolation is imposed by argument, never by environment.
- [0006](0006-server-denies-own-vendor.md): An MCP server denies its own harness's vendor; the store
  enforces it.
- [0007](0007-recursion-guard.md): agentmux refuses to run inside its own delegate.
- [0016](0016-unsaved-session-store.md): Session-store write restrictions are probed before every
  turn.
- [0026](0026-delegate-permissions.md): A delegate's permissions are the CLI's own controls,
  bounded by machine-file limits.

## Run lifecycle and transcripts

- [0009](0009-append-only-transcript.md): The transcript is append-only; a settled turn's fold input
  is frozen.
- [0010](0010-pids-and-start-times.md): A pid on disk is trusted only with its start time.
- [0020](0020-run-state-locking.md): A run changes shape only under its lock.
- [0023](0023-resume-after-delegate-exit.md): A follow-up resumes a session only once its delegate
  has exited.
- [0025](0025-eager-settlement-and-terminal-waiting.md): A supervisor settles each turn as its
  delegate leaves, and waits return only then.
- [0027](0027-automatic-resume-after-overload.md): A turn that fails because the model is
  overloaded is resumed by its own supervisor.
- [0028](0028-labels-belong-to-turns.md): A label belongs to the turn it was given for, and later
  turns inherit it.
- [0029](0029-continue-in-a-fresh-session.md): A session that cannot go on is continued by a new
  consultation that reads the old one.
- [0030](0030-refuse-to-launch-from-a-replaced-executable.md): A store refuses to launch once the
  executable it launches from has been replaced.
- [0031](0031-spend-derived-from-vendor-reports.md): What a consultation spent is derived from the
  vendors' reports, which stay evidence.

## Codex integration

- [0018](0018-codex-app-server-driver.md): A Codex turn that loads the user's configuration is
  driven over `codex app-server`, so it steers.
- [0019](0019-generated-app-server-protocol.md): The app-server protocol is generated from codex's
  schema.
- [0021](0021-mock-validates-against-codex-schema.md): The mock app-server validates messages
  against codex's schema, independently of the generated types.
