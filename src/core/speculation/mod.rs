//! Controller-owned native snapshots, private edits and deterministic integration.
//!
//! Durable records are evidence, never execution capabilities. Only sealed in-process handles
//! authorize overlay IO; only the coordinating parent can materialize a checked integration.

pub(crate) mod integration;
pub(crate) mod lifecycle;
pub(crate) mod merge;
pub(crate) mod recovery;
pub(crate) mod store;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod verification;
pub(crate) mod workers;
