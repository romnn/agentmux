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
- **Steering.** `steer` is answered `queued` at once, and pi takes the message after the tool calls
  it is running, before its next model call, when it starts a `user` message carrying it.
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
The command is `pi --mode rpc --no-extensions --offline --tools read,grep,find,ls`, with
`--session-dir <run>/pi --session <run>/pi/session.jsonl`: one session file per consultation, so
`continue_fresh` starts a new one.
Its supervisor plan carries no automatic resume, because pi retries an overloaded provider itself.

**The driver checks what it is handed before it asks anything.**
A follow-up whose session file is missing fails before pi starts, and one whose `get_state` names
another session than the consultation's fails before anything else is sent.
The model must equal exactly one `provider/id` that `get_available_models` lists, and is chosen with
`set_model`.
An effort is set with `set_thinking_level` and read back, and a level pi changed is a warning in the
capture.
A refused question fails the turn with pi's reason on stderr.

**The capture is pi's events, and the driver adds three of its own.**
Every event but `response` is kept, streaming deltas included, so an answer the turn ended in the
middle of is still recovered.
`{"type":"session","id":…}` is written once the session file holds that id in its header, so a turn
that saved nothing advertises nothing to resume; `agentmux.steer` records a steer pi took, and
`agentmux.warning` what the turn ran with that was not asked for.

**Steers are sent one at a time, and the driver is the only source of pi's queue.**
Steering opens at `agent_start`, and the next steer is sent once the one before it was answered.
With extensions off, every `user` message pi starts after the question is the oldest steer still
waiting: the driver records it and then answers it accepted.
At `agent_settled` a steer pi queued and never took is rejected, one pi did not answer is
unconfirmed, and a request nobody claimed is rejected.
A cancelled turn is aborted with `abort` once its run has started, and its end is left to the
store's record, as for Codex.

**pi always runs with the user's configuration, and only reads.**
`ignore_user_config: true` is refused with `DelegateError::PiIsolated`, under
[0036](0036-isolation-imposed-by-the-launch-agentmux-builds.md): pi cannot pass its proofs, and its
delegate line says `settings=inherited`.
There is no `[pi]` table and no pi account; an `[accounts.pi.*]` table is refused, and
`PI_CODING_AGENT_DIR` is forwarded as pi's own configuration directory.
Under a subscription, every variable pi 1.6.0 and its bundled AWS SDK read to choose a provider, a
credential, an endpoint or an auth host is a billing name ([0017](0017-billing-env-scrub.md)),
listed in `vendor::pi::is_billing_env_name`.

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
