## agentmux

[![build](https://github.com/romnn/agentmux/actions/workflows/build.yaml/badge.svg)](https://github.com/romnn/agentmux/actions/workflows/build.yaml)
[![test](https://github.com/romnn/agentmux/actions/workflows/test.yaml/badge.svg)](https://github.com/romnn/agentmux/actions/workflows/test.yaml)
[![crates.io](https://img.shields.io/crates/v/agentmux)](https://crates.io/crates/agentmux)

`agentmux` delegates a question to another vendor's coding agent and captures the whole transcript.

A coding agent running inside one harness — Claude Code, Codex — spawns the *other* vendor's CLI,
runs a prompt through it, and gets back every message of that run: the assistant turns, the tool
activity, the injected hook feedback, the failure, and where each of them came from.

### Why

Delegating to a subagent of your *own* vendor is already native to both harnesses. Reaching across
vendors is not, and the naive way to do it — shell out, read the final message — loses precisely the
part worth having:

- A blocking `Stop` hook can reopen a finished turn and leave the final result an **empty string**
  while the run reports success. The actual report is an earlier message; a last-message-only reader
  sees nothing at all.
- Codex emits `item.completed` events of type `error` that are **warnings**, not failures. Treating
  them as terminal turns every successful consultation with a warning into a failed one.
- An unknown model comes back as an ordinary "success" envelope with an error flag set inside it.

`agentmux` reads the streaming JSON both CLIs already emit, keeps every message with its source
attached, and never hides a failure by discarding the messages collected before it.

It is deliberately **model-agnostic**: model ids and reasoning-effort values are opaque strings
passed straight through. A new model never requires an `agentmux` release, and when a value is
wrong, the delegate CLI's own error — which names the values it does accept — is surfaced verbatim.

### Installation

```bash
brew install romnn/tap/agentmux
```

Or from source:

```bash
cargo install agentmux-cli
```

You also need whichever delegate CLIs you intend to use (`claude`, `codex`) on your `PATH`, already
authenticated.

### Use as an MCP server

`agentmux mcp` speaks the Model Context Protocol over stdio.

**Claude Code**

```bash
claude mcp add agentmux -- agentmux mcp
```

**Codex** — add to `~/.codex/config.toml`:

```toml
[mcp_servers.agentmux]
command = "agentmux"
args = ["mcp"]
# Codex kills a tool call after 60 seconds by default, which is shorter than the wait `ask` and
# `result` accept. Raising it lets a caller block for a whole short consultation.
tool_timeout_sec = 600
```

### Tools

Both hosts cap how long a tool may block — Codex at 60 seconds by default, Claude Code at ten
minutes — and both cap how much a tool may return. A review takes longer than either. So no tool
waits for a whole review: `start` returns an id the moment the delegate is running, the child is
detached and survives an `agentmux` restart, and every result carries the path to the transcript on
disk. The inline text is a bounded convenience; the file is the record.

| Tool        | What it does                                                                       |
| ----------- | ---------------------------------------------------------------------------------- |
| `ask`       | Start a consultation and wait up to `wait_seconds` for it — the one-call path       |
| `start`     | Start one and return the id immediately, for work that will take minutes            |
| `status`    | State, elapsed time, message count, token usage, and anything that needs a warning  |
| `tail`      | The transcript written since a cursor, plus the next cursor — call it on a loop     |
| `result`    | The complete transcript, paged, optionally after blocking for the consultation      |
| `follow_up` | Ask one more question, resuming the delegate's own session so the brief is retained |
| `cancel`    | Stop a running consultation, keeping everything collected so far                    |
| `list`      | Recent consultations with their ids, so an id can be recovered                       |

### Use from the command line

The same eight verbs are available directly, as a unified CLI over both vendors:

```bash
# One consultation, start to finish.
agentmux ask --delegate codex --model gpt-6-astra --effort high \
  'Review the diff against main for correctness bugs. Cite file:line.'

# A long review, watched as it arrives.
agentmux start --delegate claude --model claude-opus-5 --effort xhigh --keep \
  --file review/brief.md
agentmux tail <run-id> --follow

# One more question, in the delegate's own session.
agentmux follow-up <run-id> 'Which of those would you fix first, and why?'

agentmux list
agentmux prune            # drop consultations past their retention
```

`--account personal` selects the second Claude account on a machine that has two: it points the CLI
at `$HOME/.claude-personal` and withholds `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`, so the
session authenticates as that account rather than spending an API key that happens to be exported.

### What the delegate does and does not see

The child environment is built as an **allowlist from empty**, never by filtering a denylist. It
gets `PATH`, `HOME`, locale, proxy and CA settings, and the credential variables its own vendor
reads — and nothing else. None of the launching agent's session identity reaches it.

A Claude delegate runs with `--setting-sources ""`, so no user, project or local settings load and
no hook can reopen its finished turn, and with `--strict-mcp-config` and an empty MCP config so it
cannot re-enter `agentmux`. It runs in plan mode and is offered no editing tools.

A Codex delegate runs with `--ignore-user-config`, which is what actually detaches the machine's
configured MCP servers — `-c mcp_servers={}` looks like it should and does not. Measured against
codex-cli 0.153.4, a delegate launched with that override still called a configured server's tool,
so a machine with `agentmux` in its own Codex config could have had a delegate call `agentmux` and
recurse. It also runs with `features.hooks=false` and a read-only sandbox by default.

One thing does still reach a Claude delegate: `--setting-sources ""` does **not** suppress
`CLAUDE.md` discovery, so a project's own instruction file is part of the delegate's prompt.
`agentmux` says so in the result rather than hiding it.

### License

MIT — see [LICENSE](LICENSE).
