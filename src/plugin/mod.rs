//! Optional native Memory extensions. No persistent Memory engine is linked here.

pub(crate) mod adoption;
pub(crate) mod associations;
pub(crate) mod client;
pub mod configuration;
pub(crate) mod context;
pub(crate) mod events;
pub mod install;
pub mod manager;
pub mod manifest;
pub(crate) mod manual;
pub mod memory_api;
mod outbox;
pub(crate) mod releases;
pub(crate) mod runtime;
pub mod settings;
pub(crate) mod snapshots;
pub(crate) mod storage;
pub(crate) mod tool_support;
pub(crate) mod tools;
pub mod transport;
pub(crate) mod update;
pub(crate) mod update_check;
