use super::*;

fn edit<'a>(author: &'a str, base: &'a [u8], result: &'a [u8]) -> AuthoredChange<'a> {
    AuthoredChange { author, base: Some(base), result, operation: Operation::Localized }
}

fn run(changes: &[AuthoredChange<'_>]) -> Result<Candidate, Conflict> {
    merge(Language::Rust, changes, &Limits::default())
}

fn replaced(bytes: &[u8], old: &str, new: &str) -> Vec<u8> {
    let text = std::str::from_utf8(bytes).unwrap();
    assert_eq!(text.matches(old).count(), 1);
    text.replacen(old, new, 1).into_bytes()
}

const BASE: &[u8] = b"fn alpha() -> u32 {\n    1\n}\nfn beta() -> u32 {\n    2\n}\nfn gamma() -> u32 {\n    3\n}\n";

#[test]
fn distinct_function_bodies_merge_in_stable_author_order() {
    let a = replaced(BASE, "    1", "    7");
    let b = replaced(BASE, "    2", "    8");
    let candidate = run(&[edit("z", BASE, &a), edit("a", BASE, &b)]).unwrap();
    assert_eq!(candidate.bytes, replaced(&a, "    2", "    8"));
    assert_eq!(candidate.contributors, ["a", "z"]);
}

#[test]
fn repeated_digits_use_atomic_rust_bodies_not_arbitrary_character_alignment() {
    for base in [
        b"pub fn alpha() -> i32 {\n    10\n}\n\npub fn beta() -> i32 {\n    20\n}\n".as_slice(),
        b"fn alpha() -> i32 {\n    101\n}\nfn beta() -> i32 {\n    202\n}\n".as_slice(),
    ] {
        let (old_a, new_a, old_b, new_b) = if base.windows(3).any(|w| w == b"101") {
            ("101", "111", "202", "222")
        } else {
            ("10", "11", "20", "21")
        };
        let a = replaced(base, old_a, new_a);
        let b = replaced(base, old_b, new_b);
        let expected = replaced(&a, old_b, new_b);
        assert_eq!(run(&[edit("solo", base, &a)]).unwrap().bytes, a);
        for changes in [[edit("a", base, &a), edit("b", base, &b)],
                        [edit("b", base, &b), edit("a", base, &a)]] {
            assert_eq!(run(&changes).unwrap().bytes, expected);
        }
        let competing = replaced(base, old_a, "999");
        assert!(matches!(run(&[edit("a", base, &a), edit("b", base, &competing)])
            .unwrap_err().kind, ConflictKind::Overlap | ConflictKind::SameFunction));
    }
}

#[test]
fn structural_ambiguity_fallback_cannot_merge_signatures_macros_or_bypass_limits() {
    let base = b"fn alpha() -> i32 {\n    10\n}\nfn beta() -> i32 {\n    20\n}\n";
    let a = replaced(base, "10", "11");
    let b = replaced(base, "20", "21");
    let changed_signature = replaced(&a, "alpha() -> i32", "alpha() -> u32");
    assert!(run(&[edit("a", base, &changed_signature), edit("b", base, &b)]).is_err());
    let limits = Limits { max_hunks: 1, ..Limits::default() };
    assert!(matches!(merge(Language::Rust, &[edit("a", base, &a), edit("b", base, &b)], &limits)
        .unwrap_err().kind, ConflictKind::LimitExceeded(LimitKind::Hunks)));
    let opaque = b"fn alpha() { echo!(10); }\nfn beta() { 20 }\n";
    let a = replaced(opaque, "10", "11");
    let b = replaced(opaque, "20", "21");
    assert!(run(&[edit("a", opaque, &a), edit("b", opaque, &b)]).is_err());
}

#[test]
fn permutations_produce_identical_candidate_and_conflict() {
    let a = replaced(BASE, "    1", "    7");
    let b = replaced(BASE, "    2", "    8");
    let c = replaced(BASE, "    3", "    9");
    let changes = [edit("charlie", BASE, &a), edit("alice", BASE, &b), edit("bob", BASE, &c)];
    let expected = run(&changes);
    assert!(expected.is_ok());
    for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        assert_eq!(run(&order.map(|i| changes[i])), expected);
    }
    let other = replaced(BASE, "    1", "    6");
    let bad = [edit("charlie", BASE, &a), edit("alice", BASE, &other), edit("bob", BASE, &c)];
    let expected = run(&bad);
    assert!(expected.is_err());
    for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        assert_eq!(run(&order.map(|i| bad[i])), expected);
    }
}

#[test]
fn unicode_crlf_and_missing_final_newline_are_byte_preserved() {
    let base = "fn alpha() -> &'static str {\r\n    \"雪\"\r\n}\r\nfn beta() -> &'static str {\r\n    \"猫\"\r\n}".as_bytes();
    let a = replaced(base, "雪", "海");
    let b = replaced(base, "猫", "犬");
    let merged = run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap();
    assert_eq!(merged.bytes, replaced(&a, "猫", "犬"));
    assert!(!merged.bytes.ends_with(b"\n"));
    assert_eq!(merged.bytes.iter().filter(|b| **b == b'\r').count(), 5);
}

#[test]
fn utf8_hunk_boundaries_never_split_scalars() {
    let base = "abc雪xyz".as_bytes();
    let result = "abc海xyz".as_bytes();
    let mut budget = Budget::new(Limits::default());
    let hunks = diff(base, result, &mut budget).unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].base, 3..6);
    assert_eq!(hunks[0].result, 3..6);
    assert_eq!(run(&[edit("a", base, result)]).unwrap().bytes, result);
}

#[test]
fn identical_results_deduplicate_contributors_even_for_unsupported_language() {
    let changes = [edit("z", b"a", b"b"), edit("a", b"a", b"b"), edit("z", b"a", b"b")];
    let merged = merge(Language::Other, &changes, &Limits::default()).unwrap();
    assert_eq!(merged.bytes, b"b");
    assert_eq!(merged.contributors, ["a", "z"]);
}

#[test]
fn identical_shared_hunk_is_deduplicated_with_all_authors() {
    let a = replaced(BASE, "    1", "    7");
    let b = replaced(&a, "    2", "    8");
    let candidate = run(&[edit("a", BASE, &a), edit("b", BASE, &b)]).unwrap();
    assert_eq!(candidate.bytes, b);
    assert_eq!(candidate.contributors, ["a", "b"]);
}

#[test]
fn shared_edit_does_not_hide_another_change_in_the_same_function() {
    let base = b"fn alpha() { let x = 1; let y = 2; }\nfn beta() {}\n";
    let a = replaced(base, "= 1", "= 7");
    let b = replaced(&a, "= 2", "= 8");
    let err = run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err();
    assert_eq!(err.kind, ConflictKind::SameFunction);
}

#[test]
fn noops_do_not_claim_contribution_or_cause_rewrite_conflict() {
    let a = replaced(BASE, "    1", "    7");
    let mut noop = edit("noop", BASE, BASE);
    noop.operation = Operation::Rewrite;
    let merged = run(&[noop, edit("real", BASE, &a)]).unwrap();
    assert_eq!(merged.contributors, ["real"]);
    let unchanged = run(&[noop]).unwrap();
    assert_eq!(unchanged.bytes, BASE);
    assert!(unchanged.contributors.is_empty());
}

#[test]
fn absent_and_present_empty_bases_are_not_interchangeable() {
    let creation = AuthoredChange { author: "a", base: None, result: b"", operation: Operation::Localized };
    assert_eq!(run(&[creation]).unwrap().contributors, ["a"]);
    assert_eq!(run(&[creation, edit("b", b"", b"")]).unwrap_err().kind, ConflictKind::BaseMismatch);
    assert!(run(&[edit("a", b"", b"")]).unwrap().contributors.is_empty());
}

#[test]
fn identical_creations_coalesce_but_different_creations_conflict() {
    let a = AuthoredChange { author: "a", base: None, result: b"", operation: Operation::Localized };
    let b = AuthoredChange { author: "b", ..a };
    assert_eq!(run(&[b, a]).unwrap(), Candidate { bytes: vec![], contributors: vec!["a".into(), "b".into()] });
    let c = AuthoredChange { result: b"fn alpha() {}", ..b };
    assert_eq!(run(&[a, c]).unwrap_err().kind, ConflictKind::Creation);
}

#[test]
fn base_mismatch_and_author_collision_fail_closed() {
    assert_eq!(run(&[edit("a", b"x", b"a"), edit("b", b"y", b"b")]).unwrap_err().kind, ConflictKind::BaseMismatch);
    assert_eq!(run(&[edit("same", b"x", b"a"), edit("same", b"x", b"b")]).unwrap_err().kind, ConflictKind::AuthorCollision);
}

#[test]
fn invalid_utf8_or_invalid_author_is_rejected_without_echoing_the_id() {
    assert_eq!(run(&[edit("a", b"ok", &[255])]).unwrap_err().kind, ConflictKind::InvalidUtf8);
    assert_eq!(run(&[edit("a", &[255], b"ok")]).unwrap_err().kind, ConflictKind::InvalidUtf8);
    for author in ["", "bad\nid", &"x".repeat(129)] {
        let err = run(&[edit(author, b"", b"")]).unwrap_err();
        assert_eq!(err.kind, ConflictKind::InvalidAuthor);
        assert!(err.authors.is_empty());
    }
    assert_eq!(run(&[]).unwrap_err().kind, ConflictKind::EmptyInput);
}

#[test]
fn rewrite_conflicts_with_other_effective_results_but_identical_rewrites_deduplicate() {
    let a = replaced(BASE, "    1", "    7");
    let b = replaced(BASE, "    2", "    8");
    let mut rewrite = edit("a", BASE, &a);
    rewrite.operation = Operation::Rewrite;
    assert_eq!(run(&[rewrite, edit("b", BASE, &b)]).unwrap_err().kind, ConflictKind::WholeRewrite);
    assert_eq!(run(&[rewrite, edit("b", BASE, &a)]).unwrap().contributors, ["a", "b"]);
    assert_eq!(run(&[rewrite]).unwrap().bytes, a);
    let all = edit("a", BASE, b"Z");
    assert_eq!(run(&[all, edit("b", BASE, &b)]).unwrap_err().kind, ConflictKind::WholeRewrite);
}

#[test]
fn unsupported_language_does_not_merge_nonidentical_edits() {
    let a = replaced(BASE, "    1", "    7");
    let b = replaced(BASE, "    2", "    8");
    assert_eq!(merge(Language::Other, &[edit("a", BASE, &a), edit("b", BASE, &b)], &Limits::default()).unwrap_err().kind, ConflictKind::UnsupportedLanguage);
}

#[test]
fn same_function_conflicts_even_when_text_ranges_are_disjoint() {
    let base = b"fn alpha() {\n    let x = 1;\n    let y = 2;\n}\n";
    let a = replaced(base, "= 1", "= 7");
    let b = replaced(base, "= 2", "= 8");
    assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::SameFunction);
}

#[test]
fn competing_same_offset_insertions_and_replacements_conflict() {
    let base = b"fn alpha() {\n    1\n}\nfn beta() {}\n";
    let a = replaced(base, "    1", "    91");
    let b = replaced(base, "    1", "    81");
    assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::CompetingInsertion);
    let a = replaced(base, "    1", "    7");
    let b = replaced(base, "    1", "    8");
    assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::Overlap);
}

#[test]
fn insertion_on_replacement_boundary_is_conservatively_an_overlap() {
    assert!(touches(&(2..2), &(2..4)));
    assert!(touches(&(4..4), &(2..4)));
    assert!(!touches(&(1..2), &(2..3)));
    assert!(!touches(&(1..1), &(2..3)));
}

#[test]
fn imports_signatures_item_creation_and_macros_do_not_merge() {
    let base = b"use std::fmt;\nfn alpha() -> u32 {\n    1\n}\nfn beta() -> u32 {\n    2\n}\n";
    let b = replaced(base, "    2", "    8");
    for a in [
        replaced(base, "std::fmt", "std::mem"),
        replaced(base, "alpha() -> u32", "alpha() -> u64"),
        [base.as_slice(), b"fn gamma() {}\n"].concat(),
    ] {
        assert!(run(&[edit("a", base, &a), edit("b", base, &b)]).is_err());
    }
    let base = b"fn alpha() { echo!(1); }\nfn beta() { 2 }\n";
    let a = replaced(base, "!(1)", "!(7)");
    let b = replaced(base, "{ 2 }", "{ 8 }");
    assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::MacroOrAttribute);
}

#[test]
fn attributes_on_functions_or_containers_prevent_merging_their_bodies() {
    for base in [
        b"#[some_attribute]\nfn alpha() { 1 }\nfn beta() { 2 }\n".as_slice(),
        b"#[some_attribute]\nmod inner { fn alpha() { 1 } }\nfn beta() { 2 }\n".as_slice(),
    ] {
        let a = replaced(base, "{ 1 }", "{ 7 }");
        let b = replaced(base, "{ 2 }", "{ 8 }");
        assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::MacroOrAttribute);
    }
}

#[test]
fn nested_functions_are_not_independent_merge_slots() {
    let base = b"fn outer() { fn alpha() { 1 } fn beta() { 2 } }\n";
    let a = replaced(base, "{ 1 }", "{ 7 }");
    let b = replaced(base, "{ 2 }", "{ 8 }");
    assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::AmbiguousRust);
}

#[test]
fn modules_impls_and_trait_default_methods_are_scanned() {
    let base = b"mod child { pub fn alpha() { 1 } }\nstruct Example;\nimpl Example { fn beta(&self) { 2 } }\ntrait Trait { fn declaration(); fn gamma() { 3 } }\n";
    let a = replaced(base, "{ 1 }", "{ 7 }");
    let b = replaced(base, "{ 2 }", "{ 8 }");
    let c = replaced(base, "{ 3 }", "{ 9 }");
    let merged = run(&[edit("a", base, &a), edit("b", base, &b), edit("c", base, &c)]).unwrap();
    assert_eq!(merged.bytes, replaced(&replaced(&a, "{ 2 }", "{ 8 }"), "{ 3 }", "{ 9 }"));
}

#[test]
fn comments_raw_strings_chars_lifetimes_and_delimiters_do_not_create_fake_items() {
    let base = br###"/* nested /* fn phantom() { */ comment */
fn alpha<'a>(s: &'a str) -> &'a str {
    let raw = r##" \" } fn fake() { /* "##;
    let bytes = br#" } { "#;
    let normal = "escaped \" } {";
    let c = '\u{1f600}'; let quote = '\''; let byte = b'\x7f';
    // fn fake_again() { }
    let n = 1;
    s
}
fn beta() { let m = 2; }
"###;
    let a = replaced(base, "n = 1", "n = 7");
    let b = replaced(base, "m = 2", "m = 8");
    let merged = run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap();
    assert_eq!(merged.bytes, replaced(&a, "m = 2", "m = 8"));
    let mut budget = Budget::new(Limits::default());
    assert_eq!(scan_rust(base, &mut budget).unwrap().functions.len(), 2);
}

#[test]
fn functions_in_item_macro_tokens_are_never_recognized_as_real_functions() {
    let base = b"macro_rules! generated { () => { fn fake() { 1 } }; }\nfn beta() { 2 }\n";
    let a = replaced(base, "{ 1 }", "{ 7 }");
    let b = replaced(base, "{ 2 }", "{ 8 }");
    assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::OutsideFunction);
}

#[test]
fn malformed_lexical_input_and_ambiguous_item_syntax_fail_closed() {
    for base in [
        "fn alpha() { let x = \"unterminated; 1 }\nfn beta() { 2 }\n",
        "fn alpha() { let x = r###\"unterminated; 1 }\nfn beta() { 2 }\n",
        "fn alpha() { /* unterminated 1 }\nfn beta() { 2 }\n",
        "fn alpha() { let c = '\\q'; 1 }\nfn beta() { 2 }\n",
        "fn alpha() { let c = '\\u{110000}'; 1 }\nfn beta() { 2 }\n",
        "fn alpha() { let c = 'bad'; 1 }\nfn beta() { 2 }\n",
        "fn alpha() { let c = b'雪'; 1 }\nfn beta() { 2 }\n",
        "fn alpha() { (]; 1 }\nfn beta() { 2 }\n",
        "fn alpha() { 1 } }\nfn beta() { 2 }\n",
        "mysterious alpha { 1 }\nfn beta() { 2 }\n",
        "#![some_attribute]\nfn alpha() { 1 }\nfn beta() { 2 }\n",
    ] {
        let base = base.as_bytes();
        let a = replaced(base, "1 }", "7 }");
        let b = replaced(base, "{ 2 }", "{ 8 }");
        assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::AmbiguousRust, "{}", String::from_utf8_lossy(base));
    }
}

#[test]
fn malformed_result_cannot_steal_another_items_body() {
    let a = replaced(BASE, "    1", "    \"7");
    let b = replaced(BASE, "    2", "    8");
    assert_eq!(run(&[edit("a", BASE, &a), edit("b", BASE, &b)]).unwrap_err().kind, ConflictKind::AmbiguousRust);
}

#[test]
fn ambiguous_line_and_scalar_alignments_fail_closed() {
    assert_eq!(run(&[edit("a", b"x\nx\n", b"x\n")]).unwrap_err().kind, ConflictKind::AmbiguousDiff);
    assert_eq!(run(&[edit("a", b"aaaa", b"aaa")]).unwrap_err().kind, ConflictKind::AmbiguousDiff);
    let mut budget = Budget::new(Limits::default());
    assert!(diff(b"a", b"b", &mut budget).is_ok()); // Insert/delete ordering alone is not ambiguous.
}

#[test]
fn diff_cell_work_hunk_and_line_limits_are_enforced() {
    let mut limits = Limits { max_diff_cells: 0, ..Limits::default() };
    assert_eq!(merge(Language::Rust, &[edit("a", b"a", b"b")], &limits).unwrap_err().kind, ConflictKind::LimitExceeded(LimitKind::DiffCells));
    limits = Limits::default();
    limits.max_work = 1;
    assert_eq!(merge(Language::Rust, &[edit("a", b"abc", b"def")], &limits).unwrap_err().kind, ConflictKind::LimitExceeded(LimitKind::Work));
    limits = Limits::default();
    limits.max_hunks = 1;
    assert_eq!(merge(Language::Rust, &[edit("a", b"a1b2c", b"a7b8c")], &limits).unwrap_err().kind, ConflictKind::LimitExceeded(LimitKind::Hunks));
    limits = Limits::default();
    limits.max_lines = 1;
    assert_eq!(merge(Language::Rust, &[edit("a", b"a\nb\n", b"c\nd\n")], &limits).unwrap_err().kind, ConflictKind::LimitExceeded(LimitKind::Lines));
}

#[test]
fn bounds_are_global_not_per_author_and_hard_ceilings_cannot_be_disabled() {
    let a = replaced(BASE, "    1", "    7");
    let b = replaced(BASE, "    2", "    8");
    let mut limits = Limits { max_hunks: 1, ..Limits::default() };
    assert_eq!(merge(Language::Rust, &[edit("a", BASE, &a), edit("b", BASE, &b)], &limits).unwrap_err().kind, ConflictKind::LimitExceeded(LimitKind::Hunks));
    let huge = vec![b'x'; 1024 * 1024 + 1];
    limits = Limits { max_bytes: usize::MAX, max_total_bytes: usize::MAX, ..Limits::default() };
    assert_eq!(merge(Language::Rust, &[edit("a", b"", &huge)], &limits).unwrap_err().kind, ConflictKind::LimitExceeded(LimitKind::SourceBytes));
}

#[test]
fn count_total_source_token_and_nesting_bounds_fail_closed() {
    let a = replaced(BASE, "    1", "    7");
    let b = replaced(BASE, "    2", "    8");
    let changes = [edit("a", BASE, &a), edit("b", BASE, &b)];
    for (limits, kind) in [
        (Limits { max_changes: 1, ..Limits::default() }, LimitKind::Changes),
        (Limits { max_bytes: 1, ..Limits::default() }, LimitKind::SourceBytes),
        (Limits { max_total_bytes: 1, ..Limits::default() }, LimitKind::TotalBytes),
        (Limits { max_tokens: 1, ..Limits::default() }, LimitKind::Tokens),
        (Limits { max_depth: 0, ..Limits::default() }, LimitKind::Nesting),
    ] {
        assert_eq!(merge(Language::Rust, &changes, &limits).unwrap_err().kind, ConflictKind::LimitExceeded(kind));
    }
}

#[test]
fn composition_output_size_is_bounded_too() {
    let base = b"fn a() { 1 }\nfn b() { 2 }\n";
    let a = replaced(base, "{ 1 }", "{ 78901 }");
    let b = replaced(base, "{ 2 }", "{ 78902 }");
    let limits = Limits { max_bytes: a.len(), ..Limits::default() };
    assert_eq!(merge(Language::Rust, &[edit("a", base, &a), edit("b", base, &b)], &limits).unwrap_err().kind, ConflictKind::LimitExceeded(LimitKind::SourceBytes));
}

#[test]
fn macros_paths_and_neighboring_edits_cannot_evade_opaque_function_rule() {
    let base = b"fn alpha() { q::echo!(1); let n = 3; }\nfn beta() { 2 }\n";
    let b = replaced(base, "{ 2 }", "{ 8 }");
    for a in [
        replaced(base, "q::", "z::"),
        replaced(base, "n = 3", "n = 9"),
    ] {
        assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::MacroOrAttribute);
    }
}

#[test]
fn nested_item_import_or_attribute_cannot_be_laundered_as_body_local() {
    for base in [
        "fn alpha() { use before; 1 }\nfn beta() { 2 }\n",
        "fn alpha() { fn nested() -> u32 { 1 } }\nfn beta() { 2 }\n",
        "fn alpha() { #[cfg(test)] let x = 1; }\nfn beta() { 2 }\n",
    ] {
        let base = base.as_bytes();
        let a = replaced(base, "1", "7");
        let b = replaced(base, "{ 2 }", "{ 8 }");
        assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::AmbiguousRust);
    }
}

#[test]
fn multiline_insert_delete_and_identical_insert_dedup_are_byte_exact() {
    let base = b"fn alpha() {\n    let a = 1;\n}\nfn beta() {\n    let b = 2;\n}\n";
    let a = replaced(base, "let a = 1;", "let a = 1;\n    let extra = 7;");
    let b = replaced(base, "    let b = 2;\n", "");
    let both = replaced(&a, "    let b = 2;\n", "");
    assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap().bytes, both);
    assert_eq!(run(&[edit("c", base, &both), edit("b", base, &b), edit("a", base, &a)]).unwrap().bytes, both);
}

#[test]
fn qualified_generic_function_headers_are_scanned_without_changing_them() {
    let base = b"pub(crate) async unsafe fn alpha<'a, T: Copy>(x: &'a T) -> &'a T where T: Send { let n = 1; x }\nfn beta() { 2 }\n";
    let a = replaced(base, "n = 1", "n = 7");
    let b = replaced(base, "{ 2 }", "{ 8 }");
    assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap().bytes, replaced(&a, "{ 2 }", "{ 8 }"));
}

#[test]
fn literal_content_can_change_without_creating_a_function_or_normalizing_newlines() {
    let base = b"fn alpha() { let s = r##\" } fn fake() { 1 } \"##; }\r\nfn beta() { 2 }\r\n";
    let a = replaced(base, "{ 1 }", "{ 7 }");
    let b = replaced(base, "{ 2 }", "{ 8 }");
    assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap().bytes, replaced(&a, "{ 2 }", "{ 8 }"));
}

#[test]
fn long_nearly_matching_raw_closers_are_charged_to_work_limit() {
    let hashes = "#".repeat(200);
    let body = format!("\"{}", "#".repeat(199)).repeat(100);
    let source = format!("fn alpha() {{ let text = r{hashes}\"{body}\"{hashes}; }}");
    let mut budget = Budget::new(Limits { max_work: source.len() + 100, ..Limits::default() });
    assert!(matches!(lex(source.as_bytes(), &mut budget), Err(ConflictKind::LimitExceeded(LimitKind::Work))));
}

#[test]
fn unrecognized_function_or_module_header_does_not_scan_to_an_unrelated_brace() {
    for source in [
        "fn alpha() unexplained { 1 }\nfn beta() { 2 }\n",
        "mod child unexplained { fn alpha() { 1 } }\nfn beta() { 2 }\n",
    ] {
        let base = source.as_bytes();
        let a = replaced(base, "{ 1 }", "{ 7 }");
        let b = replaced(base, "{ 2 }", "{ 8 }");
        assert_eq!(run(&[edit("a", base, &a), edit("b", base, &b)]).unwrap_err().kind, ConflictKind::AmbiguousRust);
    }
}

#[test]
fn bounded_small_diff_exhaustion_always_reconstructs_exact_bytes_or_reports_ambiguity() {
    // Deterministic exhaustive coverage; no random or timing-sensitive fixtures.
    let mut words = vec![vec![]];
    for length in 1..=5 {
        for bits in 0..(1 << length) {
            words.push((0..length).map(|i| if bits & (1 << i) == 0 { b'a' } else { b'b' }).collect());
        }
    }
    for base in &words {
        for result in &words {
            let mut budget = Budget::new(Limits::default());
            match diff(base, result, &mut budget) {
                Ok(hunks) => {
                    let mut rebuilt = Vec::new();
                    let mut cursor = 0;
                    for hunk in hunks {
                        assert!(cursor <= hunk.base.start);
                        rebuilt.extend_from_slice(&base[cursor..hunk.base.start]);
                        rebuilt.extend_from_slice(&result[hunk.result]);
                        cursor = hunk.base.end;
                    }
                    rebuilt.extend_from_slice(&base[cursor..]);
                    assert_eq!(&rebuilt, result);
                }
                Err(kind) => assert_eq!(kind, ConflictKind::AmbiguousDiff),
            }
        }
    }
}
