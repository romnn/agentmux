# 0016. Probe session-store write restrictions before every turn

Status: Accepted

## Context

A delegate inherits whatever confines agentmux. Run from an agent's sandboxed shell — Codex's
`workspace-write` exec, say — agentmux hands that sandbox to its child. Measured with claude 2.1.270
under Codex's Seatbelt profile: the delegate answers, the kernel denies its transcript write under
`~/.claude/projects`, print mode says nothing, and every follow-up fails with "No conversation
found". The follow-up is paid for before it fails.

## Decision

`launch_into` (`crates/agentmux/src/run/spawn.rs`), the consultation-turn launch path, probes the
vendor's session store before every launch:

- The store is `Vendor::session_store`, resolved from the environment the child gets, request names
  included.
- The probe (`can_create_files_in`) creates and removes a file in the store, or creates the store
  itself when the CLI has not made it yet, because a sandbox that grants the store alone must pass.
- Only a permission or read-only refusal counts. When the session-store path can be resolved, the
  probe is attempted; a missing parent normally yields `NotFound`, which is not treated as evidence
  of confinement, because a CLI that is not set up there says nothing about confinement.

This is a best-effort confinement check, not proof that the CLI will save its session successfully.

A refusal is recorded in the turn's invocation record (`invocation.json`), before the launch record
that makes the directory a turn, so the fold sees it from the turn's first render. It becomes
`Turn::unsaved_session_store`, rendered under the question ahead of anything the stream adds, and
the newest turn's is `RunStatus::unsaved_session_store`.
`transcript::describe_unsaved_session_store` is the one phrasing every reader uses.

An unwritable store warns but does not block the requested turn, so the caller still receives its
answer. It blocks subsequent resumption: `resumable_session` refuses a follow-up with
`NotResumable::SessionNotSaved` rather than let the CLI fail after being paid for. The launch-time
finding remains attached to that turn even if permissions later change.

## Consequences

- Each turn is probed, because each inherits the confinement of whoever launched it: a consultation
  begun through an unconfined server can be continued from a sandboxed shell.
- The remedies are to launch through the MCP server, which a host runs outside its sandbox, or to
  grant write access to the session directory alone (`~/.claude/projects`, `~/.codex/sessions`).
