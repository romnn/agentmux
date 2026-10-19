# 0014. A configuration file is parsed into its types as it is read

Status: Accepted

## Context

Configuration values end up in argv and in the environment of a child process. A string that was
accepted at load and re-parsed later could fail, or be read differently, far from the file that held
it. One machine file is also synced across machines that run different agentmux releases.

## Decision

Vendor, account-alias and model keys, and model and effort values, deserialize directly into their
validated types — `Vendor`, `AccountAlias`, `ModelId` and `Effort` (`crates/agentmux/src/config.rs`)
— so nothing downstream re-parses a string. An identifier no argument parser would accept is a
`ConfigError::Malformed` pointing at its byte offset; the error never quotes the offending line,
which is as likely as not to hold a key.

A vendor this build does not know is passed over by the `config::by_vendor` deserializer and
reported as an unknown key ([0015](0015-unknown-config-keys-reported.md)), because one file is
synced across machines running different agentmux releases.

## Consequences

- Identifier syntax errors fail during deserialization with a parser position. Semantic
  configuration checks, such as an environment name the operating system would misread, use
  dedicated errors; runtime requirements such as a configuration directory's existence remain
  launch-time checks.
- A file naming a vendor added in a newer agentmux still loads on an older one.
