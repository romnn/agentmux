# 36. Isolation is imposed by the launch agentmux builds: by argument, or by environment it sets after every other layer

**Status:** accepted · **Date:** 2026-11-28

## Context

[0005](0005-isolation-by-argument.md) held that isolation is imposed by argument, never by
environment, because a control an environment variable could switch would be switched by whatever
environment reached the child.
agy has no argument that skips its user settings, and is isolated by a `HOME` agentmux sets
([0035](0035-agy-isolation-through-a-home-of-its-own.md)), which 0005's rule does not allow.
More vendors are coming (issue #42), each with means of its own, and each vendor's module now
prepares its whole launch (`vendor::<vendor>::prepare`).

## Decision

**Isolation is imposed by the launch agentmux builds: by an argument, or by an environment
variable agentmux sets after every other layer, the host's, the account's and the request's.**
It is never imposed by a variable a caller, an account or the host could set, and never by a copy
of the user's login, whose hazards [0018](0018-codex-app-server-driver.md) records.

Each vendor's own record names its means and what they leave out: Claude and Codex isolate by
argument, as 0005 describes, and agy by its home, as 0035 describes.
The vendor's launch preparation is the one place its means are applied.

A new vendor is offered an isolated launch only once it has proved, against its pinned release:

- where it discovers configuration, hooks and MCP servers, and that its means keep out the user's;
- where its credentials live, and that isolation neither moves them nor copies them;
- where it saves its sessions, so a follow-up resumes the one the turn saved;
- that what agentmux sets takes precedence over everything the CLI reads.

## Consequences

- 0005 is superseded; its body still records the Claude and Codex arguments.
- agy's store-owned home stays agy's: it is no facility another vendor inherits, and a vendor that
  needs one records its own.
- A vendor that cannot pass the proofs runs only with the user's configuration loaded, and says so
  in its delegate line (`settings=inherited`).
