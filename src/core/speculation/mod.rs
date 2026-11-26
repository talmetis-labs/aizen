//! Historical speculative evidence inspection and bounded nonterminal observations.
//! No source capture, overlay, custom merger or integration runtime remains in this module.

pub(crate) mod evidence;
pub(crate) mod inspection;
pub(crate) mod legacy;
pub(crate) mod namespace;
pub(crate) mod recovery;
pub(crate) mod restart;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod verification;
pub(crate) mod workers;
