# 0033. A delegate's questions are surfaced as asked, never as answered

Status: Accepted

## Context

A Codex delegate can ask the caller a question in the middle of a turn. Codex offers
`request_user_input_async` in its default collaboration mode. The tool returns
`{"accepted":true}` at once, and the model carries on under an assumption of its own, or polls for
a reply with `sleep` and `wait`. Codex emits the question as an `agentMessage` item that carries
`delivery: "async"` and `questions: [{title, options}]`, and whose id is the tool call's
(`codex-rs/core/src/tools/handlers/request_user_input_async.rs`). The blocking `request_user_input`
tool is offered only in Plan mode, and agentmux answers its server request with an empty answer
([0018](0018-codex-app-server-driver.md)).

`codex exec` writes that item as a plain reply and drops `questions`. So did the app-server
translator, and a caller had no way to tell a question from a report. Questions went unanswered for
hours while the delegate went on.

Codex's own client keeps such a question pending until it is answered by id or the live turn ends.
agentmux has neither signal. A steer or follow-up is plain text that names no question; an
unrelated steer, or one acknowledged while another question arrives, would be read as an answer;
and an automatic resume ([0027](0027-automatic-resume-after-overload.md)) ends a turn without
anybody answering anything.

## Decision

**A question is recorded where it was asked.** The translator
(`crates/agentmux/src/appserver/translate.rs`) writes an `agentMessage` with a non-empty
`questions` as agentmux's own capture event, `agentmux.question`, with the item id, the server's
`completedAtMs` as `asked_at`, and the questions. The fold (`stream::codex`) turns it into a
`MessageSource::Question` message, part of the report and rendered as `### assistant (question)` in
codex's own layout. It also records each question on `Turn::delegate_questions`, numbered from 1 in
capture order across the turn.

**Every name says whose question it is.** A consultation's `question` is the caller's brief: the
`question` of `start` and `follow_up`, the transcript's `Turn N — question`, `list`'s `question`
field. So the delegate's are `delegate_questions` in the JSON, and the wait's switches are
`or_delegate_question` and `delegate_questions_seen` (`--or-delegate-question`,
`--delegate-questions-seen`), which no caller can read as being about its own brief.

**agentmux reports that a question was asked, never whether it was answered.**
`RunStatus::delegate_questions` lists every question of every turn, with its turn and number, and
nothing ever removes one: a later steer, a follow-up or an automatic resume leaves it listed.
`RunSummary::delegate_questions` counts them across the consultation, so `list` still shows a
question after a resume has begun a turn that asked nothing. Both front ends say what was asked
through `Warning::Asked`, name `steer` or `follow_up` as the way to reply, and say that agentmux
does not track replies. A steer stays plain text.

**A wait can return on a question, by opting in.**
`RunStore::wait_for_settlement_or_delegate_question`, behind MCP `wait` with
`or_delegate_question` and `agentmux wait --or-delegate-question` (exit status 12), also returns
while the pinned turn runs, once it has asked more than `delegate_questions_seen` questions. It
returns `Asked`, the pinned turn and the questions beyond that count, built from one read of that
turn's capture, never from `RunStatus`. It hands every one of them over whole and verbatim, because
the caller acknowledges them by number; the headers of `status` and `tail`, which a caller may
poll, show only the newest few, clipped. The caller rearms the wait with the number of the last
question it returned. That is how it declines a question without steering the delegate and still
hears of the next one. A plain `wait` is unchanged, and returns only on settlement
([0025](0025-eager-settlement-and-terminal-waiting.md)).

A question returns a wait only while nothing has begun to end the turn. Each look reads the
capture's complete records with `read_complete_records` and takes no lock. It counts no question
when the settlement marker, an exit record or the cancellation marker exists, before the read or
after the fold, or when the fold's stream has ended. A cancellation creates its marker before it
chooses the length it freezes ([0009](0009-append-only-transcript.md)), so a question written past
that length never returns a wait. A settlement found on the same look wins, and a deadline is
checked only after both looks, so a turn that settled before it is never reported pending. One
look folds the whole capture, so questions are looked for every two seconds, settlement as often as
before. The `--or-delegate-question` wait takes one consultation.

## Consequences

- `codex exec` consultations, and captures recorded before this record, show questions as plain
  replies: exec's JSON does not carry them, and its turns cannot be steered anyway.
- Claude delegates have no such tool, so they never ask.
- A caller that replies does not close anything. Status keeps listing the question, and the
  rendered warning says agentmux does not track replies.
- `a_cancelling_turn_asks_nothing` and `a_half_written_question_is_read_once_its_line_is_complete`
  (`crates/agentmux/tests/settlement.rs`) pin the read rules, and
  `agentmux_wait_returns_on_a_question_and_exits_12` (`crates/agentmux-cli/tests/steer.rs`) runs
  the command against the mock app-server's `AsksThenSteerable` scenario.
