# 0018. A Codex turn that loads the user's configuration is driven over `codex app-server`

Status: Accepted

## Context

`steer` adds an instruction to a turn that is still running. `codex exec` reads its prompt once, and
`claude --print` offers no channel into a turn that has started; only `codex app-server` accepts
input for a running turn. A JSON-RPC server needs a client holding its stdin for as long as the turn
runs, and a consultation must survive the CLI or MCP server that started it. The run store and the
transcript interface — the fold, `status`, `tail`, `result`, a restarted agentmux — must keep
working unchanged for a driven turn.

The app-server has no `--ignore-user-config`, the flag Codex isolation rests on
([0005](0005-isolation-by-argument.md)). Running it with a private `CODEX_HOME` holding only the
login was rejected, because none of its hazards can be contained from outside codex:

- a login kept in the operating system's keyring is filed under a hash of the canonical
  `CODEX_HOME` path (`compute_store_key` in `codex-rs/login`), so another directory has no login at
  all;
- a copied `auth.json` refreshes its own tokens, and codex treats a refresh token used twice as
  exhausted (`refresh_token_reused`), so the copy would log the user's own codex out;
- sessions and codex's SQLite state would live in the private directory, so the user's
  `codex resume`, and agentmux's own rate-limit reading, would not find the consultation.

## Decision

**The driver is the delegate.** The client is a detached delegate process, so the consultation
survives the CLI or MCP server that started it: this executable run as the hidden
`agentmux codex-app-server <plan>` (`appserver::drive`), launched by the same launcher into the same
turn directory, reading the question on stdin and writing `codex exec --json`-shaped events to
stdout. Both launch modes use the same capture fold and transcript interface; steering admission and
cancellation grace still depend on the recorded launch mode (`LaunchMode`). What each notification
becomes follows codex's own exec client, written as the `codex_app_server_protocol::exec` mirror
([0019](0019-generated-app-server-protocol.md)).

`Delegate::app_server_settings` is the one place that decides a delegate can run this way — Codex
and `Isolation::Inherit` only, which is what a Codex consultation gets unless the caller or the
machine file chooses `ignore_user_config = true` ([0026](0026-delegate-permissions.md)) — and
`RunStore::with_codex_app_server` opts a store in; without it every Codex turn is `exec`.

Under a supervisor ([0025](0025-eager-settlement-and-terminal-waiting.md)) the driver is the
delegate the supervisor starts and records, and the supervisor settles the turn once the driver has
gone; nothing about the driver changes.

**One driver runs one turn and exits.** Follow-ups resume the persisted thread through a new driver;
keeping a server idle between turns would leave a process per retained consultation without being
needed for session continuity. Before starting its server, a driver verifies that the turn's launch
record names it, preventing an abandoned driver from using a replacement turn's directory
([0020](0020-run-state-locking.md)); a supervisor makes the same check of its own record before it
starts anything.

**Steering and cancelling go through files**, so a restarted agentmux can reach a running driver:

- The turn's mailbox (`appserver::mailbox`) gives each request one decision file, which exactly one
  of the driver (claiming) and the caller (withdrawing) can create. A withdrawn request is never
  sent.
- Every steer carries `expectedTurnId`, so a late request cannot open another turn. Acceptance is
  reported only after the acknowledgement names the expected turn and the instruction has been
  appended to the capture. A sent request is unconfirmed whenever agentmux cannot establish and
  record its acceptance, including a missing or invalid acknowledgement and a capture failure after
  acceptance.
- Once the driver observes the `cancelling` marker, it stops submitting new steers, rejects waiting
  requests, and interrupts a started turn. Requests already in flight are settled separately.

`RunStore::accepts_steers` is the one predicate behind both `RunStatus::steerable` and `steer`'s
admission, and a turn whose capture does not yet say `turn.started` is refused as
`SteerError::Starting`, so a steer never waits on setup.

**The sandbox is set on the thread and the network on the turn.** Threads start and resume with
the consultation's sandbox mode; a workspace-write network choice is a `-c` on the server's command
line, as `exec` passes it; and a read-only delegate with network sends
`sandboxPolicy: {type: readOnly, networkAccess: true}` with every `turn/start`
(`protocol::turn_sandbox_policy`), because the legacy read-only mode has no network switch. `exec`
reaches the same policy through a permission profile
([0026](0026-delegate-permissions.md)).

**Nobody is there to approve.** Threads start and resume with `approvalPolicy: never` and
`approvalsReviewer: user`, so a reviewer the loaded configuration names cannot approve anything. Approval and
input requests are declined; requests requiring unsupported client capabilities receive a JSON-RPC
error, because a consultation has nobody available to answer them.

**What the turn left running is stopped, within a budget, and reported.** Cleanup is bounded and
best effort: the driver reads its descendants with their parents and closes the server's input to
allow session saving, then signals and rescans surviving descendants within its cleanup budget
(`appserver::STOP_BUDGET`), and looks once more after its last signal. What survived the server
from outside its process tree, as it stood before the server was asked to leave, is kept in the
turn's `left-running.json` and reported in the transcript and `status`
([0032](0032-leftover-processes-reported.md)). The store allows that budget plus a margin before
falling back to signalling the driver. The platform mechanisms —
adopting orphans on Linux, a once-a-second record elsewhere, a job object on Windows — are
documented in `crates/agentmux/src/appserver/mod.rs`.

## Consequences

- Isolation is never traded for steering silently: a consultation is steerable only when it loads
  its account's configuration, which is the default unless the caller or the machine file says
  otherwise. The codex release whose app-server accepts `--ignore-user-config` lifts
  this, and `codex-app-server-codegen refresh` refuses such a release as the cue to revisit it.
- `--codex-launch exec`, `AGENTMUX_CODEX_LAUNCH=exec`, or `codex_exec` on one consultation runs
  every Codex turn as `exec` instead; nothing is steerable then.
- Failed subreaper or job setup weakens containment. Windows processes explicitly allowed to break
  away are outside the job's cleanup guarantee.
- On macOS any process started and orphaned between two once-a-second checks is out of reach, not
  only one a crashed server's command started: a job a command backgrounds is orphaned as soon as
  its shell exits.
