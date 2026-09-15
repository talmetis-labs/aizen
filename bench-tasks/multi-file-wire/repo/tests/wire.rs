use fixture_multi_file_wire::{run, Config};

#[test]
fn plain_greeting_is_unchanged() {
    assert_eq!(run(&["ada"]), "hello, ada");
    assert_eq!(run(&[]), "hello, world");
}

#[test]
fn the_flag_is_parsed_into_config() {
    let c = Config::from_args(&["--verbose", "ada"]);
    assert!(c.verbose);
    assert_eq!(c.name, "ada");
    assert!(!Config::from_args(&["ada"]).verbose);
}

#[test]
fn the_flag_reaches_the_greeting() {
    assert_eq!(run(&["--verbose", "ada"]), "hello, ada (verbose)");
    assert_eq!(run(&["ada", "--verbose"]), "hello, ada (verbose)");
}
