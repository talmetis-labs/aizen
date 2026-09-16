//! The greeting.

use crate::config::Config;

/// Greet whoever the arguments name.
pub fn run(args: &[&str]) -> String {
    let cfg = Config::from_args(args);
    format!("hello, {}", cfg.name)
}
