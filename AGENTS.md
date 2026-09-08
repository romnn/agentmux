# Agent guidelines

`agentmux` lets a coding agent running in one vendor's harness (Claude Code, Codex) delegate a
question to the *other* vendor's model by spawning that vendor's CLI. It hands back the **whole
transcript**, never just the final message — the interesting failure modes (a blocking hook that
reopens a finished turn and blanks the result, a warning event that merely looks terminal) live
entirely in the messages a last-message-only API throws away. Same-vendor subagents are already
native to each harness; agentmux exists only for the cross-vendor direction.

## Crate layout

| Crate                 | What it is                                                                    |
| --------------------- | ----------------------------------------------------------------------------- |
| `crates/agentmux`     | library: `config`, `delegate`, `stream/{claude,codex}`, `transcript`, `launch`, `quota`, `run` |
| `crates/agentmux-mcp` | library: the `rmcp` stdio server and its tools                                 |
| `crates/agentmux-cli` | binary `agentmux`: `agentmux mcp` serves, the other subcommands are a unified CLI over both vendors |

## House style

- `indoc!` for every multi-line string literal, prompt scaffolding included; leading indentation
  would otherwise leak into what the delegate reads. A literal whose newlines are written `\n` is
  a multi-line string too — `indoc!` it, and reach for `formatdoc!` where the text interpolates.
- `googletest` for test assertions: `use googletest::prelude::*;` then
  `assert_that!(actual, eq(expected))`. `std::assert_eq!` is banned in `clippy.toml`.
- `thiserror` for library error types. `color-eyre` is for `agentmux-cli`, which installs its
  report hook in `main`; the library crates do not depend on it.
- Test helpers return `googletest::Result` and convert with `.or_fail()?` rather than
  `unwrap`/`expect`. The workspace denies both, and `clippy.toml`'s test exemption covers only
  code inside a test-attributed function — a free helper in a `tests/` file is not exempt.
- **Account aliases are opaque too, and so are quota payloads.** The machine's `agentmux.toml` is
  the only authority on which accounts exist; agentmux keeps no roster and never ranks them. A
  vendor's usage figures are passed through exactly as they arrived, because a percentage means
  nothing without the plan tier behind it and every absolute figure is null on a subscription.
- **A failed quota probe is never zero usage.** `Observation::Unavailable` exists so that an
  account which could not be asked cannot be confused with an idle one; an unauthenticated Claude
  config directory reports 0% used, so anything ranking on "least used" would prefer whichever
  account is broken.
- **Isolation is imposed by argument, never by environment.** `--setting-sources ""`,
  `--strict-mcp-config` and Codex's `--ignore-user-config` are what suppress settings, hooks and
  MCP servers; no environment variable can switch them on or off. `Isolation::Inherit` drops
  exactly those flags and nothing else — plan mode and the read-only tool list are a separate axis.
- **A server denies its own harness's vendor, and the store is what enforces it.**
  `agentmux mcp --deny <vendor>` — `claude-code` is an accepted spelling of `claude` — sets
  `RunStore::with_denied_vendors`, checked beside the recursion refusal in `launch_turn`, which is
  the one place `start` and `follow_up` both pass through: a consultation begun in a terminal is
  not a way to drive a denied vendor from a server. The MCP layer only *advertises* the narrower choice, rewriting the
  `delegate` enum of every tool that requires one, and a schema whose shape has moved leaves the
  list wide rather than hiding a vendor that works — the refusal is the gate, the schema is the
  saved call. `quota` keeps both vendors because reading a usage window launches nothing. Denying
  every vendor is refused at startup, where a host surfaces it, rather than at every call, where
  a host swallows it.
- **Every delegate carries `AGENTMUX_DELEGATE=1`, and agentmux refuses to launch a delegate — or
  to serve MCP at all — when it finds itself inside one.** An isolated delegate cannot reach
  agentmux, but an inheriting one loads the operator's own MCP servers. The marker is not the only
  witness: Codex starts its MCP servers with an allowlist environment that drops it (measured:
  `HOME LANG LC_ALL LOGNAME PATH PWD SHELL TERM TMPDIR USER`, in a fresh process group), so
  `RunStore::running_inside_a_delegate` also walks the process tree (`launch::ancestors`, through
  `sysinfo`) and refuses when any ancestor is a recorded delegate whose start time still matches
  and whose turn nobody has settled. The store names its own resolved root to every delegate as
  `AGENTMUX_STATE_DIR`, so a nested agentmux looks in the same store whatever the platform
  default would have been — except under Codex's MCP-server allowlist, which drops that too, so
  with a non-default state directory the tree witness is blind there (a cost hazard, not an
  identity one; documented, not solved). Decided once per process; a store that cannot be read
  is an error, never "not inside".
- **A request's `env` is an allowlist the operator writes, never a denylist agentmux keeps.** Only
  the names under `request_env` in `agentmux.toml` may be set per call, and by default none: no
  list of names to refuse stays complete against `PATH`, `NODE_OPTIONS`, `LD_PRELOAD` and whatever
  the next runtime reads. An account's own `request_env` counts only when the machine file or the
  caller chose the account, not when a project file did. Every environment name agentmux handles
  passes through `config::canonical_env_name` on the way in (upper-cased on Windows, exact
  elsewhere), so every comparison is a plain one.
- **The rendered transcript is append-only for the life of a consultation, and a turn's fold input
  is frozen once it is terminal.** A terminal event ends the fold. A child observed gone is
  recorded once, together with how much capture existed and the tail of its stderr
  (`ExitRecord::Exited { status, events_len, stderr_tail }`), and every later fold reads the
  record rather than the files, which a descendant of the child may still be writing. `cancel`
  marks the turn as cancelling under the lock, stops the child, waits for it to go, then records
  `ExitRecord::Cancelled { events_len }`; a concurrent reader that sees the child go first records
  the same cancellation. A Codex reply recovered from `last-message.md` is only ever added to the
  newest turn and is then kept in `recovered.md`, so it can neither land above a later turn nor
  go missing once rendered; text that cannot be kept is not rendered. Nothing taken under the
  run lock calls back into anything that takes it — a second lock on the same file from one
  process waits on the first for ever. Whatever record settles a turn, the turn is re-folded
  from exactly the length that record froze, so two observers publish the same bytes. `remove`
  and the sweep refuse a run while any recorded child without an exit record is still alive,
  because both CLIs outlive their closing event. `every_fixture_renders_monotonically_over_its_prefixes`
  checks every fixture at every line cut.
- **A pid on disk is trusted only with the start time recorded beside it.** `Launched::started`
  comes from `sysinfo` at spawn; liveness after a restart requires it to match, and a settled turn
  is never observed again. Per-run state changes — claiming a turn, cancelling, publishing the
  render, removing the run — happen under the advisory lock at `runs/<id>.lock`, which is never
  unlinked.
- **A project file selects an account but cannot switch on its `inherit_settings`.** It arrives
  with a clone; `resolve_isolation` answers `Isolated` for an alias the project file chose unless
  the caller asked explicitly.
- **Only a machine config file may define an account.** A project `agentmux.toml`, found by walking
  up from the delegate's working directory, may only select one. It can arrive with a `git clone`,
  and a file that could name a `base_url` and an `api_key_env` would be a credential-exfiltration
  primitive. Machine files live under the home directory only — `XDG_CONFIG_HOME` and `APPDATA`
  count only when they point there — with `AGENTMUX_CONFIG` as the one deliberate escape hatch, so
  a container that points the config base at a workspace cannot promote a checkout to a machine
  file. A turn directory is a turn once it holds a launch or exit record; one with neither is a
  claim, and a claim found under the lock is abandoned: it is set aside under a name no fold
  walks (a child spawned by the caller that died may still be writing to it) and taken over.
- **Model id and reasoning effort are opaque pass-through strings.** agentmux keeps no roster of
  either, and a new model identifier must never require an agentmux release. The delegate CLI is the
  sole authority on which values it accepts; when one is wrong, that CLI's own error — which names
  the valid set — is surfaced verbatim. agentmux validates only that the strings are argv-safe:
  non-empty, no control characters, bounded length.
- **A model rewrite is the operator's file speaking, not a roster.** `[models.<vendor>]` in a
  machine file maps an identifier a caller asks for to the one launched, which is how "fable-5"
  keeps meaning the current point release without an agentmux release or a code change; an
  identifier the file does not name is untouched. Exactly one substitution is applied and a file
  whose rules would chain is refused at load, because either resolution would be a guess made on
  every launch. `Config::rewritten_model` is the only reader, `RunStore::pin_defaults` the only
  caller: the model is frozen with the account and the isolation at `start`, so a file edited
  mid-consultation cannot answer the second half of one transcript with another model. What was
  asked for is kept in `Meta::rewritten_from` and rendered by `run::describe_delegate`, since a
  caller that pinned a model and read back another must be able to tell its own rule from a pin
  agentmux dropped. Only a machine file may define one — which model answers decides what an
  operator pays and what they are told.
- **A default effort is a fallback keyed by the model that runs, and no effort at all is a valid
  answer.** `Delegate::effort` is an `Option`; `[efforts.<vendor>]` in a machine file fills it in
  `pin_defaults`, after the model rewrite and only when the caller named none, and a model the file
  says nothing about launches with no `--effort` and no `model_reasoning_effort` at all, so the
  CLI's own default runs rather than a value agentmux invented. A default keyed by a model the
  same file rewrites away is refused at load (`ConfigError::EffortForRewrittenModel`): it could
  never match and reads as if it did. Both sides are plain strings, because effort vocabularies
  change with models. The schema no longer requires `effort`, and the record and every summary
  name the effort that ran, or `cli-default`.
- **An unknown configuration key is reported, never refused.** No struct in `config` sets
  `deny_unknown_fields`: one machine file is read by every agentmux on the machine, and the ones
  that matter are the long-running MCP servers started before the key was added — refusing the file
  takes them out of service over a table they have no use for. `Config::unknown_keys` finds them by
  serialising the parsed config back and diffing it against the file, so the answer cannot drift
  from the types; it runs before `canonicalise_env_names`, which on Windows rewrites the keys it
  would be compared against. A key whose value is an empty table or array is passed over, because
  serialising drops an empty `request_env` and the file would be told its own key is unknown. The
  cost is that a misspelled `api_key_env` now silently authenticates as the CLI's own login, which
  is why every reader says the ignored keys out loud: `tracing::warn!` at each load, an `ignored`
  line in `agentmux accounts`, and an `unknown` array in its `--json`.
- The workspace denies `unwrap`/`expect`/`panic`/`indexing_slicing` outside tests, and denies
  `#[allow]`/`#[expect]` without a `reason = "..."`.

## Commands

| Command                | What it does                                                          |
| ---------------------- | --------------------------------------------------------------------- |
| `task run -- <args>`   | run the `agentmux` binary                                             |
| `task check`           | `cargo check` across the workspace                                    |
| `task test`            | nextest across the workspace                                          |
| `task test:fc`         | nextest across every feature combination                              |
| `task test:doc`        | doctests (nextest cannot run them)                                    |
| `task lint`            | clippy, workspace, all features                                       |
| `task lint:fc`         | clippy across every feature combination × target — what CI gates on   |
| `task lint:fix`        | clippy with `--fix`                                                   |
| `task format`          | `cargo fmt --all`                                                     |
| `task spellcheck`      | typos                                                                 |
| `task audit`           | advisories against the dependency tree                                |
| `task unused`          | unused dependencies (nightly)                                         |
| `task lint:actions`    | actionlint over `.github/workflows`                                   |

## Probing accounts

`agentmux quota` reads what each configured account has left, and costs nothing:

- **Claude** keeps its own figures in `$CLAUDE_CONFIG_DIR/.claude.json` under
  `cachedUsageUtilization`. agentmux reads that file, refreshing it with `claude --print /usage`
  when it is over five minutes old — that command reports `total_cost_usd: 0` and no API duration.
  Only `limits[]` carries the per-model windows; the printed text omits some of them.
- **Codex** publishes nothing to disk outside a session rollout, so agentmux asks
  `codex app-server` over JSON-RPC for `account/rateLimits/read`. The server interleaves unsolicited
  notifications with replies, so the response is matched on request id rather than read as the next
  line.
- Codex reports no rate limits on the stream `codex exec --json` produces. They are recovered
  afterwards from `$CODEX_HOME/sessions/**/rollout-*<thread-id>*.jsonl`, and cached per turn in
  `rate-limit.json`, because that search would otherwise run on every `status` and every page of a
  `tail`.

## Testing against the real CLIs

`crates/agentmux/tests/live.rs` drives real `claude` and `codex` processes. Those tests are
`#[ignore]`d, spend money, and need both CLIs logged in; CI passes without them. Run them after
upgrading either CLI:

```bash
cargo nextest run -p agentmux --run-ignored all -E 'test(live)'
```

Everything else runs against `agentmux::testing::ScriptedLauncher` and the recorded streams under
`crates/agentmux/fixtures/`, which were captured from real runs with the exact flags `delegate.rs`
builds. `no_committed_fixture_contains_an_unnamed_event_type` fails the build if a re-recorded
fixture contains an event type the parser does not name, which is how a CLI's format change becomes
loud instead of lossy.

A real capture arrives carrying the recording machine's identity: the `system`/`init` event holds
your home directory, the uid in a temp path, the absolute path of your checkout, the session UUID
encoded into the memory-directory slug, and the list of skills and slash commands you have
installed. None of it is read by either parser. Replace those fields with mocked values before
committing; `no_committed_fixture_carries_a_recording_machine_path` fails the build if you forget.
