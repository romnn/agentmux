# 35. An isolated agy delegate runs in a home of its own

**Status:** accepted · **Date:** 2026-11-27

## Context

Isolation is imposed by argument ([0005](0005-isolation-by-argument.md)): `--setting-sources ""`
with an empty strict MCP configuration for Claude, `--ignore-user-config` for Codex.
agy, Google's Antigravity CLI, has no such argument.
Probes of agy 1.5.18 established what it has instead:

- It always loads its user settings, `$HOME/.gemini/antigravity-cli/settings.json`.
  `--settings <json>` overrides keys for one session and never saves them, and refuses a key it
  does not know before any model call.
- `--settings` replaces a whole top-level key rather than merging into it: an overlay naming only
  `permissions.deny` dropped the deny rules of the user's own file.
- It loads hooks and MCP servers from two places, and runs them headless: the user's
  `$HOME/.gemini/config/{hooks,mcp_config}.json` and the workspace's `.agents/hooks.json` and
  `.agents/mcp_config.json`, where the workspace is the repository the working directory is in, or
  that directory outside one.
  Its documentation adds plugins, under `$HOME/.gemini/config/plugins/` and `.agents/plugins/`,
  which can bundle both.
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
`Delegate::invocation` sets `HOME`, and `USERPROFILE` where agy looks on Windows, after every other
environment layer, so no account or request can move it, and creates the home on first use.
agentmux writes nothing into it: the pinned settings travel in `--settings` on every launch, as for
an inheriting delegate.
Its conversations live there, so a follow-up resumes them and the session-store check
([0016](0016-unsaved-session-store.md)) reads them through the same environment.
Isolation resolves for agy as for the other vendors: an explicit request, then an account's
`inherit_settings`, then a project-selected account, which runs isolated, then the machine file's
`[agy.defaults]` and `[agy.limits]`, and otherwise loading the user's configuration.

**A workspace that would still be loaded refuses an isolated launch.**
A private home cannot drop what agy reads from the workspace, and nothing turns it off, so an
isolated launch whose working directory, or any directory above it up to the repository root,
holds `.agents/hooks.json`, `.agents/mcp_config.json` or `.agents/plugins` is refused with
`DelegateError::AgyWorkspaceConfig`, which names the file.
An inheriting delegate runs with them: loading the user's configuration is what it asked for, as
for Claude's and Codex's.

**What it may do is pinned by argument in both modes.**
`agy_args` passes `--settings` on every launch, with `agentMode: ""`, `toolPermission:
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

- An isolated agy delegate has isolation's three guarantees as far as agy's own configuration
  goes: no hook of the user's runs, no MCP server of the user's loads, and its answer does not
  depend on the user's settings.
  The `mcp(*)` deny rule refuses every MCP call besides, and the delegate marker
  ([0007](0007-recursion-guard.md)) still stops an agentmux it reaches from consulting again.
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
