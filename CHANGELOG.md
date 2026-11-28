# Changelog

## Unreleased

- **pi is a fourth delegate, and its running turns can be steered.** `--delegate pi`
  (`delegate: "pi"`) runs `pi --mode rpc` with your pi configuration, read-only, on the model
  named as `provider/id`.
- **agy, Google's Antigravity CLI, is a third delegate.** `--delegate agy` (`delegate: "agy"`)
  reaches Gemini models on a Google AI subscription, read-only; it always runs the project's own
  `.agents` tooling, and `ignore_user_config` keeps out only the user's.
- **A running Claude turn can be steered.** Its supervisor holds Claude's input, so `steer` reaches
  it until Claude reports its result.
- **A Claude turn ends when Claude exits, and keeps what Claude answers after its first result,**
  to a late steer or a background task that finished.
- **Breaking: agentmux v0.0.13 and older cannot read a steerable Claude turn.** Restart every MCP
  server started before upgrading.

## v0.0.13

- **The codex app-server protocol is pinned to codex-cli 0.171.0,** up from 0.162.1.

## v0.0.12

- **A delegate's mid-turn questions are surfaced.** A codex question shows as
  `### assistant (question)`, `status` and `list` report them, and `wait` with
  `or_delegate_question` (`agentmux wait --or-delegate-question`, exit 12) returns on one.

## v0.0.11

- **A consultation's spend is counted per turn.** `status` summed codex's cumulative reports into
  many times the real spend; `status` and `list` now show per-turn and total spend, and `list`
  gains `--since`.
- **A turn reports the processes it left running.** The first question tells the delegate that
  background jobs may be stopped with the turn, and `status` warns when a codex app-server turn
  left processes behind.
- **A launch is refused once the agentmux executable was replaced.** An MCP server that outlived
  an upgrade failed its consultations long after `start`; restart it after upgrading.
- **Breaking: the store no longer renumbers zero-based turn directories.** Open runs left by
  agentmux v0.0.9 or older with v0.0.10 first.

## v0.0.10

- **An overloaded model no longer stops a consultation.** Its turn resumes with "continue" after
  30 seconds, backing off to 15 minutes, at most 12 times in a row; `cancel` stops a pending
  resume.
- **Label consultations and filter them.** `start` and `follow_up` take a `label`, and `list`
  filters by `label` and `state`.
- **Continue in a fresh session when a consultation's own cannot go on.** `continue_fresh`
  (`agentmux continue-fresh`) hands the transcript and working tree to a new consultation.
- **`tail` can start from the end of the transcript.** `tail { from_end: true }` or
  `agentmux tail --from-end`.
- **Breaking: turns are numbered from 1 everywhere, directories included.** `turns/0001` is
  turn 1, and existing runs are renumbered when the store opens.
- **Breaking: earlier failed turns are reported beside the state, not inside it.** `status --json`
  replaces `earlier_failure` with `earlier_failures`.
- **Breaking: a usage window "reopens" only when it is exhausted.** `status --json` replaces the
  rate limit with `usage_window`, which carries `exhausted` and `used_percent`.

## v0.0.9

- **Steer a running codex turn.** A codex consultation that inherits its account's settings runs
  over `codex app-server`, so `steer` reaches it mid-turn; `codex_exec` keeps `codex exec`.
- **Delegates stay on their subscription.** API keys, endpoints and auth tokens are removed from a
  delegate's environment unless its account sets `billing = "api"`.
- **Choose what a delegate loads and may do.** Delegates load your CLI configuration, codex runs
  read-only and claude in plan mode by default, and `[codex|claude.defaults]` and `.limits` in
  the machine file, request parameters and flags change that.
- **`wait` returns when a turn's delegate exits.** `agentmux wait` and the MCP `wait` tool block
  until the turn settles instead of polling.
- **The machine file can block a model and name the one to use instead.** `[blocked.<vendor>]`.
- **Consultations are kept for a week by default.** The machine file's `retention` sets another
  window.
- **A group-writable config file is accepted when its owner is the group's only member,** as on
  distributions with user-private groups.
- **Installing with Homebrew no longer warns about the macOS quarantine step.**

## v0.0.8

- **A follow-up warns up front when the delegate cannot save its session.** Every launch checks the
  vendor's session store, which a sandboxed shell may deny.
- **A failed turn's status quotes its stderr** when no exit record explains it.
- **More of claude's task events read as known.** Task updates and background task events no
  longer report stream drift.

## v0.0.7

- **A claude resume against a conversation the CLI no longer holds reads as that failure, not as
  drift.**
- **Claude's task and tool progress events no longer report stream drift.**

## v0.0.6

- **Breaking: a model's reasoning effort falls back to the machine file's `[efforts.<vendor>]`.**
  `effort` is now optional; a model no file names runs at the CLI's own default.

## v0.0.5

- **Rewrite model identifiers per machine.** `[models.<vendor>]` in the machine file maps the
  identifier asked for to the one launched.
- **Unknown config keys are warnings, not errors.** `agentmux accounts` lists them.

## v0.0.4

- **The transcript records the model the vendor reports running,** and `result` says "answered by"
  when it differs from the identifier asked for.
- **`follow_up` pages from the new turn** instead of re-sending the whole consultation.
- **Quota reads the login a codex account actually uses** and says whether a claude cache refresh
  succeeded.
- **`result` ends with the next call to make.** Cost is labelled as list price.

## v0.0.3

- No user-facing changes.

## v0.0.2

- **First release: delegate a question to the other vendor's coding agent and keep the whole
  transcript,** over `agentmux mcp` or the CLI.
- **A rate limit carries the time its usage window reopens.**
