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
  would otherwise leak into what the delegate reads.
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
- **Every delegate carries `AGENTMUX_DELEGATE=1`, and agentmux refuses to launch a delegate when it sees it.** An
  isolated delegate cannot reach agentmux, but an inheriting one loads the operator's own MCP
  servers; the marker is what makes the opt-out safe rather than a recursion waiting to happen.
- **Only a machine config file may define an account.** A project `agentmux.toml`, found by walking
  up from the delegate's working directory, may only select one. It can arrive with a `git clone`,
  and a file that could name a `base_url` and an `api_key_env` would be a credential-exfiltration
  primitive.
- **Model id and reasoning effort are opaque pass-through strings.** agentmux keeps no roster of
  either, and a new model identifier must never require an agentmux release. The delegate CLI is the
  sole authority on which values it accepts; when one is wrong, that CLI's own error — which names
  the valid set — is surfaced verbatim. agentmux validates only that the strings are argv-safe:
  non-empty, no control characters, bounded length.
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
