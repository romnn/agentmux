//! Delegate a question to another vendor's coding agent and capture the whole transcript.
//!
//! A coding agent running in one vendor's harness can already spawn same-vendor subagents
//! natively.
//! Reaching the *other* vendor means spawning its CLI, and that spawn has half a dozen flags that
//! must all be right or the answer is silently lost — the process still exits zero and still
//! prints something plausible.
//! agentmux turns those flags into types, so a caller fills in a schema instead of composing a
//! command line, and gets back a durable, addressable transcript.
//!
//! # The shape
//!
//! - [`delegate`] — the closed set of delegates, and the exact argument vector each one needs.
//! - [`stream`] — folds each vendor's event stream into a transcript.
//!   Nothing else parses JSON.
//! - [`transcript`] — the vendor-neutral record of what a delegate said.
//! - [`launch`] — turns a prepared invocation into a detached child writing to files.
//! - [`run`] — run directories, ids, lifecycle, retention and resume state.
//! - [`testing`] — a scripted launcher and the recorded vendor fixtures, so the whole pipeline
//!   runs with no credentials and no network.
//!
//! The dependency direction is one way: `transcript` and `delegate` import nothing heavy;
//! `stream` imports `transcript`; `run` imports `launch`, `stream` and `delegate`.
//! Nothing here knows about MCP, which is what makes the whole pipeline testable without a client.
//!
//! # Example
//!
//! ```no_run
//! # // Uses `no_run` because the example spawns a real delegate CLI and writes a run directory.
//! use std::sync::Arc;
//!
//! use agentmux::delegate::{Delegate, Effort, ModelId};
//! use agentmux::launch::ProcessLauncher;
//! use agentmux::run::{Retention, RunStore, StartRequest};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let env = agentmux::host_env();
//! let store = RunStore::open(RunStore::default_root(&env)?, Arc::new(ProcessLauncher), env)?;
//!
//! let status = store.start(&StartRequest {
//!     delegate: Delegate::Claude {
//!         model: ModelId::parse("claude-opus-5")?,
//!         effort: Effort::parse("xhigh")?,
//!         account: None,
//!     },
//!     question: "Review the merge-base diff for correctness bugs.".to_owned(),
//!     cwd: std::env::current_dir()?,
//!     retention: Retention::Ttl,
//!     env: Default::default(),
//! })?;
//!
//! println!("consultation {} started", status.run_id);
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod delegate;
pub mod launch;
pub mod quota;
pub mod run;
pub mod stream;
pub mod testing;
pub mod transcript;

use std::collections::BTreeMap;

/// The environment agentmux itself was launched with.
///
/// Captured once and threaded through, rather than read from the process at each use, so a test
/// can supply a hostile environment and assert that none of it reaches a child.
#[must_use]
pub fn host_env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}
