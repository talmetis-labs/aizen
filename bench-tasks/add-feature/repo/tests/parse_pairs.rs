use fixture_add_feature::parse_pairs;

#[test]
fn splits_on_semicolons_and_skips_malformed_entries() {
    assert_eq!(
        parse_pairs("a=1; b = 2 ;junk;=3;c=x=y"),
        vec![("a", "1"), ("b", "2"), ("c", "x=y")]
    );
}

#[test]
fn empty_input_gives_no_pairs() {
    assert!(parse_pairs("").is_empty());
    assert!(parse_pairs(" ; ; ").is_empty());
}
