---
name: codex-protocol-codegen
description:
    Regenerate crates/codex-app-server-protocol from the vendored codex schema, change what the
    generator binds (its surface! table), refresh the vendored schema, check the protocol against
    codex's own crates with the oracle, or move the pinned codex release. Use when a file under
    crates/codex-app-server-protocol/src/generated/ needs to change, when editing
    crates/codex-app-server-codegen, when the driver needs an app-server method it does not bind
    yet, when a codegen check fails in CI, or when upgrading codex.
---

# Regenerating the app-server protocol and moving the codex pin

The design — why the protocol is generated, what the `surface!` table and the oracle guarantee, and
which parts are hand-written — is ADR
[0019](../../../docs/adr/0019-generated-app-server-protocol.md). This skill holds the steps.

## Never edit the generated files

`crates/codex-app-server-protocol/src/generated/` (`schema.rs` and `methods.rs`) is written by
`crates/codex-app-server-codegen`. Change the generator, then regenerate. The hand-written parts of
the protocol crate — `envelope.rs`, `exec.rs`, `rollout.rs` and `lib.rs` — are edited normally.

## Commands

- `task codegen:app-server` regenerates the protocol crate from the vendored schema; it needs
  neither codex nor the network.
- `task codegen:app-server:check` runs the generator's tests, which fail when the protocol crate is
  not what the vendored schema generates.
- `task codegen:app-server:refresh` re-exports the vendored schema from the pinned codex on `PATH`,
  then regenerates.
- `task codegen:app-server:upstream` fails when the vendored schema is not what the pinned codex on
  `PATH` exports.
- `task codegen:app-server:oracle` checks the answers table and the `exec` and `rollout` mirrors
  against codex's own crates at the pinned tag. It builds codex's crates in a workspace of its own,
  so the first run is slow.

The `codegen` workflow (`.github/workflows/codegen.yaml`) runs `check`, `upstream` and `oracle`.
`cargo run -p codex-app-server-codegen -- pin` prints the pinned codex release.

## Binding another method

1. Add the method to the `surface!` table in `crates/codex-app-server-codegen/src/surface.rs`, in
   the section of its message union (`client_requests`, `client_notifications`, `server_requests`,
   `server_notifications`), with codex's own variant name, its params and, for a request, the type
   that answers it: `"turn/start" = TurnStart(TurnStartParams) => TurnStartResponse`. Write
   `Params?` for params that may be left out, and no parentheses for a method without params.
2. Run `task codegen:app-server`. It refuses an entry whose method or params the schema does not
   list, whose response type it does not define, or whose params are not unique: bindings with
   params must have a unique params specification within that stable-schema union.
3. Run `task codegen:app-server:check`, then `task codegen:app-server:oracle`. See ADR
   [0019](../../../docs/adr/0019-generated-app-server-protocol.md) for what the two checks prove and
   the limits of tying wire methods to upstream Rust variants.

When a schema refresh adds a server-request method, update the exhaustive `decline_response` match
in `crates/agentmux/src/appserver/protocol.rs` to decide how agentmux handles it.

## Moving the codex pin

The pin is the oracle's git tag, as `crates/codex-app-server-codegen/oracle/Cargo.lock` resolves it.
Moving it is deliberate:

1. Set the new `rust-v<version>` tag on every codex dependency in
   `crates/codex-app-server-codegen/oracle/Cargo.toml`. All of codex's crates must resolve to one
   tag and commit, or the pin is refused.
2. Copy the forks that `codex-rs/Cargo.toml` patches at that tag, the ones this graph uses, into the
   oracle's `[patch.crates-io]`.
3. Seed `oracle/Cargo.lock` from `codex-rs/Cargo.lock` at that tag, because the oracle resolves only
   with codex's lockfile pins. The oracle task runs `--locked`, so a tag moved in the manifest alone
   fails there.
4. Resolve the oracle once without `--locked`, preserving the seeded dependency pins:

   ```bash
   cargo metadata --manifest-path crates/codex-app-server-codegen/oracle/Cargo.toml \
     --format-version 1 > /dev/null
   ```

   This updates the lockfile for the oracle's dependency graph and records codex's git sources,
   which the seeded lockfile lists as workspace members without a source. Verify that
   `cargo run --quiet -p codex-app-server-codegen -- pin` prints the intended release before
   refreshing.
5. Install that codex on `PATH` (CI installs `@openai/codex@<version>` with npm).
6. Run `task codegen:app-server:refresh`, then fix schema, binding and mirror incompatibilities.
   Regenerate with `task codegen:app-server` after generator or surface-table changes.
7. Finish with `task codegen:app-server:check`, `task codegen:app-server:upstream`, and
   `task codegen:app-server:oracle`.

`codex-app-server-codegen refresh` refuses a codex other than the pin, and one whose app-server
accepts `--ignore-user-config`. Treat the second refusal as a decision to revisit, not something to
work around: ADR [0019](../../../docs/adr/0019-generated-app-server-protocol.md) explains the gate,
and `Delegate::app_server_settings` with ADR
[0018](../../../docs/adr/0018-codex-app-server-driver.md) governs which delegates are driven.
