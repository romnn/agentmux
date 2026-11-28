# 37. A pi turn is driven over `pi --mode rpc`, read-only, with the user's own configuration

**Status:** accepted · **Date:** 2026-11-28

## Context

pi, the pi coding agent, is a fourth delegate (issue #49), probed at 1.6.0.
`pi --mode rpc` speaks JSON lines on stdio: commands with an id, a `response` per command, and the
session's events without one.
What the probe and the live runs established:

- **Completion.** `agent_settled` is the one terminal event: `agent_end` comes once per attempt, and
  pi goes on after it to retry, compact or take a queued message.
  It says only whether the run was aborted; how it ended is the last assistant message's
  `stopReason`.
  pi exits 0 even when the provider failed, so only that message says so.
- **Input.** Closing pi's stdin aborts a running turn, and SIGTERM ends pi without a terminal event.
- **Steering.** `steer` is answered `queued` as soon as pi has queued the message, which says
  nothing about when pi takes it: after the tool calls it is running, before its next model call,
  when it starts a `user` message carrying it.
  No id ties the two together.
  A message still queued when the run settles is held for a later prompt.
- **Sessions.** `--session <path>` opens that file, or silently starts a new session there when it
  is missing; `get_state` names the session's id, which the file's header repeats once pi has
  saved it.
  A session saved by one process is resumed by the next: a follow-up through a new driver answered
  from the first turn's history.
- **Models.** `--model` matches fuzzily and accepts an unknown id under a known provider with only a
  warning, while `set_model` matches exactly and refuses an unknown one.
- **Configuration.** pi always reads its agent directory's `settings.json`, `models.json`,
  `auth.json`, `SYSTEM.md` and `APPEND_SYSTEM.md`, `~/.agents/skills`, and the `AGENTS.md` and
  `CLAUDE.md` of the working directory and every directory above it.
  No flag skips `settings.json` or the system prompt files.
  The only clean cut is a private agent directory and a private home, which would have to carry a
  copy of, or a link to, the user's login: what [0018](0018-codex-app-server-driver.md) rejects for
  Codex.
- **Extensions.** `--no-extensions` keeps out every extension, pi's built-in MCP, `llama.cpp`,
  `codemode` and `tool-search` included, so nothing can register a tool, open a dialog that blocks
  the run, or queue a message of its own.
  `--offline` stops the package installs and the model catalog refresh pi otherwise starts with.
- **Tools.** `--tools read,grep,find,ls` offers only tools that read; pi asks no permission before
  a tool call.
- **Credentials.** Nothing needs a key in the argument vector: pi reads its login from `auth.json`,
  `models.json` or provider variables, and an inherited `HF_TOKEN` or `OPENAI_API_KEY` adds whole
  providers.
- **Shutdown.** pi leaves within about a tenth of a second of an `abort` or of its stdin closing;
  a cancelled live turn was stopped, its driver and pi gone, in about half a second, far inside
  `STOP_BUDGET`.

The live test passed with its first turn on pi 1.6.0 and its follow-ups on 1.6.1.

## Decision

**A pi turn is always driven by agentmux, as `agentmux pi-rpc <plan>`, in pi's place.**
`vendor::pi::driver` shares the process with the Codex driver through `crate::driver`: the launch
record check, the spawn in the driver's process group with the billing scrub, the descendant
monitor, the stop and sweep, and `STOP_BUDGET`.
A store without a driver refuses a pi launch with `DelegateError::PiUndriven`.
The command is `pi --mode rpc --no-extensions --offline --tools read,grep,find,ls`, to which the
driver adds `--session-dir <run>/pi --session <run>/pi/session.jsonl` from the one session file its
plan names: one per consultation, so `continue_fresh` starts a new one.
Its supervisor plan carries no automatic resume, because pi retries an overloaded provider itself.

**The driver checks what it is handed before it sends the question.**
A follow-up whose `get_state` names another session than the consultation's fails before anything
else is sent, which covers a session file gone missing too: pi opens a new session there.
The model must equal exactly one `provider/id` that `get_available_models` lists, and is chosen with
`set_model`.
An effort is set with `set_thinking_level` and read back, and a level pi changed is a warning in the
capture.
Once the question is sent, the driver follows the turn alone: an answer refusing it, one that
starts no run, or none within the setup deadline, fails the turn with pi's reason on stderr.

**The capture is pi's events, and the driver adds three of its own.**
Every event but `response` is kept, streaming deltas included, so an answer the turn ended in the
middle of is still recovered.
`{"type":"session","id":…}` is written once the turn is over, before its `agent_settled`, when the
session file's header holds that id: pi appends to the file after it reports a message, so only
then does the header say whether the turn saved anything to resume.
A cancelled turn names it too, though a cancelled consultation is never resumed: the store checks
every follow-up that ended for the session it continued.
`agentmux.steer` records a steer pi took, and `agentmux.warning` what the turn ran with that was
not asked for.

**Steers are sent one at a time, and the driver is the only source of pi's queue.**
Steering opens at `agent_start`, and the next steer is sent once the one before it was answered.
With extensions off, every `user` message pi starts after the question is the oldest steer still
waiting: the driver records it and then answers it accepted.
At `agent_settled` every steer pi did not take is rejected, the one still unanswered included.
A turn that ends without `agent_settled`, because pi went away or a cancellation's grace ran out,
leaves every steer pi was sent and not seen to take unconfirmed, since pi may have taken it.
A request nobody claimed is rejected either way.
A cancellation is an `abort`, sent only once pi has answered the question `started`: pi takes its
commands without waiting for the one before, and an `abort` it takes before the run is active finds
it idle and leaves the run to start anyway.
Before that answer the driver waits for it within the interrupt grace, and a cancellation during a
compaction pi runs before the question can therefore take the whole grace.
A run the abort ended ends as the store records the cancellation, and one that settled on its own
first ends as pi says, as for Codex.

**pi always runs with the user's configuration, and only reads.**
`ignore_user_config: true` is refused with `DelegateError::PiIsolated`, under
[0036](0036-isolation-imposed-by-the-launch-agentmux-builds.md): pi cannot pass its proofs, and its
delegate line says `settings=inherited`.
There is no `[pi]` table and no pi account; an `[accounts.pi.*]` table is refused, and
`PI_CODING_AGENT_DIR` is forwarded as pi's own configuration directory.
Under a subscription, every variable pi 1.6.0 and its bundled AWS SDK read to choose a provider, a
credential, an endpoint or an auth host is a billing name ([0017](0017-billing-env-scrub.md)),
listed in `vendor::pi::is_billing_env_name`.
`GOOGLE_CLOUD_PROJECT` is not among them, though pi's Vertex provider reads it: agy reads it beside
its login, so withholding it could break agy, and withholding `GOOGLE_CLOUD_LOCATION` already turns
pi's environment-driven Vertex path off.

**Spend is every usage pi reports, once; quota is unavailable.**
A turn counts each assistant message's usage, a compaction's and a cache refresh's, with cache reads
and writes beside the input; pi's price is a catalog estimate and is not kept.
pi reports no usage window, so its quota is unavailable
([0003](0003-failed-quota-probe-is-unavailable.md)).

## Consequences

- A pi consultation loads the user's skills, prompt templates and context files, the `AGENTS.md`
  and `CLAUDE.md` above its working directory included; what it answers can depend on the machine.
- A steer is accepted only when pi takes it, between model calls, which on a slow local model can be
  later than `steer` waits; `steer` then reports it unconfirmed, and it still arrives.
- The billing names are shared by every vendor, so `AWS_PROFILE`, `HF_TOKEN` and the other names pi
  reads are withheld from every subscription delegate.
- A pi release that changes its events, its RPC commands or what `--no-extensions` keeps out is
  checked by the live test in `crates/agentmux-cli/tests/live.rs` and the recorded capture under
  `crates/agentmux/fixtures/pi/`.
