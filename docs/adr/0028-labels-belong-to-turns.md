# 0028. A label belongs to the turn it was given for, and later turns inherit it

Status: Accepted

## Context

A caller running many consultations has to know what each run id is for. Without a field for it, it
keeps a registry of its own beside the store, which drifts: a follow-up that gives a consultation a
new purpose duplicates a row, and a forgotten `start` leaves none. A consultation's purpose can
change between turns — a review, then a re-review after rework — so one label per consultation
cannot say what the turn running now is for.

## Decision

**`start` and `follow_up` take an optional `label`, and each turn records the label it carries.**
`RunLabel` (`crates/agentmux/src/run/label.rs`) is one trimmed line of at most 120 characters with
no control characters, so every listing prints it on one line. `start` records its label in
`meta.json`, which the transcript header shows, and in the first turn's `invocation.json`. A
follow-up that names a label records it in its own turn's `invocation.json`; one that names none
records the label its newest turn carried, and an automatic resume does the same
([0027](0027-automatic-resume-after-overload.md)). The newest turn's record therefore holds the
consultation's current label, which `status` and `list` report, and `wait` reports the label of
the turn it returns.

The label is kept with the turn rather than rewritten in `meta.json` for the reason the turn's
permissions are ([0026](0026-delegate-permissions.md)): the header rendered from `meta.json` is
part of the append-only transcript ([0009](0009-append-only-transcript.md)), and a record a later
turn rewrote could not say what an earlier turn was for.

`list` filters by a substring of either the label `start` gave or the current one, ignoring case,
and by the newest turn's state, before it applies its limit. Each summary carries the label, the
working directory, the vendor, model and effort the newest turn ran, its outcome with the failure
kind, and its last write, so a caller watching its consultations needs one call.

## Consequences

- A consultation recorded before labels existed has none, and lists, renders and filters exactly as
  it did.
- Finding the current label reads one small record per consultation; a state filter folds each
  consultation's newest turn, as `list` already did, and stops once the limit is filled.
- A label is a caller's note, never an identity: two consultations may carry the same one, and
  nothing is looked up by it but `list`'s filter.
