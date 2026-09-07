## agentmux

[![build](https://github.com/romnn/agentmux/actions/workflows/build.yaml/badge.svg)](https://github.com/romnn/agentmux/actions/workflows/build.yaml)
[![test](https://github.com/romnn/agentmux/actions/workflows/test.yaml/badge.svg)](https://github.com/romnn/agentmux/actions/workflows/test.yaml)

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
brew install --cask romnn/tap/agentmux
```

Or from source:

```bash
cargo install --git https://github.com/romnn/agentmux agentmux-cli
```

There is no crates.io release: the name is taken by an unrelated crate.

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
| `cancel`    | Stop a running consultation, keeping everything collected so far; returns once it has  |
| `list`      | Recent consultations with their ids, so an id can be recovered                       |
| `quota`     | What each configured account has left, in the vendor's own numbers — free, no tokens |

### Use from the command line

The same nine verbs are available directly, as a unified CLI over both vendors:

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

# Every command takes --json; a consultation's state is always under `outcome`.
agentmux status <run-id> --json | jq -r .outcome.state

# Which accounts this machine has, and what each has left.
agentmux accounts
agentmux quota
```

### Accounts

An agent asking for a second opinion knows it wants "the personal account". It has no way to know
where that account's credentials live, and the answer differs on every machine. `agentmux.toml`
is the map between the two, so one prompt works everywhere:

```toml
# ~/.config/agentmux/agentmux.toml

# Applied to every delegate launch, before any account chooses an identity.
[launch]
env = { DISABLE_HOOKS = "true" }
env_passthrough = ["AWS_PROFILE"]
# The only names a single call's `env` may set. Nothing, unless listed here.
request_env = ["REVIEW_MODE"]

# Used when a caller names no account.
[defaults.claude]
account = "personal"

# A second logged-in profile.
[accounts.claude.personal]
config_dir = "~/.claude-personal"
description = "Personal Max subscription"
# Run this account's delegates with its own settings, hooks and MCP servers.
inherit_settings = true

# A key kept out of the file, read from the environment at launch.
[accounts.claude.ci]
api_key_env = "CI_ANTHROPIC_KEY"

# A gateway that authenticates with a bearer token rather than a key (Claude only).
[accounts.claude.gateway]
base_url = "https://llm-gateway.example.com"
auth_token_env = "GATEWAY_TOKEN"

# A locally served OpenAI-compatible endpoint.
[accounts.codex.local]
base_url = "http://localhost:11434/v1"
api_key = "ollama"
```

Aliases are opaque, exactly like model identifiers: agentmux keeps no roster, and naming one that
does not exist returns an error listing the ones that do. Omit `account` entirely and the vendor
CLI's own configuration is used, which is right on a machine with one login per vendor and needs
no file at all.

Choosing an account **withholds** that vendor's credential variables from the host environment —
and from the `[launch]` layer — so an exported `ANTHROPIC_API_KEY` cannot silently outrank the
account you asked for. That is the whole point: without it a caller believes it switched accounts
while spending another. With no account named, a Codex delegate follows the host's `CODEX_HOME`; a
Claude delegate deliberately does not follow the host's `CLAUDE_CONFIG_DIR`, because under Claude
Code that variable names the launching session's own identity, and runs against the CLI's default
directory instead.

**Two files, two jobs.** A *machine* file — `$AGENTMUX_CONFIG`, `~/.config/agentmux/agentmux.toml`,
`~/agentmux.toml`, or the platform config directory when it lies under your home — may define
accounts. `AGENTMUX_CONFIG` must be absolute and is the one way to keep the file anywhere else;
`XDG_CONFIG_HOME` and `APPDATA` count only when they point under your home, so a container that
points them at a workspace cannot turn a checkout into a machine file. A *project* file, found by
walking up from the delegate's working directory to the root of its repository (the nearest
`.git`), or to just below `$HOME` when it is in none, may only **select** one:

```toml
# <repo>/agentmux.toml — safe to commit
[defaults.claude]
account = "clientx"
```

A project file that tries to define an account, or a `[launch]` table, is refused by name. It has
to be, because such a file arrives with a `git clone`: were it able to name a `base_url` and an
`api_key_env`, cloning a repository and asking for one second opinion would send your key to
whoever wrote it. For the same reason a project file may choose *which* of your accounts pays but
nothing more: an account it selects runs isolated unless the call says otherwise, and does not
bring its own `request_env` with it. Either file is read only when you own it and nobody else can
write it, and a file that cannot be parsed is reported by position, never by quoting the line.

### Rate limits

Every consultation records the usage window its vendor reported, and a rate-limited failure says
when the window reopens rather than only that it closed — the difference between "wait four
minutes" and "wait nine hours, go elsewhere". On a refusal, agentmux also reports what the
machine's *other* accounts have left, so the next call is obvious.

`agentmux quota` asks the same question at any time. It costs nothing and spends no tokens: Claude
answers from the cache it keeps itself, refreshed with its own free `/usage`; Codex answers over
its app-server protocol.

The numbers are the vendor's own, passed through unchanged. agentmux does not rank accounts,
because a percentage means nothing without the plan behind it — every absolute figure comes back
null on a subscription — and because a per-model window like `Fable` is reported by display name
while agentmux only ever sees an opaque model id. An account that could not be asked is reported
as **unavailable, not idle**; an unauthenticated configuration directory answers "0% used"
cheerfully, and anything ranking on "least used" would route straight to the broken one.

### Environment

Three layers reach the delegate, each overriding the last: the `[launch]` table above, then the
chosen account's own `env`, then whatever one call asks for — `--env KEY=VALUE` on the CLI, an
`env` object on the MCP tools. Useful when a consultation inherits its account's settings and that
tooling reads configuration of its own; an isolated delegate loads nothing that would read it.

A per-request `env` may set only the names `agentmux.toml` lists under `request_env`, and by
default that is nothing. A request arrives from a delegating agent acting on text it was given, and
there is no list of names dangerous enough to refuse that stays complete: `PATH` chooses which
binary runs, `NODE_OPTIONS` runs code inside the CLI before it reads a setting, and the next runtime
will read one more. So the operator names what may be set, in a file no request can reach. An
account's own `request_env` applies when the machine file or the call chose the account, not when
a checkout's project file did. The values of `env` are never printed back, so a key put there
stays there. On Windows, where the operating system reads environment names without regard to
case, agentmux spells every name in upper case on the way in, so `path` and `PATH` are one name
everywhere it looks.

### Isolation, and opting out of it

By default a delegate loads **no** user, project or local settings, no hooks and no MCP servers.
Three things follow. A blocking `Stop` hook cannot reopen the finished turn and blank its result.
The delegate cannot re-enter `agentmux` through a configured MCP server and recurse. And what the
delegate saw does not depend on the machine it ran on.

That default is sometimes wrong. A reviewer meant to exercise the project's own tooling needs that
tooling, and a hook you want applied to every model you run is not usefully suppressed here. So it
can be turned off, per account or per consultation:

```bash
agentmux ask --delegate claude --model claude-opus-5 --effort xhigh \
  --inherit-settings "Run the project's own checks and tell me what fails."

agentmux ask ... --isolated      # override an account that asks to inherit
```

The MCP tools take `inherit_settings: true` for the same thing. Isolation is imposed by
*argument*, not by environment — no variable switches it on or off — so this is the only way to
change it.

What inheriting does **not** change is what the delegate may do: plan mode and the read-only tool
list stay, because whose configuration is loaded is a different question from what may be edited.

Every delegate is launched with `AGENTMUX_DELEGATE=1`, and `agentmux` refuses to launch a
delegate — or to serve as an MCP server at all — when it finds itself inside one. An isolated
delegate could never reach `agentmux` anyway; an inheriting one loads your own MCP servers, and
`agentmux` may be among them. The variable is not the only witness, because Codex starts its MCP
servers with an environment of its own choosing that does not carry it: `agentmux` also walks its
own ancestry and refuses when any ancestor — however many shells or version-manager shims sit in
between — is a delegate it launched that is still running. That second witness needs both
processes to open the same state directory, which Codex's own allowlist cannot tell the nested
one about; with `AGENTMUX_STATE_DIR` or `XDG_STATE_HOME` set to something other than the
platform default, a Codex delegate that inherits an MCP configuration naming `agentmux` is
guarded by the variable alone.

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

### Stopping, and what survives a restart

`cancel` marks the turn as cancelling, asks the delegate to stop, waits a few seconds, makes it
stop if it did not, and records the cancellation once the process has actually gone. The footer
it writes is the last thing the transcript will ever say, so it cannot be written while the child
is still able to say more; whatever the child wrote before going is kept, and a delegate that had
already finished its turn when the signal landed is reported as having finished. A `status` or
`tail` that happens to see the child go first records the same cancellation, and a child that
somehow survives both signals is stopped again by the next `cancel`. `prune` refuses a running
consultation — or a finished one whose process is still there — for the same reason: deleting
its record would leave the child running, billing, and unreachable.

A delegate that exits without a closing event is recorded once, with its exit status and the tail
of its stderr, and the transcript is rendered from that record from then on — never from files a
tool subprocess the delegate started may still be writing to.

A delegate outlives the `agentmux` that started it. While that process lives it holds the child's
handle; afterwards the pid on disk is trusted only together with the process start time recorded
beside it, so a recycled pid is never mistaken for a delegate. On Windows a restarted `agentmux`
can stop a delegate but only ever reaches the direct child, not the tool subprocesses it started.

### License

MIT — see [LICENSE](LICENSE).
