//! Source/CLI boundary for the external persistent-Memory extraction.
//! This inspects only the checkout, never legacy data or the operator's HOME.

use crate::cli_args::Cli;
use clap::Parser;
use std::path::Path;

#[test]
fn persistent_memory_engine_sources_are_not_shipped_in_core() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for removed in [
        "src/memory",
        "src/bench/brain.rs",
        "src/bench/metrics.rs",
        ".github/workflows/dense-bench.yml",
    ] {
        assert!(
            !root.join(removed).exists(),
            "dormant persistent-Memory implementation returned: {removed}"
        );
    }

    let main = include_str!("../main.rs");
    let builtin_memory_module =
        regex::Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+memory\s*[;{]").unwrap();
    assert!(
        !builtin_memory_module.is_match(main),
        "Core must not wire a built-in persistent Memory engine"
    );
}

#[test]
fn core_identity_session_and_retrieval_owners_remain_present() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for retained in [
        "src/agent/mod.rs",
        "src/skills/mod.rs",
        "src/persona/mod.rs",
        "src/persona/self_mem.rs",
        "src/persona/soul.rs",
        "src/core/session_mem.rs",
        "src/core/session_store.rs",
        "src/core/memory_context.rs",
        "src/retrieval/mod.rs",
        "src/plugin/mod.rs",
        "src/tests/legacy_memory.rs",
    ] {
        assert!(
            root.join(retained).is_file(),
            "Core owner missing: {retained}"
        );
    }
    // Pin actual compiled ownership too, not only names of files in the checkout.
    let session = crate::core::session_mem::SessionMem::new();
    assert!(session.is_empty());
    assert_eq!(
        crate::retrieval::tokenize::tokenize("Core retrieval").len(),
        2
    );
}

#[test]
fn removed_memory_benchmarks_are_not_core_commands() {
    for removed in ["memory", "profile", "dialectic"] {
        assert!(
            Cli::try_parse_from(["aizen", "bench", removed]).is_err(),
            "persistent Memory benchmark must remain provider-owned: {removed}"
        );
    }
    assert!(Cli::try_parse_from(["aizen", "memory", "status", "--json"]).is_ok());
    assert!(Cli::try_parse_from(["aizen", "bench", "loop"]).is_ok());
}
