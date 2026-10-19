# 0012. A model rewrite is the operator's file speaking, not a roster

Status: Accepted

## Context

A caller names the exact model it wants, and that name is often one a person wrote into a prompt
weeks ago — "fable-5", the series, rather than whichever point release is current. agentmux keeps no
roster ([0001](0001-opaque-models-and-efforts.md)), so it cannot know what a series name should mean
today.

## Decision

`[models.<vendor>]` in a machine file maps an identifier a caller asks for to the one launched,
which is how "fable-5" keeps meaning the current point release without an agentmux release or a code
change. An identifier the file does not name is untouched.

Exactly one substitution is applied, and a file whose rules would chain is refused at load
(`ConfigError::ChainedRewrite`), because either resolution would be a guess made on every launch.

Launch-time rewriting is applied through `Config::rewritten_model` while `RunStore::pin_defaults`
prepares the consultation at `start`: the model is frozen together with the account and the
isolation, so a file edited mid-consultation cannot answer the second half of one transcript with
another model.

What was asked for is kept in `Meta::rewritten_from` and rendered by `run::describe_delegate` (for
example `codex gpt-6-astra … (asked for gpt-6)`), since a caller that pinned a model and read back
another must be able to tell its own machine's rule from a pin agentmux dropped.

Only a machine file may define a rewrite
([0011](0011-machine-files-define-project-files-select.md)): which model answers decides what an
operator pays and what they are told.

## Consequences

- A model released today works today; a rewrite for it is one line the operator adds when wanted.
- `agentmux accounts` lists the rules alongside the accounts.
- A default effort is keyed by the rewritten model, not the asked one
  ([0013](0013-default-efforts.md)).
