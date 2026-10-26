# 0028. A label belongs to the turn it was given for, and later turns inherit it

Status: Accepted

## Context

A caller running many consultations has to know what each run id is for. Without a field for it, it
keeps a registry of its own beside the store, which drifts: a follow-up that gives a consultation a
new purpose duplicates a row, and a forgotten `start` leaves none. A consultation's purpose can
change between turns — a review, then a re-review after rework — so one label per consultation
cannot say what the turn running now is for.

## Decision

**`start` and `follow_up` take an optional `label`, which is recorded only with the turn it was
given for, and every later turn inherits it until another is given.** `RunLabel`
(`crates/agentmux/src/run/label.rs`) is one trimmed line of at most 120 characters with no control
character or line break, so every listing prints it on one line. The first turn's `invocation.json`
holds the label `start` was given, and a follow-up's holds the one it was given; a turn given none,
an automatic resume ([0027](0027-automatic-resume-after-overload.md)) among them, holds none.
`RunStore::label_of_turn` (`crates/agentmux/src/run/spawn.rs`) reads a turn's label as the one it
was given or else the latest an earlier turn was. `status` and `list` report the newest turn's, and
`wait` the label of the turn it returns.

Each label is stored once, where it was given, rather than copied forward or kept in `meta.json`. A
copy would have to be written by every path that launches a turn, and a turn whose record is lost
would take the label back to whatever the consultation's record still said. A turn's label is fixed
before its child is spawned, so it renders under the turn's question as the transcript's other
launch facts do ([0009](0009-append-only-transcript.md)), and the transcript shows where the purpose
changed.

`list` filters by a substring of the current label, ignoring case, and by the newest turn's state,
before it applies its limit. Each summary carries the label, the working directory, the vendor,
model and effort the newest turn ran ([0026](0026-delegate-permissions.md)), its outcome with the
failure kind, and its last write, so a caller watching its consultations needs one call.

## Consequences

- A consultation recorded before labels existed has none, and lists, renders and filters exactly as
  it did.
- Finding the current label reads launch records back from the newest turn until one holds a label;
  a consultation never labelled reads all of them, which is a few small files.
- A filter finds a consultation by what it is for now: a consultation renamed by a follow-up is no
  longer found by its first label.
- A label is a caller's note, never an identity: two consultations may carry the same one, and
  nothing is looked up by it but `list`'s filter.
