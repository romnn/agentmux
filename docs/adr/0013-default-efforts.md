# 0013. A default effort follows the running model; omission preserves the CLI default

Status: Accepted

## Context

Most callers pin the reasoning effort, and should. For those that leave it out, the operator may
want a per-model default. agentmux must not invent an effort of its own: effort vocabularies change
with models, and a value agentmux chose would silently override the CLI's own default.

## Decision

The effort is optional: `Delegate::effort` returns an `Option<&Effort>`. `[efforts.<vendor>]` in a
machine file fills it in `RunStore::pin_defaults` through `Config::default_effort`, after the model
rewrite ([0012](0012-model-rewrites.md)) and only when the caller named none. When neither caller
nor machine configuration supplies an effort, agentmux passes no effort override — no `--effort` and
no `model_reasoning_effort` — so the CLI's own default runs rather than a value agentmux invented.

The resolved override is pinned at `start` with the model. Leaving it absent does not freeze a
vendor-controlled default: each turn runs whatever default the CLI has then.

A default keyed by a model the same file rewrites away is refused at load
(`ConfigError::EffortForRewrittenModel`): it could never match, and reads as if it did.

Both sides of a rule are opaque `ModelId` and `Effort` values
([0001](0001-opaque-models-and-efforts.md)), because effort vocabularies change with models.

## Consequences

- The tool schemas do not require `effort`.
- Records and summaries name the supplied effort override, or `cli-default` when none was supplied.
  They do not discover the CLI's effective default.
- `agentmux accounts` lists the defaults under `efforts`.
