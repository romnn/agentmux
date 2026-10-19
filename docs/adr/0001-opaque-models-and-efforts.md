# 0001. Model ids and reasoning efforts are opaque pass-through strings

Status: Accepted

## Context

Both vendors ship new model identifiers and reasoning-effort levels without notice, and the
vocabularies differ between vendors and between models. A roster kept in agentmux would be stale the
day a model is released, and every new identifier would need an agentmux release.

## Decision

Model and effort values have no built-in roster. `ModelId` and `Effort`
(`crates/agentmux/src/delegate.rs`) wrap the string the caller gave, and an accepted value is
forwarded to the delegate CLI unchanged, except for an explicit machine-file model rewrite
([0012](0012-model-rewrites.md)). The CLI is the sole authority on which values it accepts; when one
is wrong, that CLI's own error — which names the valid set — is surfaced verbatim.

agentmux checks only a bounded token syntax suitable for argv and configuration overrides
(`ModelId::parse` and `Effort::parse`): non-empty, bounded in length, not starting with `-`, and
drawn from a small character set. The parsers' documentation gives the exact sets.

## Consequences

- A new identifier within the accepted token syntax requires no roster update or agentmux release.
- A misspelled identifier reaches the CLI and comes back as the CLI's own refusal.
- An operator who wants one name to mean another writes a machine-file rule
  ([0012](0012-model-rewrites.md)) rather than asking for a code change.
