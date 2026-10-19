# 0024. A blocked model is refused at `start`, naming what to ask for instead

Status: Accepted

## Context

A model can become too expensive, or be superseded by a cheaper one, while prompts and agent
instructions written weeks ago still name it. A rewrite ([0012](0012-model-rewrites.md)) could
silently send those requests elsewhere, but which model answers decides what the operator pays and
what the caller is told, and a caller that pinned an expensive model for a hard review should learn
that the machine refuses it rather than receive a cheaper model's answer under its name.

## Decision

`[blocked.<vendor>]` in a machine file maps a model to the one to ask for instead, written either as
the recommendation alone or as `{ use = "<model>", reason = "<text>" }`. Both spellings deserialize
into `config::BlockedModel`; the table form serializes back as a table so `Config::unknown_keys`
reports a misspelled key inside it ([0015](0015-unknown-config-keys-reported.md)).

**A block refuses; it never substitutes.** `RunStore::start` refuses a blocked model with
`RunError::ModelBlocked`, which names the recommendation and the reason, before a run directory is
created or a delegate launched. The caller asks again, knowingly.

**The launched id is checked**: the model after the machine file's one rewrite
(`Config::rewritten_model`), looked up through `Config::blocked_model`. An old spelling the file
rewrites to an allowed model keeps working, and a block cannot be dodged by asking under another
name the file rewrites onto it.

**Contradictions are refused at load** (`Config::validate`): a rewrite whose target is blocked
(`ConfigError::RewriteToBlockedModel`), a block keyed by a model the same file rewrites away, which
could never apply (`ConfigError::BlockForRewrittenModel`), and a recommendation that would itself
launch a blocked model (`ConfigError::BlockedRecommendation`). Since a rewrite onto a blocked model
is refused, the model a refusal names is always the one the caller asked for.

**Only a new consultation is checked.** A follow-up keeps the model pinned at `start` and is not
refused when that model has been blocked since, so an expensive review already under way can still
be followed up rather than paid for again.

**There is no roster.** The recommendation is checked only against the same file's rewrites and
blocks; whether the vendor still offers it is the vendor CLI's to say
([0001](0001-opaque-models-and-efforts.md)).

Only a machine file may block a model
([0011](0011-machine-files-define-project-files-select.md)).

## Consequences

- A prompt naming a retired model fails fast with the model to use, at no cost.
- The MCP server's instructions list the aliases and blocked models of the vendors it launches as
  a snapshot from server start. `quota` lists them from the current file on every call, saying so
  when there are none, so a rule removed since start is corrected rather than left unsaid.
  `agentmux accounts` lists them under `blocked`.
- A consultation started before a block keeps running and taking follow-ups on the blocked model.
