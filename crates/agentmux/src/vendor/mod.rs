//! What is particular to each delegate CLI, one module per vendor.
//!
//! What every vendor shares — the delegate's header, its identity and environment — belongs to
//! [`crate::delegate`], and the run store's machinery to [`crate::run`]; a vendor's module holds
//! the decisions only its CLI makes: what it may do, and the arguments it runs with.

pub mod agy;
pub mod claude;
pub mod codex;
