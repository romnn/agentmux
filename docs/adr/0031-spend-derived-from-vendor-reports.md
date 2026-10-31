# 0031. What a consultation spent is derived from the vendors' reports, which stay evidence

Status: Accepted

## Context

Each turn's terminal result carries the vendor's token figures and, for Claude, a dollar figure.
Neither vendor reports them in one scope or one set of categories. Codex reports its thread's
running total, which a resumed thread carries on from, so a ten-turn consultation's figures grow
with every turn and summing them counts the first turn ten times; when it saw no usage it writes
zeros, and it can restart its counters inside a session. Claude's tokens cover the one call, but
its `total_cost_usd` covered the one call only until Claude Code 2.1.277 and has been the session's
running total across `--resume` since. Codex counts cache reads inside its prompt tokens and Claude
beside them. A Claude error result reports what the turn spent before it failed, which a failed
outcome had no room for.

The published transcript and `settled.json` are append-only and never rewritten
([0009](0009-append-only-transcript.md)), and each turn's footer already quotes the vendor's figures.

## Decision

**A turn keeps the vendor's figures exactly as reported, and `crates/agentmux/src/accounting.rs`
is the one place that derives what each turn spent from them.**

- The evidence is raw. `Outcome::Completed { usage, cost_usd }`, the footer it renders and
  `settled.json` keep the vendor's own figures, so no published byte changes. The fold also keeps
  each turn's terminal report, succeeded or failed, with the CLI release that wrote it, as
  `Turn::spend_report` (`SpendReport` in `crates/agentmux/src/transcript.rs`), which is neither
  serialised nor rendered. The release is the one Claude's stream names, else the one
  `invocation.json` recorded.
- The vendor's semantics live in `accounting` alone: Codex tokens are session-cumulative and it
  reports no price; Claude tokens are per turn, and its price is per turn before 2.1.277 and
  session-cumulative from it. A price whose release cannot be read has an unknown scope and is not
  counted. Figures are normalised into `Tokens`, whose input counts every prompt token with the
  cached ones as a part of it, and whose output counts reasoning as a part of it.
- `Accounting::of` reads the turns in order, tokens and price as independent measures with their
  own baselines, kept count by count so a count one report leaves out is taken apart against its
last known value when it returns. A report of nothing at all is no report and moves nothing. A turn that broke
  continuity starts a new session and clears every baseline. A running total is taken apart
  against the baseline; one that fell below it is a restarted counter, counted as the new figure
  and marked incomplete. A running total covers every turn since that measure's previous report, so
  spends are spans of turns. A running price after per-turn prices in the same session is not
  counted, only adopted as the baseline. Turns no report covers are listed as unreported.
- Tokens and price are each marked incomplete on their own, so a price that could not be counted
  never qualifies the tokens. `status` reports the run total across all turns under `usage` and
  `cost_usd`, and the spans, unpriced and unreported turns and both marks under `spend`; `list`
  carries the totals and both marks (`Totals`), computed by the same derivation over a full fold
  of each listed consultation, and both say "at least" or "price unknown" the same way.
- A settled turn's capture is read only up to the length its record froze, so a fold of every turn
  never reads what was appended after a freeze.

## Consequences

- No second accounting computation exists: `Transcript` sums nothing, and both front ends print
  the same lines from `crates/agentmux/src/report.rs`.
- A footer and the run total can disagree for a Codex follow-up, deliberately: the footer quotes
  what the vendor said, and `status` says what it means.
- `list` folds every turn of each consultation it shows rather than the newest alone, which a warm
  fold of a long run does in well under a second; a run that cannot be folded is skipped, not
  fatal.
- A vendor changing how it reports spend changes `accounting` and this record, and nothing else.
