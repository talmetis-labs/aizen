//! A two-module crate: `config` parses the arguments, `app` acts on them. The task threads a new
//! flag from one to the other.

pub mod app;
pub mod config;

pub use app::run;
pub use config::Config;
