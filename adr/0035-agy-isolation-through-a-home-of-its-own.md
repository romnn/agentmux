# 35. An isolated agy delegate runs in a home of its own

**Status:** accepted · **Date:** 2026-11-27

## Context

Isolation is imposed by argument ([0005](0005-isolation-by-argument.md)): `--setting-sources ""`
with an empty strict MCP configuration for Claude, `--ignore-user-config` for Codex.
[0036](0036-isolation-imposed-by-the-launch-agentmux-builds.md) has since made the rule the launch
agentmux builds, which this record's home is an instance of.
agy, Google's Antigravity CLI, has no such argument.
Probes of agy 1.5.18 established what it has instead:

- It always loads its user settings, `$HOME/.gemini/antigravity-cli/settings.json`.
  `--settings <json>` overrides keys for one session and never saves them, and refuses a key it
  does not know before any model call.
- `--settings` replaces a whole top-level key rather than merging into it: an overlay naming only
  `permissions.deny` dropped the deny rules of the user's own file.
- It loads hooks, MCP servers and plugins from two places, and runs them headless: the user's
  `$HOME/.gemini/config/`, and the project's `.agents/` — its `hooks.json`, `mcp_config.json`,
  `plugins/` and the plugins `plugins.json` registers.
  It finds the project's in every directory it can read: the working directory's repository, and
  equally a directory granted with `--add-dir`, with or without a repository, while running in an
  empty directory of its own.
  No setting turns hooks or MCP servers off: `--settings` refused every candidate key as unknown,
  and the documentation disables them only one by one.
- It honours `HOME`, and has no other variable for its configuration: `GEMINI_HOME` and
  `ANTIGRAVITY_CONFIG_REL_DIR` move nothing.
  Its login is in the operating system's keyring, not in its home.
  Under a fresh absolute `HOME` a turn authenticated, stored its conversation in that home, resumed
  it with `--conversation`, and `/usage` answered; the user's hooks and MCP servers did not run.
- agy rewrites the settings file in the home it runs under, so a file is not a place to pin a
  setting.
- Headless agy with no `agentMode` runs in `auto` where its terminal sandbox is available, which
  approves workspace edits, and `--mode plan` writes a plan file, so neither mode is read-only.

## Decision

**An isolated agy delegate runs with `HOME` set to a home agentmux keeps for it.**
The run store keeps one, `agy-home` beside `runs`, for every isolated agy delegate: agy has one
login, so one home serves it.
agentmux owns that directory: it creates it, and nothing but agy is meant to write there, so a home
restored over it or linked to another profile is the user's own doing.
The store hands that home to the launch it builds, which sets `HOME`, and `USERPROFILE` where agy
looks on Windows, over every other environment layer, so no account or request can move it.
The public `Delegate::invocation` refuses an isolated agy delegate with
`DelegateError::AgyHomeUnknown`, since built anywhere else it would run in the user's own home.
agentmux writes nothing into it: the pinned settings travel in `--settings` on every launch, as for
an inheriting delegate.
Its conversations live there, so a follow-up resumes them and the session-store check
([0016](0016-unsaved-session-store.md)) reads them through the same environment.
A follow-up that would change an agy consultation's isolation, and so its home, is refused before
its turn is claimed, because the conversation it resumes is kept in the other one.
Isolation resolves for agy as for the other vendors: an explicit request, then an account's
`inherit_settings`, then a project-selected account, which runs isolated, then the machine file's
`[agy.defaults]` and `[agy.limits]`, and otherwise loading the user's configuration.

**The project's own tooling runs in both modes.**
agy runs the project's `.agents` hooks, MCP servers and plugins whether it is isolated or not: it
finds them in any directory it can read, and nothing turns them off.
Running it elsewhere does not help, since the project it must read is then such a directory, and
predicting which files it will load proved a moving target, so agentmux does neither.
Isolation therefore keeps out the user's own configuration only, which is narrower than Claude's:
an agy delegate is not to be pointed at a checkout the user does not trust.

**What it may do is pinned by argument in both modes.**
`vendor::agy::agy_args` passes `--settings` on every launch, with `agentMode: ""`, `toolPermission:
"request-review"`, deny rules for commands, file writes, MCP calls, browser actions and downloads
(`AGY_DENY_RULES`), and `useG1Credits: false`, which stops a model whose quota is used up from
going on at the price of the account's AI credits.
Because `--settings` replaces `permissions`, the launch reads the `deny` and `ask` rules of the
settings file in the home it runs in and passes them on beside its own; `allow` rules are left out,
since they only widen what a delegate may do.
In its own home an isolated delegate has none to carry over.
A settings file that exists but cannot be parsed refuses the launch, rather than dropping rules it
may hold; a leading byte-order mark, which agy itself reads past, is not a parse failure.
A live probe with a permissive user file — `accept-edits`, `always-proceed`, an allow rule for the
command and the write — confirmed that this overlay refuses both and keeps `request-review`.

**One login, so at most one account.**
An `[accounts.agy.<alias>]` table can only label the keyring login: there is at most one, it holds
only a `description` and `inherit_settings`, and it must be the `[defaults.agy]` account, so
`quota` reports the login once (`Config::validate_agy_accounts`).

## Consequences

- An isolated agy delegate runs none of the user's hooks or MCP servers and none of their
  settings, and spends no AI credits, but it runs the project's own tooling, which an isolated
  Claude delegate does not.
  The `mcp(*)` deny rule refuses every MCP call the model makes, though not an MCP server starting,
  and the delegate marker ([0007](0007-recursion-guard.md)) still stops an agentmux it reaches from
  consulting again.
- Under inherit, agy's own sign-in decides what is billed: agentmux cannot see what the login agy
  saved is priced at, and records the turn as a subscription's.
- Conversations of isolated consultations stay in the store's `agy-home` for as long as the store
  does; pruning a run does not delete them, as it does not delete a Claude or Codex session.
- The quota probe runs `agy --print /usage` in a fresh temporary home, because agy starts the MCP
  servers of the home it runs in even to answer that command.
- `useG1Credits: false` could not be observed while quota remained: `/usage` and `agy models` read
  the same either way.
  It is the documented setting, and `--settings` accepts it as a boolean.
- On macOS the keyring is the login keychain, which does not follow `HOME`; that a private home
  still reaches the login there is untested.
- The user's rules are read at every launch, so an edit to them applies from the next turn.
