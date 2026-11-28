//! What is particular to each delegate CLI, one module per vendor.
//!
//! What every vendor shares — the delegate's header, its identity and environment — belongs to
//! [`crate::delegate`], and the run store's machinery to [`crate::run`].
//! A vendor's module holds what only its CLI decides: what it may do, how its launch is prepared,
//! how its stream folds, what its spend reports cover and where its quota is read.

pub mod agy;
pub mod claude;
pub mod codex;
pub mod pi;
