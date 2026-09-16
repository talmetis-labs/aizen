//! Argument parsing.

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The name to greet: the first argument that does not start with `--`, else `world`.
    pub name: String,
}

impl Config {
    pub fn from_args(args: &[&str]) -> Config {
        let name = args
            .iter()
            .find(|a| !a.starts_with("--"))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "world".to_string());
        Config { name }
    }
}
