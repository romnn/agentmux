# 35. An agy delegate always loads the user's settings, and an isolated one is refused

**Status:** accepted · **Date:** 2026-11-27

## Context

Isolation is imposed by argument ([0005](0005-isolation-by-argument.md)): `--setting-sources ""`
with an empty strict MCP configuration for Claude, `--ignore-user-config` for Codex.
agy, Google's Antigravity CLI, has no such argument.
Probes of agy 1.5.18 established what it has instead:

- It always loads its user settings, `~/.gemini/antigravity-cli/settings.json`.
  `--settings <json>` overrides keys for one session and never saves them.
- `--settings` replaces a whole top-level key rather than merging into it: an overlay naming only
  `permissions.deny` dropped the deny rules of the user's own file.
- Neither `GEMINI_HOME` nor `ANTIGRAVITY_CONFIG_REL_DIR` moves its configuration, and the binary
  does not read the first at all.
  Its login is in the operating system's keyring, not in a directory.
- It honours `HOME`: pointed at another absolute directory, it creates its configuration there and
  still reaches the keyring login.
- Headless agy with no `agentMode` runs in `auto` where its terminal sandbox is available, which
  approves workspace edits, and `--mode plan` writes a plan file, so neither mode is read-only.

## Decision

**An agy delegate runs as the user's own agy, and an isolated one is refused.**
`Delegate::held_to_limits` refuses an agy delegate whose resolved isolation is
`Isolation::Isolated` with `PermissionRefusal::IsolationUnavailable`, at `start`, before every
follow-up and at every launch.
Running it anyway under `ignore_user_config: true` would claim guarantees the run does not have.
The machine file has no `[agy]` table and an agy account no `inherit_settings`, so the isolation
resolves to `Inherit` unless a call explicitly asks for `true`.

**What it may do is pinned by argument all the same.**
`agy_args` passes `--settings` on every launch, with `agentMode: ""`, `toolPermission:
"request-review"` and deny rules for commands, file writes, MCP calls, browser actions and
downloads (`AGY_DENY_RULES`).
Because `--settings` replaces the user's `permissions`, the launch reads the `deny` and `ask` rules
of the user's settings file and passes them on beside its own; the user's `allow` rules are left
out, since they only widen what a delegate may do.
A settings file that exists but cannot be parsed refuses the launch, rather than dropping rules it
may hold.
A live probe with a permissive user file — `accept-edits`, `always-proceed`, an allow rule for the
command and the write — confirmed that this overlay refuses both and keeps `request-review`.

**One login, so at most one account.**
An `[accounts.agy.<alias>]` table can only label the keyring login: there is at most one, it holds
only a `description`, and it must be the `[defaults.agy]` account, so `quota` reports the login
once (`Config::validate_agy_accounts`).

## Consequences

- None of isolation's three guarantees holds for an agy delegate: a hook in its settings runs, its
  configured MCP servers are loaded, and its answer depends on this machine.
  The `mcp(*)` deny rule refuses every MCP call, and the delegate marker
  ([0007](0007-recursion-guard.md)) still stops an agentmux it reaches from consulting again.
  Its delegate line says `settings=inherited`.
- The user's rules are read at every launch, so an edit to them applies from the next turn.
- A private `HOME` holding only agy's settings could isolate it later, since agy honours `HOME` and
  its login is outside it.
  That would also move its conversations, which a follow-up resumes and the session-store check
  ([0016](0016-unsaved-session-store.md)) reads, so it is a decision of its own.
