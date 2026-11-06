//! Core-owned codebase/Skill retrieval primitives; no persistent User Memory store.
pub(crate) mod embed;
#[cfg(any(feature = "dense", test))]
pub(crate) mod fuse;
pub(crate) mod model_dl;
pub(crate) mod score;
pub(crate) mod tokenize;
