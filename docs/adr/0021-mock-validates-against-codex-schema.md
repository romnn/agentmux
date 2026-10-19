# 0021. The mock app-server validates messages against codex's own schema, not the generated types

Status: Accepted

## Context

The driver is tested against `codex-app-server-mock` ([0018](0018-codex-app-server-driver.md)),
which builds and reads messages through the same generated protocol types the driver uses
([0019](0019-generated-app-server-protocol.md)). A mistake in those types, or a constraint progeny
could not represent, would then pass both sides unseen: the driver and the mock could agree on the
same wrong message, and every test would be green against a codex that refuses it.

## Decision

The mock holds messages to the vendored JSON Schema bundle the types were generated from, through a
JSON Schema validator independent of the generated types (`codex_app_server_mock::conformance`):

- Requests and notifications a `Peer` sends through a method binding are validated against the
  server's message unions when sent, and a nonconforming one fails the script. `Peer::send`, which
  sends any message, stays unchecked, so a test can still send what codex never would.
- Every message the driver sent in a scripted conversation is validated against the client's
  message unions (`crates/agentmux/tests/appserver.rs`).
- Response fixtures are checked against their named response definitions by the mock's conformance
  tests (`crates/codex-app-server-mock/tests/conformance.rs`), because the schema does not say which
  result answers which request and an answer on the wire is checked only for its envelope.

## Consequences

- A payload the pinned schema would not carry fails a test even when the generated types accept it.
- The checks are only as current as the vendored bundle; agreement between the bundle and the pinned
  codex is the upstream check's job ([0019](0019-generated-app-server-protocol.md)).
