# 0026. A delegate's permissions are the CLI's own controls, bounded by machine-file limits

Status: Accepted

## Context

A delegate that cannot reach the network cannot answer a question about a crate's latest release,
a GitHub issue, or a dependency's documentation, and one that skips the user's own configuration
runs stricter than the user's own CLI: without the providers, MCP servers and rules the owner set
up for it. A caller can loosen a launch, but it is usually an agent that cannot see what the owner
is comfortable with, so it needs to be told what it may ask for and to be refused, legibly, when it
asks for more. And some modes — Codex's `danger-full-access`, Claude's `bypassPermissions` — must be
the owner's to allow, never a caller's.

Both CLIs already offer the controls: Codex's sandbox modes, `[sandbox_workspace_write]
network_access` and its native permission profiles; Claude's `--permission-mode`, `--tools`,
`--allowedTools` and `--disallowedTools`. A permission model of agentmux's own would have to be
translated into those and would drift from them.

## Decision

**The controls are the CLIs' own.** `delegate::CodexPermissions` carries Codex's sandbox mode
(`CodexSandbox`) and `network_access`; `delegate::ClaudePermissions` carries Claude's permission
mode (`ClaudePermissionMode`) and its three tool lists. Whether the user's configuration loads is
the existing `Isolation` choice, spelled `ignore_user_config` in configuration, on the MCP tools and
on the command line; `inherit_settings`, `--inherit-settings` and `--isolated` remain its inverse
spellings, and a request that spells it twice inconsistently is refused
(`Isolation::from_spellings`).

**Defaults and limits are per vendor and machine-only.** `[codex.defaults]`, `[codex.limits]`,
`[claude.defaults]` and `[claude.limits]` (`config::CodexPolicy`, `config::ClaudePolicy`) say what a
consultation gets when the caller names nothing and the most it may ask for. A project file that
sets any of them is refused ([0011](0011-machine-files-define-project-files-select.md)), an unknown
key in them is reported ([0015](0015-unknown-config-keys-reported.md)), and a default its own file's
limits would refuse — including an account's `inherit_settings = true` under
`allow_user_config = false` — fails the load (`ConfigError::DefaultBeyondLimit`). With no file:
both vendors load the user's configuration, Codex runs read-only with its network left to Codex,
Claude runs in plan mode with the read-only tool list, the sandbox ceiling is `workspace_write`, and
every Claude mode but `bypassPermissions` is allowed.

**Resolution is fixed at `start`.** The isolation is the caller's, else the checkout-selected
account's isolation, else the account's own `inherit_settings`, else the vendor default, else
loading the configuration unless `allow_user_config = false`
(`delegate::resolve_isolation`). The permissions are the caller's, else the vendor defaults, else
the built-in ones (`Delegate::with_default_permissions`). What results is recorded in `Meta` and
shown in every delegate line, as `sandbox=read-only+network`.

**A limit refuses; it never clamps.** `Delegate::held_to_limits` is the one check, run by
`RunStore::start` before a run directory exists, by `RunStore::follow_up_with` before the turn is
claimed, and by `Delegate::invocation` for every launch. A request beyond a limit is refused with
`config::PermissionRefusal`, whose message names the limit, where it was set and what to ask for
instead. The one value the check fills in is a network nobody chose under `allow_network = false`,
which becomes an explicit `false` so the user's configuration cannot switch it on.

**Follow-ups start from the newest turn, and the limits are current.** Each turn records the
delegate it ran as in its own `invocation.json` (`InvocationRecord::delegate`), written before the
turn's supervisor starts it ([0025](0025-eager-settlement-and-terminal-waiting.md)); the
supervisor runs exactly the argument vector recorded there (`SupervisorPlan`), so the permission
flags, the approval pins and the profile named for that launch are what the CLI gets. A follow-up
is admitted, resolved and claimed under one hold of the run's lock: the newest turn published and
settled, its delegate gone, then its record read (`RunStore::current_delegate`) and the overrides
applied (`Delegate::with_options`), so no concurrent follow-up can launch from an older record.
`Meta` and the transcript header rendered from it never change: the transcript stays append-only
([0009](0009-append-only-transcript.md)), so a turn that ran with other settings says so under its
own question, and what a hook reopening a turn means is judged by that turn's own isolation
(`HookReopening::of_turns`). `status`, `list` and a steer refusal describe the newest turn's
isolation and permissions, and `wait` those of the turn it returns (`RunStore::turn_delegate`); only those are taken from a turn's record, while who is consulted —
vendor, model, effort, account — stays `Meta`'s (`Delegate::with_settings_of`). The current
limits are applied before every turn, so a limit tightened after a consultation began refuses its
next turn. That differs deliberately from blocked models
([0024](0024-blocked-models.md)), which exempt a running consultation: a model block is about cost,
which the consultation has already chosen to pay, while a permission limit is about what may happen
on the owner's machine from now on. A record written before these choices were kept follows up as
it ran: isolated, read-only, plan mode, read-only tools.

**Translation, per launch.** `codex exec` gets `--sandbox` on a new session and
`-c sandbox_mode=…` on a resume, plus `-c sandbox_workspace_write.network_access=…` for a
workspace-write delegate whose network was chosen. Read-only *with* network has no legacy spelling:
it runs under a permission profile defined and selected with `-c` on every turn
(`permissions.<name>={extends=":read-only",network={enabled=true}}`,
`default_permissions="<name>"`), with no legacy sandbox flag beside it, because `--sandbox` forces
the legacy modes. The name is random and fresh for every launch, resumes included, because codex
merges configuration tables recursively across layers: a same-named profile in the user's or a
trusted project's configuration would otherwise add its own entries, such as a filesystem write
grant. Every `exec` turn also gets `-c approval_policy="never"` and `-c approvals_reviewer="user"`:
`exec` defaults to `never`, but codex 0.162.1 drops that default when the loaded configuration
names the `auto_review` reviewer, which would let an automatic reviewer approve commands outside
the sandbox. That path is gated on the codex release (`codex_supports_permission_profiles`,
0.162.1 and later) and refused, never widened, on any other. Over `codex app-server` the thread
starts and resumes with the sandbox mode, `approvalPolicy: never` and
`approvalsReviewer: user`, a workspace-write network
choice is a `-c` on the server's command line, and read-only with network is the `sandboxPolicy` of
every `turn/start` ([0018](0018-codex-app-server-driver.md)). Claude gets `--permission-mode` and
`--tools` on every launch and `--allowedTools` and `--disallowedTools` when chosen.

**A limit bounds what agentmux asks for, not what the user's own configuration grants.** Codex
`.rules` allow-rules from the user's configuration or a trusted project are honoured as in the
user's own sessions: codex runs a command every segment of which an allow-rule matches outside the
sandbox, under approval policy `never` too (`Decision::Allow` → `Skip { bypass_sandbox: true }` in
codex 0.162.1). agentmux passes no `--ignore-rules`. Isolating does not change that for the user's
own rules: `--ignore-user-config` keeps an empty user layer whose folder is still `$CODEX_HOME`, so
`$CODEX_HOME/rules/*.rules` still load; only a project's rules drop out when the project was
trusted by the user's configuration alone.

## Consequences

- Every launch passes its sandbox or permission mode explicitly, so a delegate that loads the
  user's configuration never takes them from it.
- Loading the user's configuration is the default, so the guarantees isolation buys
  ([0005](0005-isolation-by-argument.md)) hold only for isolated consultations: a hook can reopen a
  finished turn, which the transcript marks as expected, and a configured agentmux is reached and
  refuses ([0007](0007-recursion-guard.md)).
- Codex's sandbox governs the delegate's commands; it does not constrain the MCP servers and hooks
  a delegate that loads the configuration starts.
- Defaults and limits are published where model rules are: in the MCP server's instructions, in
  `quota`, and in `agentmux accounts` (`Config::describe_permissions`).
- Changing `ignore_user_config` on a follow-up can move a Codex consultation between `exec` and the
  app-server driver, and so change whether it can be steered.
- Claude's native `sandbox` settings block is not exposed: it is the user's own settings, which a
  delegate that loads them applies.
