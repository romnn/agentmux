//! A scriptable stand-in for `codex app-server`, for testing a client of its protocol.
//!
//! - [`Peer`] is the server's end of one conversation, over any reader and writer, which a test
//!   drives step by step: expect a request, answer it or not, notify, ask the client something,
//!   or send what codex never would; [`Peer::open_thread`] and [`Peer::start_turn`] take the
//!   steps every conversation with codex takes.
//! - [`fixtures`] builds the payloads codex sends from the generated protocol types, and
//!   [`conformance`] holds messages to codex's own schema, so a script cannot send a payload the
//!   pinned protocol would not carry without saying so.
//! - The crate's `codex` binary runs a [`Peer`] as a process standing in for the `codex`
//!   executable, for a test of a client that spawns codex itself; [`scenario`] is what it plays.
//!
//! A script is ordinary Rust on a thread of the test's own; ordering between it and the client
//! is the test's to arrange, with channels or by waiting for the client's next message, rather
//! than with sleeps.

pub mod conformance;
mod conversation;
pub mod fixtures;
mod peer;
pub mod scenario;

pub use conversation::{Opened, OpenedThread};
pub use peer::{DEFAULT_TIMEOUT, Peer, PeerError, Received, Sent};
