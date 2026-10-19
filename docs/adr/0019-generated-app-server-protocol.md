# 0019. The app-server protocol is generated from codex's own schema

Status: Accepted

## Context

The driver ([0018](0018-codex-app-server-driver.md)) speaks codex's app-server protocol, which codex
changes between releases. Hand-written payload types drift silently, a method name typed as a string
matches nothing when it is renamed, and a newer codex sends variants an older agentmux has never
seen. The schema codex publishes names every method and its params, but never says which type
answers a request.

## Decision

### Generation

We generate a serde-only protocol crate rather than depend on codex's protocol crate, whose large
dependency graph, workspace patches and lockfile requirements belong only in the separate oracle.
`codex-app-server-protocol`'s `src/generated/` — `schema.rs` (progeny's types) and `methods.rs` — is
written by `crates/codex-app-server-codegen` from codex's JSON Schema
(`crates/codex-app-server-codegen/schema/codex_app_server_protocol.schemas.json`), whose four
message unions name every method and its params. The generated files are never edited by hand.

Every method is bound by name, but only the methods the generator's `surface!` table
(`crates/codex-app-server-codegen/src/surface.rs`) lists — what agentmux sends, answers or decodes —
are bound to types, and only their payloads are generated: a method agentmux never reads costs
nothing and cannot be decoded. The table is also the one place that says which type answers a
request, which the schema never does:
`"turn/start" = TurnStart(TurnStartParams) => TurnStartResponse`, with codex's own variant name.

### Two checks keep the table honest

Generation checks the method and its params specification against the stable schema, requires
uniqueness for bindings with params, and checks that response definitions exist. The oracle
(`crates/codex-app-server-codegen/oracle`) checks the named upstream variants' params and response
types by expanding the same table into a match on codex's own message enums at the pinned release;
nothing reads codex's source. The link between a wire method and its Rust variant relies on params
uniqueness; experimental variants, Rust aliases, and parameterless methods limit that guarantee.

The oracle is a workspace of its own, because codex's crate is several hundred packages with native
dependencies that only the oracle should build, and it resolves only with codex's
`[patch.crates-io]` forks and lockfile pins.

### Every message goes through its method binding

Method bindings fix message direction, params and response types at compile time. Incoming methods
are classified before their payloads are decoded, so a receiver matches variants, not strings, and
decodes only what it acts on; the protocol crate's documentation describes the traits.
`appserver::protocol::decline_response` matches `AnyServerRequest` with no wildcard, so a
regeneration that adds a server request fails to build until it is decided.

### Lenient reading

Open enums and tagged unions accept unknown variants, so a newer codex does not fail the message
around one. For notifications it processes, the translator preserves decode failures and unknown
values that prevent translation as drift with the original payload. Ignored methods and fields are
not inspected for drift.

### The pin

The codex pin is the oracle's git tag, as its `Cargo.lock` resolves it
(`codex-app-server-codegen pin` prints it). The oracle's codex crates must resolve to one tag and
commit, or the pin is refused (`Error::ConflictingCodexPins`), because the method table and the
`exec` mirror would otherwise be checked against different releases.

`codex-app-server-codegen refresh` refuses a codex other than the pin, and one whose app-server
accepts `--ignore-user-config` (`Error::AppServerIsolationAvailable`). We deliberately block refresh
so an upgrade must revisit isolated steering rather than leave it as a reminder. This can block a
protocol-only upgrade until launch support is implemented or the gate policy is deliberately
revised. Launch eligibility remains governed by [0018](0018-codex-app-server-driver.md). Moving the
pin is deliberate; the steps are in the
[`codex-protocol-codegen`](../../.agents/skills/codex-protocol-codegen/SKILL.md) skill.

### Three hand-written parts

- **The JSON-RPC envelope** (`envelope`): codex leaves out the `"jsonrpc"` member, so a message is
  classified by which of `id` and `method` it carries, and a payload is decoded separately, so drift
  is recorded rather than the message lost.
- **`rollout`**, the usage windows codex records in a session rollout: it reads only a token count's
  snapshot, every other line and event reading as `Other`. The oracle converts codex's snapshot into
  it with every field of codex's named, and reads codex's own rollout lines back as it.
- **`exec`**, the events `codex exec --json` writes, for which codex publishes no schema: it mirrors
  codex's `exec_events.rs` type for type. The oracle uses exhaustive upstream matches and complete
  struct patterns and literals to check the mirrored shapes, and its tests compare representative
  upstream and mirror values as JSON.

The mirror writes what codex writes and reads more than codex reads, because a capture holds lines a
newer codex wrote and lines an older agentmux wrote, and the render of a finished consultation must
not change ([0009](0009-append-only-transcript.md)):

- an `Unknown` variant names a type, status or kind it was not built for;
- every field a reader takes that codex requires is an `Option`, so absence and `null` read as they
  always did;
- every field of a narration item, which nothing reads, has a default.

`a_capture_an_older_agentmux_wrote_folds_as_it_did` (`crates/agentmux/tests/capture.rs`) holds the
old shapes.

agentmux's own side — which settings a thread runs under, how a request is declined — stays in
`crates/agentmux/src/appserver/protocol.rs`.

## Consequences

- Change generated bindings or payloads through the generator or its inputs, then run
  `task codegen:app-server`. Edit the hand-written envelope and mirrors directly.
- Schema generation, the upstream export and oracle agreement are each checked in CI; the commands
  are in the skill's [Commands](../../.agents/skills/codex-protocol-codegen/SKILL.md#commands)
  section.
- The mock app-server checks its messages against the schema independently of these types
  ([0021](0021-mock-validates-against-codex-schema.md)).
