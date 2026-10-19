# 0004. Quota is read from what each vendor already records, at no cost

Status: Accepted

## Context

`agentmux quota` reports what each configured account has left, and a rate-limited failure reports
when its window reopens. Asking a model would spend the quota being measured, so every figure has to
come from somewhere free. The two vendors keep their figures in different places, and Codex reports
none on the stream `codex exec --json` writes.

## Decision

**Claude** keeps its own figures in `$CLAUDE_CONFIG_DIR/.claude.json` under
`cachedUsageUtilization`; without a configured directory, the cache is `~/.claude.json`. agentmux
reads that file (`crates/agentmux/src/quota.rs`), refreshing it with `claude --print /usage` when it
is older than five minutes, the vendor's own refresh interval. `/usage` is answered locally: the run
reports `total_cost_usd: 0` and no API duration. Only the cache's `limits[]` carries the per-model
windows; the printed text omits some of them, so it is discarded and the cache is read.

**Codex** publishes nothing to disk outside a session rollout, so agentmux asks `codex app-server`
over JSON-RPC for `account/rateLimits/read`. The server interleaves unsolicited notifications with
replies, so the response is matched on its request id rather than read as the next line.

**A Codex turn's own window** is recovered after the turn from the session rollout and cached per
turn in `rate-limit.json`, because that search would otherwise run on every `status` and every page
of a `tail`. The window recorded (`select_rate_limit_window` in `crates/agentmux/src/run/settle.rs`)
is an exhausted one if any, the one reopening last if several are, and otherwise the fullest. A plan
with a five-hour and a weekly window is refused against whichever is used up, and advice to wait for
the other would be refused again.

**A probe must not report another login's figures.** An account that authenticates with API
credentials, or that its environment routes to another backend, has no subscription window, and
probing it would answer with the default login's figures under its name. Identity is classified
before probing (`quota::ProbeTarget`), and such an account is reported as unavailable.

Probe children receive the host base allowlist and the resolved account configuration directory.
They always scrub billing-sensitive variables ([0017](0017-billing-env-scrub.md)) and do not inherit
the delegate's configured launch layers.

## Consequences

- `quota` costs nothing and spends no tokens, so a caller can ask before every long consultation.
- Claude's cache is refreshed when older than five minutes. If refreshing fails, older figures may
  still be returned with their timestamp and refresh outcome.
- The figures are passed through unranked ([0002](0002-opaque-accounts-and-quota-payloads.md)), and
  a probe that fails is unavailable, not idle ([0003](0003-failed-quota-probe-is-unavailable.md)).
