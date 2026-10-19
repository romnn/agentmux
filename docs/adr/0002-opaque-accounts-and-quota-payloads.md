# 0002. Account aliases and quota payloads are opaque

Status: Accepted

## Context

A delegating agent knows it wants "the personal account"; which accounts exist, and where their
credentials live, differs on every machine. Usage figures differ by vendor and by plan: a percentage
means nothing without the plan tier behind it, and every absolute figure is null on a subscription.

## Decision

Account aliases are opaque, like model identifiers ([0001](0001-opaque-models-and-efforts.md)).
`AccountAlias` (`crates/agentmux/src/delegate.rs`) has a bounded identifier syntax, but an alias's
meaning and existence come exclusively from machine configuration: the machine's `agentmux.toml` is
the only authority on which accounts exist. agentmux keeps no roster of accounts and never ranks
them.

A vendor's usage figures are passed through exactly as they arrived: `quota::Observation::Reported`
carries the vendor's own JSON as an untyped `payload`.

## Consequences

- A new window, plan tier or limit kind reaches the caller without an agentmux release, exactly as a
  new model identifier does.
- Choosing an account from the figures is the caller's job; agentmux reports and does not recommend.
- An account that could not be asked needs its own representation, because it must not read as an
  idle one ([0003](0003-failed-quota-probe-is-unavailable.md)).
