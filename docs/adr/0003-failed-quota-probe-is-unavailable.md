# 0003. A failed quota probe is never zero usage

Status: Accepted

## Context

An unauthenticated Claude configuration directory answers `/usage` cheerfully with zeroes: it
reports 0% used. A probe that failed, or that returned something unusable, could therefore look like
an idle account, and anything ranking on "least used" would prefer whichever account is broken.

## Decision

`quota::Observation` has an `Unavailable` variant beside `Reported`. Detected probe failures are
represented explicitly as `Observation::Unavailable`; agentmux never manufactures zero usage from a
failure. A failed refresh may still return cached figures, as `Reported`, with its refresh failure
recorded beside them.

## Consequences

- Every reader of quota (the `quota` tool and command, and the note a rate-limited failure carries
  about the machine's other accounts) distinguishes "unavailable" from "idle".
- Code that would compute or rank on usage must handle `Unavailable` explicitly rather than treat a
  missing figure as zero.
- `Reported` means a vendor payload was obtained, not that agentmux independently authenticated the
  account or validated every payload field. Cached zeroes alone do not establish that an account is
  idle.
