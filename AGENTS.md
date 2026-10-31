# Agent guidelines

`agentmux` delegates questions across Claude Code and Codex and preserves the complete
transcript, including tool activity, warnings and a turn a hook reopened: the whole transcript, not
only the final message, is the deliverable. Its purpose is cross-vendor delegation; each harness
already provides native same-vendor subagents.

## Crate layout

Each crate is under `crates/`; the `agentmux` library's modules are `accounting`, `config`,
`delegate`, `launch`, `left_running`, `run`, `stream`, `transcript`, `quota` and `appserver`.

| Crate                       | What it is                                                       |
| --------------------------- | ---------------------------------------------------------------- |
| `agentmux`                  | library: delegates, the run store, stream folding, quota, driver |
| `agentmux-mcp`              | library: the `rmcp` stdio server and its tools                   |
| `agentmux-cli`              | binary `agentmux`: `agentmux mcp` serves, the rest is a CLI      |
| `codex-app-server-protocol` | library: the `codex app-server` protocol, mostly generated       |
| `codex-app-server-mock`     | test support: scriptable `Peer`, `fixtures`, mock `codex` binary |
| `codex-app-server-codegen`  | tool: writes the protocol crate's `src/generated/`               |

## House style

- `indoc!` for every multi-line string literal, prompt scaffolding included; leading indentation
  would otherwise leak into what the delegate reads. A literal whose newlines are written `\n` is a
  multi-line string too — `indoc!` it, and reach for `formatdoc!` where the text interpolates.
- `googletest` for test assertions: `use googletest::prelude::*;` then
  `assert_that!(actual, eq(expected))`. `std::assert_eq!` is banned in `clippy.toml`.
- `thiserror` for library error types. `color-eyre` is for `agentmux-cli`, which installs its report
  hook in `main`; the library crates do not depend on it.
- Test helpers return `googletest::Result` and convert with `.or_fail()?` rather than
  `unwrap`/`expect`. `clippy.toml`'s test exemption covers only code inside a test-attributed
  function — a free helper in a `tests/` file is not exempt.
- The workspace denies `unwrap`/`expect`/`panic`/`indexing_slicing` outside tests, and denies
  `#[allow]`/`#[expect]` without a `reason = "..."`.
- Never hand-edit `crates/codex-app-server-protocol/src/generated/`: change the generator and
  regenerate (skill `codex-protocol-codegen`).

## Commands

| Command                   | What it does                                                        |
| ------------------------- | ------------------------------------------------------------------- |
| `task run -- <args>`      | run the `agentmux` binary                                           |
| `task check`              | `cargo check` across the workspace                                  |
| `task test`               | nextest across the workspace                                        |
| `task test:fc`            | nextest across every feature combination                            |
| `task test:doc`           | doctests (nextest cannot run them)                                  |
| `task lint`               | clippy, workspace, all features                                     |
| `task lint:fc`            | clippy across every feature combination × target — what CI gates on |
| `task lint:fix`           | clippy with `--fix`                                                 |
| `task format`             | `cargo fmt --all`                                                   |
| `task spellcheck`         | typos                                                               |
| `task audit`              | advisories against the dependency tree                              |
| `task unused`             | unused dependencies (nightly)                                       |
| `task lint:actions`       | actionlint over `.github/workflows`                                 |
| `task codegen:app-server` | regenerate the protocol crate; its other tasks are in the skill     |

## Skills

Project skills live in `.agents/skills/` (`.claude/skills` links there). Load one before the task it
names:

- [`real-cli-testing`](.agents/skills/real-cli-testing/SKILL.md) — running the live tests against
  real `claude`/`codex`, re-recording or adding fixtures and scrubbing a capture, or choosing
  between live, mock app-server and fixture tests.
- [`codex-protocol-codegen`](.agents/skills/codex-protocol-codegen/SKILL.md) — regenerating the
  app-server protocol, binding another method, running the codegen checks and the oracle, or
  moving the codex pin.

## Architecture decisions

Design decisions and their rationale are indexed in [docs/adr/README.md](docs/adr/README.md). Read
the relevant records before changing an area, and update affected records and references in the
same change.
