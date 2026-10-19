---
name: real-cli-testing
description:
    Run agentmux's live tests against the real claude and codex CLIs, re-record or add stream
    fixtures under crates/agentmux/fixtures, scrub the recording machine's identity from a capture,
    or decide which test harness a change needs (live, codex-app-server-mock, ScriptedLauncher). Use
    after upgrading either CLI, when a CLI's output format may have changed, when a fixture test
    fails, or when adding a fixture or an app-server driver test.
---

# Testing against the real CLIs

agentmux parses vendor JSON it does not control. Live tests run against the real CLIs and spend
money; tests of the app-server driver run against `codex-app-server-mock`; everything else runs
against recorded fixtures.

## Choosing a harness

| What needs verification               | Harness                                  |
| -------------------------------------- | ---------------------------------------- |
| Stream folding and rendering           | Recorded fixtures                        |
| Run-store behavior                     | `agentmux::testing::ScriptedLauncher`    |
| Driver protocol ordering and deadlines | A scripted `Peer`                        |
| Driver process launch and teardown     | The CLI against the mock `codex` binary  |
| Compatibility with the installed CLIs  | Live tests                               |

## Live tests

Run live tests where the delegates can write their session stores, normally from an unsandboxed
terminal; see ADR [0016](../../../docs/adr/0016-unsaved-session-store.md). Both CLIs must be logged
in, and the tests spend money; they are `#[ignore]`d, and CI passes without them. Use the operator's
chosen accounts: the library suite uses the configured default accounts, and
`AGENTMUX_LIVE_CODEX_ACCOUNT` selects an account only for the CLI steering test.

`crates/agentmux/tests/live.rs` drives real `claude` and `codex` processes. Run it after upgrading
either CLI:

```bash
cargo nextest run -p agentmux --run-ignored all -E 'test(live)'
```

The library helper retains its store after startup, waiting, and status retrieval succeed,
including when later assertions fail. Earlier errors may remove it.

A steered turn over `codex app-server` has a live test of its own in the crate that builds the
driver, `crates/agentmux-cli/tests/live.rs`, which runs the `agentmux` built with the test as the
delegate:

```bash
AGENTMUX_LIVE_CODEX_ACCOUNT=<chosen-account> \
  cargo nextest run -p agentmux-cli --run-ignored all -E 'test(live)' --no-capture
```

`AGENTMUX_LIVE_CODEX_ACCOUNT` names an account from the machine's `agentmux.toml`; without it the
file's default, or the CLI's own login, is used. The CLI steering test retains its directory before
launch and prints the transcript's path.

When a capture reads differently than expected, trace it to what wrote it: each turn's
`invocation.json` records what the delegate's CLI printed for `--version` (`Launcher::version`,
asked before the launch with the child's environment) and the agentmux release that launched it.

## Mock app-server tests

- `crates/agentmux/tests/appserver.rs` scripts the server's side of each conversation with a `Peer`
  over real pipes — what it expects, what it answers, in order — with payloads built by the mock's
  `fixtures` from the generated types. It shortens the driver's `DriverTimeouts` to reach what a
  deadline ends. Each wait requests one line; a timed-out wait leaves that read pending. To simulate
  a server that stops reading, finish any pending read and keep the script blocked without another
  receive. Returning successfully starts the harness's drain loop.
- `crates/agentmux-cli/tests/steer.rs` runs the real `agentmux` as the driver against the mock's
  `codex` binary, which plays a `scenario` as a process. The test builds that binary with `escargot`
  (nothing else builds another package's binary for a test) and configures it through
  `app-server-mock.json` in the turn's codex home, because the driver's environment is an allowlist.

Both check messages against codex's own schema; see ADR
[0021](../../../docs/adr/0021-mock-validates-against-codex-schema.md).

## Fixture tests

The recorded streams live under `crates/agentmux/fixtures/{claude,codex}/`, registered in
`agentmux::testing::fixtures`. They were captured from real runs with the exact flags
`crates/agentmux/src/delegate.rs` builds. `crates/agentmux/tests/capture.rs` checks them:
`no_committed_fixture_contains_an_unnamed_event_type` fails if a fixture contains an event type the
parser does not name, which is how a CLI's format change becomes loud instead of lossy, and
`every_fixture_renders_monotonically_over_its_prefixes` renders every fixture at every line cut.

When a live test fails, first determine whether the failure reflects changed vendor output.
Authentication failures, session-store failures and agentmux regressions do not justify changing
recorded expectations. If the vendor's output did change:

1. Record the relevant behavior. A real consultation is such a recording: a turn's `events.jsonl`
   (under `runs/<id>/turns/<NNNN>/` in the store a live test keeps) is the CLI's stdout verbatim,
   launched with the argv recorded in that turn's `invocation.json`. For Codex, record an `exec`
   turn; a turn driven over `codex app-server` holds the driver's translation, not codex's own
   output.
2. Scrub the capture (below).
3. Add or replace the fixture, and register a new one in `agentmux::testing::fixtures`: its
   constants and `fixtures::all()`.
4. Update the parser as needed.
5. Run `cargo nextest run -p agentmux -E 'binary(capture)'` to check folding, rendering, event
   coverage, and the path guard.

## Scrubbing a capture before committing it

A real capture arrives carrying the recording machine's identity. Claude's `system`/`init` event
contains several recording-machine details:

- your home directory;
- the uid in a temp path;
- the absolute path of your checkout;
- the session UUID encoded into the memory-directory slug;
- the list of skills and slash commands you have installed.

Inspect the entire capture for machine identity, including tool arguments and output; Codex captures
need inspection too. None of it is read by either parser. Replace those fields with mocked values
before committing, following the committed fixtures: `/work/project` for the checkout, `/home/dev`
for the home directory, `-work-project` for the memory slug, `/tmp/socket` for a socket path, and an
empty or stock list of skills and slash commands.

`no_committed_fixture_carries_a_recording_machine_path` rejects several known path markers when the
capture tests run. It does not replace manual scrubbing.
