# 0020. A run changes shape only under its lock

Status: Accepted

## Context

Several agentmux processes may address one consultation — an MCP server and a terminal, or a server
restarted underneath a `tail`. Every change to a run's shape is a check followed by a write: two
callers folding a one-turn consultation both choose index 1 for a follow-up, and a reader that
settles a turn while another publishes an older reading would hand out two transcripts. Another
process could act between any check and its write.

## Decision

**Turn admission, settlement, reply recovery, transcript publication, the commitment of a turn's
`settled.json`, a supervisor's start of its delegate and removal of recorded runs are serialized by
the run lock.** A `run::LockedRun` (`crates/agentmux/src/run/lock.rs`) is the
guard; only `RunStore::lock` makes one.

- Folding a turn (which may settle it), recovering a late reply, publishing the render, committing
  a settlement (`LockedRun::fold_and_publish`) and claiming and launching a turn are methods of
  `LockedRun`.
- Finalizing a turn (`RunStore::finalize_turn`) and a supervisor's admission, which checks its
  record and spawns and records the delegate under one hold, are `RunStore` methods that hold
  one ([0025](0025-eager-settlement-and-terminal-waiting.md)).
- Marking and recording a cancellation, removing the run, the retention sweep and submitting a steer
  are `RunStore` methods that hold one across their check and write.
- A fold and the publication of its render happen under one hold, so no reader can publish a reading
  another caller has since settled differently.
- Nothing reachable from a `LockedRun` takes the lock again: a second lock on the same file from one
  process waits on the first for ever.

Initial run creation and failed-start cleanup are outside that protocol, and so are the per-turn
usage-cache writes, which are written once and never changed.

**The lock file is `runs/<id>.lock`, beside the run directory, and is never unlinked.** Removing a
run removes nothing another process may be holding, and two processes locking two different inodes
under one name would each believe they held the lock. It is an advisory lock the operating system
releases when its holder dies.

**A turn directory is a turn once it holds a launch, supervisor or exit record.** Creating the
directory is the claim on its index, and the launch — the delegate's, or its supervisor's — is
recorded before the lock is released. A directory without any of them found under the lock is
therefore an abandoned claim (`claim_turn` in
`crates/agentmux/src/run/spawn.rs`). It is renamed outside the indexed turn sequence before the
index is reused, preserving files an unrecorded child may still hold open. This does not discover or
stop that child. The app-server driver must confirm its own launch record before starting work
([0018](0018-codex-app-server-driver.md)).

## Consequences

- A new mutation of a run belongs on `LockedRun`, or holds one across its check and write.
- A helper called while a `LockedRun` is held must not call `RunStore::lock`.
- A caller that died between claiming and recording leaves a claim the next follow-up takes over
  rather than one that blocks the consultation for good.
