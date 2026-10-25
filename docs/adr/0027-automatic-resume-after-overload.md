# 0027. A turn that fails because the model is overloaded is resumed by its own supervisor

Status: Accepted

## Context

A long turn can fail with a transient vendor error hours into the work: Codex reports `Selected
model is at capacity. Please try a different model.`, Claude an `overloaded_error` with status
529. The delegate's session is intact, and a follow-up saying "continue" carries on from where it
stopped, but nothing sends one. A caller that looks in once a day loses a night of work to a
failure that waiting thirty seconds would usually have cured.

The only agentmux process still alive when such a turn fails is its supervisor
([0025](0025-eager-settlement-and-terminal-waiting.md)). It was started with the environment
resolved for its delegate, not the host's, so it carries `AGENTMUX_DELEGATE=1`
([0007](0007-recursion-guard.md)), and it holds neither the host environment nor the machine file
that environment was resolved from ([0022](0022-delegate-env-from-allowlist.md)).

## Decision

**Overload is a failure kind of its own.** `stream::classify` reads `FailureKind::Overloaded`
(label "model overloaded") from `overloaded` as a whole word, which covers Claude's
`overloaded_error` and an OpenAI `server_is_overloaded`, or from "at capacity" together with the
whole word "model"; Claude's `api_error_status` 529 says the same structurally. Capacity alone is
not enough, because the text classified includes a CLI's stderr, where a full disk is at capacity
too.

**The turn's supervisor resumes the consultation, with "continue", in the same session.** The model
is never changed. The policy, `AutoResumePolicy` in `crates/agentmux/src/run/auto_resume.rs`, is:

| Parameter       | Value      | Meaning                                                                   |
| --------------- | ---------- | ------------------------------------------------------------------------- |
| `first_delay`   | 30 seconds | the wait before the first automatic resume of a run                       |
| doubling        | ×2         | each further resume in an unbroken run waits twice the one before         |
| `longest_delay` | 15 minutes | no wait is longer                                                         |
| `attempts`      | 12         | automatic resumes in a row that may fail overloaded before agentmux stops |

A run is an unbroken sequence of turns that failed overloaded; a turn that ends any other way,
including a caller's own follow-up, ends it, and the next overload starts again at thirty seconds.
Twelve resumes wait about an hour and forty-five minutes in all. `AutoResumePolicy::DEFAULT` is the
one place to change these; the hidden `--auto-resume-delay` flag shortens the first delay for tests.

1. **Deciding.** Whoever commits the newest turn's settlement decides, from that turn's records,
   and writes `auto-resume.json` just before `settled.json`, under the same hold of the lock
   (`LockedRun::schedule_auto_resume`). A reader that finds the settlement — a `wait` above all —
   therefore finds the decision. The turn qualifies when it failed overloaded, nobody asked to
   cancel it, and its `supervisor-plan.json` carries an `auto_resume` plan. Its own
   `invocation.json` says how many automatic resumes preceded it (`auto_resume_attempt`); one past
   the policy's attempts is recorded `exhausted`. Otherwise the resume is recorded `scheduled`, with
   its attempt and due time, only while the turn's supervisor is alive and the consultation passes
   `resumable_session` ([0023](0023-resume-after-delegate-exit.md)).
2. **Waiting.** The supervisor, once its turn has settled, sleeps until the resume is due, looking
   every second for `auto-resume-cancelled` and for a next turn somebody else started.
3. **Claiming.** Under the run lock it checks the marker again, folds and publishes, requires its
   turn to still be the newest and `resumable_session` to pass, and launches the next turn with
   the question `continue` (`RunStore::resume_when_due`). A `cancel` or a follow-up is therefore
   either seen by that check or finds the resumed turn already there
   ([0020](0020-run-state-locking.md)).

**The resumed turn runs as the turn it continues would have run.** The plan's `auto_resume` carries
the policy, the supervisor executable and the Codex app-server driver of the store that launched
the turn, and `agentmux supervise` opens its store with them, so the resumed turn is supervised and
driven the same way, and resumes again on the same terms.

**Its environment is the previous turn's resolved environment, unchanged.** The supervisor's own
environment is exactly what the launching process resolved for the turn being continued: the
allowlisted base, the machine's launch layer, the account's identity, the request's `env`, the
billing scrub and the recursion marker. `Delegate::continuation` builds only the arguments, for a
resume of the recorded session, and hands that environment on, as a supervisor already hands it to
its own delegate. Rebuilding it through `Delegate::invocation` from the supervisor's environment
would be wrong rather than merely redundant: the allowlist has already dropped what the machine
file is found by (`AGENTMUX_CONFIG`, `XDG_CONFIG_HOME`) and the host variables an account's
`env_passthrough` or `${VAR}` secrets read, so it could load another machine file, or none, and
fail or change identity. The turn is held to the limits the turn it continues was admitted under,
for the same reason: there is no configuration to re-read. That is the run the overload
interrupted, continued.

**The recursion guard does not apply to an automatic resume.** `LockedRun::launch_turn` refuses
nothing for a `Resolution::Inherited` launch, which only `auto_resume` builds, inside the
supervisor of a turn whose launch already passed `refuse_to_spawn`. The marker in its environment
is the one that launch set for its delegate; refusing on it would refuse the supervisor for its own
handiwork. No caller can reach that path: it needs a plan an admitted launch wrote, read by the
supervisor that plan's record names.

**The automatic turn says it is automatic.** Its `invocation.json` records `auto_resume_attempt`,
its `question.md` is `continue`, and the failed turn keeps `auto-resume.json`. Existing bytes are
never rewritten ([0009](0009-append-only-transcript.md)).

**What callers are told.** `RunStore::auto_resume` reads the decision together with what happened
since, and `RunStatus::auto_resume` reports it for the newest turn: `scheduled` (turn, attempt,
attempts, due time), `resumed`, `cancelled`, `abandoned` (the supervisor went without resuming;
its log is named) or `exhausted`. Both front ends print one line for it
(`report::auto_resume_line`). The MCP failure advice says what agentmux is doing in place of what
the caller should do, and `next` offers `wait` for the turn the resume starts, once it has begun,
and `cancel`, rather than a `follow_up` that would take the resume's place. `wait` on the failed
turn still returns it as failed, with the same exit status; it adds the resume and the turn to wait
for next.

**`cancel` stops a pending resume.** On a consultation whose newest turn failed overloaded, `cancel`
writes `auto-resume-cancelled` under the lock, whether or not the resume has been decided yet. The
supervisor sees it while waiting, or when it claims, and leaves without starting anything; the turn
stays failed.

## Consequences

- An overloaded consultation continues overnight with nobody asking, and the transcript shows each
  automatic turn as a `continue` agentmux sent.
- A turn that fails overloaded before the delegate said anything, or without a session, is not
  resumable and is not resumed; it is reported as an ordinary failure.
- A store opened without a supervisor never resumes anything, and neither does a turn whose
  supervisor died before its settlement was committed.
- A supervisor that dies while waiting leaves the decision on disk; readers report the resume as
  abandoned rather than promise it, and the consultation can be followed up by hand.
- `wait --turn N+1` cannot be issued before the resumed turn exists: a wait pins only a turn that
  has begun, so the caller waits until the due time has passed.
- While a supervisor waits to resume, the run is not removable: `prune` and the sweep refuse a run
  whose supervisor is alive.
- A limit the machine file tightened after a turn began does not reach that turn's automatic
  resumes; the next caller's follow-up is held to it.
