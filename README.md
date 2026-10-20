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
claude mcp add agentmux -- agentmux mcp --deny claude
```

That writes the entry below, which can also be added by hand: to `~/.claude.json` under
`mcpServers` for every project (`--scope user`), or to a checkout's `.mcp.json` (`--scope project`).

```json
{
  "mcpServers": {
    "agentmux": {
      "command": "agentmux",
      "args": ["mcp", "--deny", "claude"]
    }
  }
}
```

**Codex** — add to `~/.codex/config.toml`:

```toml
[mcp_servers.agentmux]
command = "agentmux"
args = ["mcp", "--deny", "codex"]
# Codex kills a tool call after 60 seconds by default, which is shorter than the wait `ask` and
# `result` accept. Raising it lets a caller block for a whole short consultation.
tool_timeout_sec = 600
```

`--deny <vendor>` keeps the server to the job it exists for. Each registration denies the vendor of
the harness it is registered in, because that harness already spawns same-vendor subagents itself —
natively, in-process, and under its own supervision, which is cheaper and better managed than
anything `agentmux` can start for it. So under Claude Code the tools offer `codex` and nothing else,
and under Codex they offer `claude`.

A denied vendor is left out of the `delegate` choices `ask` and `start` advertise, so a calling
model does not spend a call discovering it; one that names it anyway is refused before any process
starts, and so is a `follow_up` that would resume it. `quota` still reports both vendors — denial
restricts consultations, not usage probes, which may start a CLI but never ask a model.
`--deny claude-code` is accepted as another spelling of `--deny claude`, and denying both vendors is
refused rather than served, since it would leave nothing to consult. The flag is a server flag only:
`agentmux ask` in a terminal still reaches either vendor. Leave it off when the server is how you
reach a second account of your own vendor — it refuses the vendor as a whole, not one account.

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
| `steer`     | Add an instruction to a turn that is still running, read before its next model call |
| `cancel`    | Stop a running consultation, keeping everything collected so far; returns once it has  |
| `list`      | Recent consultations with their ids, so an id can be recovered                       |
| `quota`     | What each configured account has left, in the vendor's own numbers — free, no tokens |

### Use from the command line

The same ten verbs are available directly, as a unified CLI over both vendors:

```bash
# One consultation, start to finish.
agentmux ask --delegate codex --model astra --effort high \
  'Review the diff against main for correctness bugs. Cite file:line.'

# A long review, watched as it arrives.
agentmux start --delegate claude --model opus --effort xhigh --keep \
  --file review/brief.md
agentmux tail <run-id> --follow

# A correction while the turn is still running, from another terminal. Only a Codex turn that
# loads its account's configuration, which is the default, runs over `codex app-server`, which is
# what can take one.
agentmux start --delegate codex --model sol --effort high \
  'Review the diff against main for correctness bugs. Cite file:line.'
agentmux steer <run-id> 'Leave the tests alone; the bug is in src/run.rs.'

# One more question, in the delegate's own session.
agentmux follow-up <run-id> 'Which of those would you fix first, and why?'

# Read-only, but with network, for a question that needs `cargo` or `gh` to answer.
agentmux ask --delegate codex --model sol --effort high --network-access \
  'Does the latest tokio release still support our MSRV?'

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
# Run this account's delegates with its own settings, hooks and MCP servers, whatever the vendor's
# `ignore_user_config` default says; `false` runs them isolated.
inherit_settings = true

# A key kept out of the file, read from the environment at launch.
# A key is billed per token, so the account must say it may be: see "Billing safety".
[accounts.claude.ci]
api_key_env = "CI_ANTHROPIC_KEY"
billing = "api"

# A gateway that authenticates with a bearer token rather than a key (Claude only).
[accounts.claude.gateway]
base_url = "https://llm-gateway.example.com"
auth_token_env = "GATEWAY_TOKEN"
billing = "api"

# A locally served OpenAI-compatible endpoint.
[accounts.codex.local]
base_url = "http://localhost:11434/v1"
api_key = "ollama"
billing = "api"
```

Aliases are opaque, exactly like model identifiers: agentmux keeps no roster, and naming one that
does not exist returns an error listing the ones that do. Omit `account` entirely and the
`[defaults]` alias is used, or the vendor CLI's own configuration when the file names none, which
is right on a machine with one login per vendor and needs no file at all.

Choosing an account **withholds** that vendor's credential variables from the host environment —
and from the `[launch]` layer — so an exported `ANTHROPIC_API_KEY` cannot silently outrank the
account you asked for. That is the whole point: without it a caller believes it switched accounts
while spending another. With no account named, a Codex delegate follows the host's `CODEX_HOME`; a
Claude delegate deliberately does not follow the host's `CLAUDE_CONFIG_DIR`, because under Claude
Code that variable names the launching session's own identity, and runs against the CLI's default
directory instead.

**Two files, two jobs.** A *machine* file — `$AGENTMUX_CONFIG`, `~/.config/agentmux/agentmux.toml`,
`~/agentmux.toml`, or the platform config directory when it lies under your home — may define
accounts, model rewrites, default efforts, the retention window, and what a delegate loads and may
do. `AGENTMUX_CONFIG` must be
absolute and is the one way to keep the file anywhere else; `XDG_CONFIG_HOME` and `APPDATA` count
only when they point under your home, so a container that points them at a workspace cannot turn a
checkout into a machine file. A *project* file, found by walking up from the delegate's working
directory to the root of its repository (the nearest `.git`), or to just below `$HOME` when it is in
none, may only **select** an account:

```toml
# <repo>/agentmux.toml — safe to commit
[defaults.claude]
account = "clientx"
```

A project file that tries to define an account, a `[launch]` table, a `[models]` rewrite, an
`[efforts]` default, a `[blocked]` model, a `retention`, or a `[codex]` or `[claude]` permission
table is refused by name. It has to be, because
such a file arrives with a `git clone`: were it able to name a `base_url` and an `api_key_env`,
cloning a repository and asking for one second opinion would send your key to whoever wrote it. For
the same reason a project file may choose *which* of your accounts pays but nothing more: an account
it selects runs isolated unless the call says otherwise, and does not bring its own `request_env`
with it. On Unix either file is read only when you own it and nobody else can write it: a group
write bit is accepted only when the account databases report you as the group's sole member, as in
the user-private groups behind Ubuntu's default umask `002` (other platforms check only that it is a
regular file), and a file that cannot be parsed is reported by position, never by quoting the line.

**A key agentmux does not know is a warning, not a failure.** One machine file is read by every
agentmux on the machine, including the MCP servers that have been running since before you added
the key; refusing the file would take those servers out entirely over a table they have no use for.
So an unknown key is ignored, named on stderr and listed by `agentmux accounts` as `ignored`, and
the rest of the file takes effect. The same warning is what a misspelling gets — `configdir` for
`config_dir` can leave that account on the CLI's own login, and an account left with nothing
understood is refused — so read the `ignored` lines: the two need
opposite fixes, and only one of them is fixed by upgrading.

### Model rewrites

A caller names the model it wants, and the name it names is often one a person wrote into a prompt
weeks ago — "fable", the family rather than whichever point release is current. The machine file is
where the two are reconciled, so a rule written once outlives every prompt that predates it, and a
family alias on the left is what a caller is told to prefer:

```toml
# ~/.config/agentmux/agentmux.toml

[models.claude]
# Asked for on the left, launched on the right.
"opus" = "claude-opus-5-5[1m]"
"fable" = "claude-fable-5-1"
"sonnet" = "claude-sonnet-5"

[models.codex]
"sol" = "gpt-6.1-sol"
"astra" = "gpt-6-astra"
```

agentmux still keeps no roster of models: an identifier the file says nothing about reaches the CLI
untouched, so a model released this morning works this morning and a rewrite for it is one line you
add when you want it. A rewrite is exactly one substitution — the result is never looked up again,
and a file whose rules would chain is refused when it is read, so what one line says is what runs.
Rules are per vendor, matched exactly as the CLI matches them, and pinned at `start`: a file edited
between two questions cannot answer the second half of one transcript with a different model.

A consultation whose model was rewritten says so wherever it is described — `codex gpt-6-astra …
(asked for astra)` — because a caller that pinned a model and read a different one back would
otherwise have no way to tell a rule of its own machine from a pin agentmux ignored. `agentmux
accounts` lists the rules alongside the accounts. Only a machine file may define one, for the same
reason only a machine file may define an account: a project file arrives with a `git clone`, and
which model answers is a decision about what you pay and what you are told.

### Default efforts

Most callers pin the reasoning effort, and should: a review left to a default is how it silently
runs at the wrong depth. For the ones that leave it out, the machine file names an effort per
model, keyed by the identifier that runs — after any rewrite above — so every consultation of that
model shares it whatever name it asked by:

```toml
# ~/.config/agentmux/agentmux.toml

[efforts.claude]
"claude-fable-5-1" = "high"

[efforts.codex]
"gpt-6-astra" = "medium"
```

A fallback only: an effort the caller names is never overridden, and a model the file says nothing
about runs with no effort passed at all, at the CLI's own default. Both columns are plain strings
that reach the CLI verbatim, like the rewrites, so a vendor's new effort vocabulary is a line in
this file and never an `agentmux` release. A default keyed by a model the same file rewrites away
is refused when the file is read, because it could never apply and would read as though it did.
Every description of a consultation names the effort that ran, and `agentmux accounts` lists the
defaults under `efforts`.

### Blocked models

A model can be too expensive, or retired in favour of a cheaper one, while prompts written weeks ago
still name it. The machine file can block it and say what to ask for instead, either as the
recommendation alone or as a table that also says why:

```toml
# ~/.config/agentmux/agentmux.toml

[blocked.codex]
# Blocked on the left; the model to ask for instead on the right.
"gpt-5.6-sol" = "sol"
"gpt-6-sol" = { use = "sol", reason = "gpt-6.1-sol is much cheaper" }

[blocked.claude]
"claude-opus-5" = "opus"
"claude-fable-5" = "fable"
```

A new consultation whose model is blocked is refused before anything is launched, naming the model
to use instead: `codex model "gpt-6-sol" is blocked by this machine's agentmux configuration; use
model "sol" instead (gpt-6.1-sol is much cheaper). No delegate was launched.` The recommendation is
never launched in the caller's place, because which model answers is the caller's choice to make
knowingly. The model checked is the one that would run, after any rewrite above, so an old
spelling the file rewrites to an allowed model still works. A follow-up is not checked: it keeps the
model its consultation started with, so a block added later does not strand a review that is
already paid for.

A file whose rules contradict each other is refused when it is read: a rewrite onto a blocked model,
a block keyed by a model the same file rewrites away, and a recommendation that is itself blocked.
Nothing else is checked about the recommendation, which reaches the CLI like any other model a
caller names. The MCP server's instructions list the aliases and blocked models it found when it
started, for the vendors it launches, and `quota` lists them again from the current file on every
call, saying so when there are none; `agentmux accounts` lists them under `blocked`. Only a machine
file may block a model.

### Retention

A consultation is kept until its delegate has written nothing for a week, then removed by the sweep
that runs when an MCP server starts or by `agentmux prune`. The clock runs from the delegate's last
write, not from the start, so a long review is not stale the moment it ends. The machine file can
set another window, in `humantime` units (`36h`, `30d`, `1w`, `2w 3d`, `1 week`):

```toml
# ~/.config/agentmux/agentmux.toml
retention = "30d"
```

A value that is not a duration, or is zero, fails the load. `keep: true` on `start` or `ask`
(`--keep` on the command line) exempts one consultation from the sweep altogether, until
`agentmux prune <run_id>` removes it. Neither a kept consultation nor one whose delegate is still
running is ever swept. When the configuration cannot be read, a starting MCP server skips the sweep
rather than falling back to the default window.

### Launching from a sandboxed agent

A delegate runs with whatever confines the process that launched it. Run `agentmux start` or
`agentmux follow-up` from the shell tool of a sandboxed agent, such as Codex in `workspace-write`
mode, and the delegate inherits that sandbox. If the sandbox cannot write the directory where the
delegate CLI saves its sessions, the question is still answered, but the session is lost and every
follow-up fails.

agentmux checks for this every time it launches a turn. When it cannot write there, the turn in the
transcript and every status say the consultation cannot be followed up, and `follow-up` refuses with
the reason instead of paying for a resume that cannot work. Two ways to avoid it:

- **Launch through the MCP server.** A host runs its MCP servers outside its sandbox.
- **Grant write access to the session directory alone:** `~/.claude/projects` for Claude,
  `~/.codex/sessions` for Codex. The rest of each configuration directory holds settings and hooks.

A turn that fails for a reason its CLI printed only to stderr has that reason quoted by `status`.

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

### Billing safety

A delegate runs on the subscription its CLI is logged in with, unless the machine file says
otherwise for the account it runs on. A key or endpoint override found in the environment can move
a CLI onto per-token API billing — which one wins over a stored login depends on the vendor and the
login — so without this a key the launching shell happened to export could quietly turn a
consultation into a paid one.

**`billing = "subscription"` is the default**, and needs no configuration:

- Every API-key and billing-override variable is removed from the delegate's environment at the
  launch itself, whichever layer put it there: the host, `[launch]`, an account's `env`, or a
  call's `env`. That is any name ending in `_API_KEY` or `_API_KEY_FILE`, any `ANTHROPIC_`,
  `CLAUDE_`, `OPENAI_`, `AZURE_OPENAI_` or `CODEX_` name that names a URL (`_BASE_URL`,
  `_URL_OVERRIDE` and the like) or ends in `_ISSUER` or `_AUTH_TOKEN`, and
  `ANTHROPIC_CUSTOM_HEADERS`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`,
  `CLAUDE_CODE_USE_FOUNDRY`, `AWS_BEARER_TOKEN_BEDROCK` and `CODEX_ACCESS_TOKEN`, in any case.
  A subscription token such as `CLAUDE_CODE_OAUTH_TOKEN` is not among them.
- A call whose `env` names one of those is refused, even when `request_env` lists it.
- An account that sets `api_key`, `api_key_env`, `auth_token`, `auth_token_env` or `base_url` is
  refused at launch, naming the field; `agentmux accounts` marks it in advance.
- The quota probes run with the same base environment a delegate starts from and the account's
  configuration directory, always scrubbed of those names, and without the `[launch]` or account
  layers.

**`billing = "api"`** on an account in the machine file lifts exactly those restrictions for that
account, as in the [examples above](#accounts): it permits API pricing, and agentmux cannot
tell whether the CLI then uses the account's key or a login. Every report on such a consultation
says so:
`billing=api` beside the delegate in the header every tool and `agentmux status` print and in
`list`, `"billing": "api"` in `--json`, and a line under the question in the transcript. The key is
still the account's own; a key the host exported is withheld, as it is for any account that was
named.

A call may pin the pricing it expects, with `billing` on `start` and `ask` (`--billing` on the
command line), as it pins a model. `api` is refused unless the account opted in, so it cannot
switch API pricing on; it only makes a consultation that is meant to use the account's key fail
rather than run under the subscription rule. `subscription` keeps even an account that allows API
pricing on its login, and is refused for one that authenticates only with a key. Whatever the
first turn was allowed is pinned for the consultation's follow-ups, however the files change in
between.

A project `agentmux.toml` may select an account that opts in, but the opt-in does not come with it:
the consultation runs under the subscription rule, and an account that names a key is refused.
That file arrives with a clone, so it may say which account runs, never that a key reaches it.

The rule is about the environment, because that is where a key a project needs for its own
backend ends up by accident. What a CLI's own configuration says is left alone: a provider or an
endpoint in `~/.codex/config.toml`, a Codex login made with `codex login --with-api-key`, or a key
in the `settings.json` an inheriting Claude delegate loads is a choice the user made for that CLI.
A Claude delegate names the key source it used when its turn starts, so a turn that did not run
on the subscription says so in the transcript.

**Upgrading from 0.0.8**, which passed keys through: an account that names a key or an endpoint
(`api_key`, `api_key_env`, `auth_token`, `auth_token_env`, `base_url`) needs `billing = "api"`
added to its table in the machine file, or its launches are refused naming the field, and
`agentmux accounts` marks it until then. A key exported in the shell that starts `agentmux`
reaches no delegate any more; to keep using it, give it to an account with `api_key_env` and
`billing = "api"`. Nothing else in the file changes.

### Permissions: what a delegate loads and may do

Every launch passes each CLI's own controls explicitly, so a delegate never takes its sandbox or
permission mode from the user's configuration. The machine file sets what a consultation gets when
the caller names nothing, and the most a caller may ask for:

```toml
# ~/.config/agentmux/agentmux.toml
[codex.defaults]
ignore_user_config = false   # load ~/.codex/config.toml: providers, MCP servers, hooks
sandbox = "read_only"        # or "workspace_write", "danger_full_access"
network_access = true        # unset: Codex's own configuration decides

[codex.limits]
allow_user_config = true
max_sandbox = "workspace_write"
allow_network = true

[claude.defaults]
ignore_user_config = true    # load no settings, hooks or MCP servers
permission_mode = "plan"
tools = ["Bash", "Read", "Grep", "Glob", "WebFetch", "WebSearch"]
disallowed_tools = ["Bash(git push *)"]

[claude.limits]
allowed_permission_modes = ["plan", "default", "acceptEdits", "dontAsk", "auto"]
```

Callers choose within those limits: `ignore_user_config`, `sandbox` and `network_access` for
Codex, and `ignore_user_config`, `permission_mode`, `tools`, `allowed_tools` and
`disallowed_tools` for Claude, as MCP parameters on `start` and `ask` or as `--ignore-user-config`,
`--sandbox`, `--network-access`, `--permission-mode`, `--tools`, `--allowed-tools` and
`--disallowed-tools`. A request beyond a limit is refused before anything is launched, naming the
limit and the file, and never lowered to fit:

```text
Cannot launch codex: sandbox="danger_full_access" exceeds codex.limits.max_sandbox="workspace_write"
in /home/you/.config/agentmux/agentmux.toml. Request sandbox="workspace_write" or "read_only". Only
the owner can raise this limit.
```

With no file, both vendors load the user's configuration, Codex runs read-only with its network
left to Codex's own configuration, and Claude runs in plan mode with the read-only tool list.
`danger_full_access` and `bypassPermissions` can be asked for only when the machine file allows
them, and a default its own limits refuse fails the load. `allow_network = false` also switches the
network off for a consultation that did not choose, so the user's configuration cannot switch it
on. Every limit is the machine file's alone; `agentmux accounts`, `quota` and the MCP server's
instructions list each vendor's defaults and limits.

The transcript header names what a consultation started with, as `sandbox=read-only+network`, and
never changes. `status` and `list` name what its newest turn ran with. A follow-up keeps that
unless it passes the same parameters or flags to change them, and a turn that ran with other
settings than the header names says so under its question. The current limits
are checked again before every turn, so a limit tightened since refuses the next turn until it asks
for less.

**How each choice reaches the CLI.** Codex's sandbox goes as `--sandbox` (as `-c sandbox_mode=…`
on a resume, which rejects `--sandbox`), and a workspace-write delegate's network as
`-c sandbox_workspace_write.network_access=…`. Codex's legacy read-only mode has no network switch,
so read-only *with* network runs under a native permission profile defined and selected with `-c`
on every turn: `extends = ":read-only"` with `network.enabled = true`, under a random name drawn
afresh for each launch so no configuration file can extend it. Every `codex exec` turn also pins
`approval_policy="never"` and `approvals_reviewer="user"`, so an `auto_review` reviewer in your
configuration cannot approve a command out of the sandbox; app-server threads get the same.
That was measured against codex-cli 0.163.0, and agentmux refuses it on a codex older than 0.162.1
when it runs over `codex exec`; a turn driven over `codex app-server` sends the same policy as the
`sandboxPolicy` of every `turn/start` instead. Claude gets `--permission-mode`, `--tools` and, when
chosen, `--allowedTools` and `--disallowedTools`. Codex's sandbox governs the commands the delegate
runs, not the MCP servers or hooks a delegate that loads the user's configuration starts.

**A limit is a ceiling on what agentmux requests, not on what your own Codex rules allow.** An
allow-rule in your `.rules` files, or a trusted project's, runs the commands it matches outside the
sandbox, as in your own sessions. `ignore_user_config: true` does not drop the rules in
`$CODEX_HOME/rules`; it drops a project's only when your configuration was what trusted it.

### Loading the user's configuration, or not

`ignore_user_config` decides whether a delegate loads the user's own CLI configuration: settings,
hooks and MCP servers. Both vendors load it unless the machine file or the call says otherwise,
because a delegate that runs as your own CLI runs with the providers, tools and rules you set up
for it, while what it may *do* is held separately by the permissions above. The older spellings
still work: `inherit_settings: true` on the MCP tools, `--inherit-settings` and `--isolated` on the
command line, and `inherit_settings` on an account, which wins over the vendor's default; a call
that spells the choice twice must agree with itself.

Skipping it is what makes a consultation a second opinion rather than a second copy of your setup.
An isolated delegate — `ignore_user_config: true` — has three guarantees a loading one does not. A
blocking `Stop` hook cannot reopen the finished turn and blank its result; with your configuration
loaded one can, and the transcript marks the reopened turn so the answer above it is still found.
The delegate cannot re-enter `agentmux` through a configured MCP server. And what it saw depends
far less on the machine it ran on, though an isolated Codex delegate still applies your own
exec-policy rules (see [Permissions](#permissions-what-a-delegate-loads-and-may-do)).

```bash
agentmux ask --delegate claude --model opus --effort xhigh \
  --ignore-user-config "Review the diff against main for correctness bugs."

agentmux ask ... --ignore-user-config=false   # load it, even where the machine's default skips it
```

Isolation is imposed by *argument*, not by environment — no variable switches it on or off. An
account a checkout's own `agentmux.toml` selects runs isolated unless the call explicitly asks to
load the configuration.

Every delegate is launched with `AGENTMUX_DELEGATE=1`, and `agentmux` refuses to launch a
delegate — or to serve as an MCP server at all — when it finds itself inside one. An isolated
delegate could never reach `agentmux` anyway; one that loads your configuration loads your own MCP
servers, and `agentmux` may be among them: that server then refuses to start, which the delegate
reports as a server that failed. The variable is not the only witness, because Codex starts its MCP
servers with an environment of its own choosing that does not carry it: `agentmux` also walks its
own ancestry and refuses when any ancestor — however many shells or version-manager shims sit in
between — is a delegate it launched that is still running. That second witness needs both
processes to open the same state directory, which Codex's own allowlist cannot tell the nested
one about; with `AGENTMUX_STATE_DIR` or `XDG_STATE_HOME` set to something other than the
platform default, a Codex delegate that loads an MCP configuration naming `agentmux` is
guarded by the variable alone.

### What the delegate does and does not see

The child environment is built as an **allowlist from empty**, never by filtering a denylist. It
gets `PATH`, `HOME`, locale, proxy and CA settings, and the credential variables its own vendor
reads, less the API keys [Billing safety](#billing-safety) removes — and nothing else. None of the
launching agent's session identity reaches it
([ADR 0022](docs/adr/0022-delegate-env-from-allowlist.md)).

What it loads besides depends on `ignore_user_config`. By default both vendors load the user's own
configuration, so its settings, hooks and MCP servers are part of the run. An isolated Claude
delegate runs with `--setting-sources ""`, so no user, project or local settings load and no hook
can reopen its finished turn, and with `--strict-mcp-config` and an empty MCP config so it cannot
re-enter `agentmux`. An isolated Codex delegate runs with `--ignore-user-config`, which is what
actually detaches the machine's configured MCP servers — `-c mcp_servers={}` looks like it should
and does not. Measured against codex-cli 0.153.4, a delegate launched with that override still
called a configured server's tool, so a machine with `agentmux` in its own Codex config could have
had a delegate call `agentmux` and recurse. It also runs with `features.hooks=false`. It still
loads the exec-policy rules in `$CODEX_HOME/rules`, as described under Permissions.

Either way, a Claude delegate runs in plan mode and is offered no editing tools, and a Codex
delegate runs read-only, unless the consultation's permissions say otherwise.

One thing reaches a Claude delegate even when isolated: `--setting-sources ""` does **not**
suppress `CLAUDE.md` discovery, so a project's own instruction file is part of the delegate's
prompt. `agentmux` says so in the result rather than hiding it.

### Steering a running turn

`steer` adds an instruction to the turn that is running now. The delegate reads it before its next
model call, and the transcript shows it where it arrived, under `### user (steer)`. It is for the
correction you notice ten minutes into a review; a new question, once the turn has ended, is a
`follow_up`.

```bash
agentmux steer <run-id> 'Leave the tests alone; the bug is in src/run.rs.'
```

`status` says `steerable: yes` while a turn can take one. Today that is a **Codex delegate that
loads its account's configuration**, which is the default, because only `codex app-server` accepts
input for a running turn and only such a delegate can run over it:

- `codex exec`, which reads its prompt once, is what an isolated Codex delegate needs: the
  app-server has no equivalent of `--ignore-user-config` (codex-cli 0.159.3), and a private
  `CODEX_HOME` holding only the login cannot stand in for it — a keyring login is filed under the
  directory's path, a copied login's token refresh would log your own codex out, and sessions
  would land where your codex cannot find them. So isolation is never traded for steering
  silently: a consultation with `ignore_user_config: true` runs over `codex exec` and cannot be
  steered. The codex release whose app-server takes the flag lifts this.
- A Claude delegate runs as `claude --print`, which reads its question once and offers no channel
  into a turn that has started.

`steerable: yes` appears once the turn is running; for the first seconds, while the delegate opens
its thread, a steer is refused with a request to send it again shortly.

Everything else is refused with the reason, and a refused message is never in the transcript: a
consultation that is not running or is being cancelled, a delegate launched another way, a message
the server rejected, and one the turn ended before picking up (it is withdrawn unsent). A message
that was sent but never answered — the turn or the server ended first, or no answer came within
30 seconds — is reported as unconfirmed rather than refused, because codex may have taken it; read
the transcript before sending it again.

**How a driven Codex turn runs.** `agentmux` itself, run as a detached child exactly where
`codex exec` would be, starts `codex app-server`, runs the one turn with `approvalPolicy: never`,
writes what the server reports in the event format `codex exec --json` writes, and exits. So
`status`, `tail`, `result` and the files on disk read the same in either mode, and the turn survives
a restart of the `agentmux` that started it. An approval request the server sends anyway is
declined and noted in the transcript. A `follow_up` starts a new turn that resumes the same thread,
and `cancel` asks the server to interrupt the turn before it signals anything. When a turn ends, the
commands it left running are stopped too, within a time budget and on a best-effort basis; on
macOS, a command started by a crashed server's command after the driver's last once-a-second check
is out of reach.
[ADR 0018](docs/adr/0018-codex-app-server-driver.md) explains why it is built this way, and
`crates/agentmux/src/appserver/mod.rs` how the process tree is found on each platform.

`--codex-launch exec`, or `AGENTMUX_CODEX_LAUNCH=exec` for an MCP server, launches every Codex turn
with `codex exec` instead, for a CLI whose app-server cannot run them; nothing is steerable then.
`codex_exec: true` on `start` or `ask` does the same for one consultation, and its follow-ups keep
it. When a driven turn fails, or codex sends something agentmux cannot read, the status says so and
points at these switches: that is what a codex whose app-server protocol has moved looks like.
A running turn keeps the mode it started in.

### Stopping, and what survives a restart

`cancel` marks the turn as cancelling, asks the delegate to stop, waits a few seconds, makes it
stop if it did not, waits a few seconds more, and then records the cancellation. A turn run over
`codex app-server` is first interrupted through the protocol and given up to about twenty seconds to
close and clear up after itself. The record freezes the transcript at what the child had written by
then: whatever the child wrote before is kept, nothing it writes afterwards is read, and a delegate
that had already finished its turn when the signal landed is reported as having finished. A
`status` or `tail` that happens to see the child go first records the same cancellation. The record
does not prove the child left: one that somehow survives both signals is stopped again by the next
`cancel`, and `prune` refuses a running consultation — or a finished or cancelled one whose process
is still there — because deleting its record would leave the child running, billing, and
unreachable.

A delegate that exits without a closing event is recorded once, with its exit status and the tail
of its stderr, and the transcript is rendered from that record from then on — never from files a
tool subprocess the delegate started may still be writing to.

A delegate outlives the `agentmux` that started it. While that process lives it holds the child's
handle; afterwards the pid on disk is trusted only together with the process start time recorded
beside it, so a recycled pid is never mistaken for a delegate. On Windows a restarted `agentmux`
can stop a delegate but its signal reaches only the delegate process: a turn run over
`codex app-server` holds its descendants in a job that ends with the driver, while a directly
launched CLI's tool subprocesses are not contained.

### License

MIT — see [LICENSE](LICENSE).
