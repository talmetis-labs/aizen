use super::*;
use std::fs;
use std::path::PathBuf;

const ID: &str = "p-0123456789abcdef0123456789abcdef";
const OTHER_ID: &str = "p-fedcba9876543210fedcba9876543210";

fn document(body: &str) -> String {
    format!("---\naizen_plan: 1\nid: {ID}\n---\n{body}")
}

fn one_phase() -> String {
    document("<!-- aizen:phase investigate -->\n## Investigate\n")
}

/// Unique filesystem-only fixtures; no cwd, HOME, configuration, or environment mutation.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("aizen-plan-test-{}", new_id().unwrap()));
        fs::create_dir(&root).unwrap();
        // The platform's temp root can itself have a canonical alias (notably on macOS).
        let root = fs::canonicalize(root).unwrap();
        fs::create_dir_all(root.join("docs/plans")).unwrap();
        Self { root }
    }

    fn write(&self, relative: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn plan_ids_are_exact_and_generated_independently() {
    assert!(valid_plan_id(ID));
    assert!(valid_plan_id("p-00000000000000000000000000000000"));
    for invalid in [
        "",
        "p-0123456789abcdef0123456789abcde",
        "p-0123456789abcdef0123456789abcdef0",
        "p-0123456789ABCDEF0123456789abcdef",
        "p-0123456789abcdef0123456789abcdeg",
        "x-0123456789abcdef0123456789abcdef",
        "P-0123456789abcdef0123456789abcdef",
        "p-0123456789abcdef0123456789abcdeé",
    ] {
        assert!(!valid_plan_id(invalid), "{invalid:?}");
    }
    let ids: HashSet<_> = (0..64).map(|_| new_id().unwrap()).collect();
    assert_eq!(ids.len(), 64);
    assert!(ids.iter().all(|id| valid_plan_id(id)));
}

#[test]
fn phase_ids_use_bounded_ascii_grammar() {
    for valid in ["a", "investigate", "step-1", "a-", &"a".repeat(32)] {
        assert!(valid_phase_id(valid), "{valid:?}");
    }
    for invalid in [
        "",
        "A",
        "0step",
        "-step",
        "step_1",
        "two words",
        "étape",
        "step\t",
        "step\n",
        &"a".repeat(33),
    ] {
        assert!(!valid_phase_id(invalid), "{invalid:?}");
    }
}

#[test]
fn parses_lf_crlf_reordered_keys_and_last_line_without_newline() {
    let lf = document("<!-- aizen:phase investigate -->\n<!-- aizen:phase implement -->");
    let expected = ParsedPlan {
        id: ID.into(),
        phases: vec!["investigate".into(), "implement".into()],
    };
    assert_eq!(parse(lf.as_bytes()).unwrap(), expected);
    assert_eq!(
        parse(lf.replace('\n', "\r\n").as_bytes()).unwrap(),
        expected
    );
    let reordered = format!(
        "---\nid: {ID}\naizen_plan: 1\n---\n<!-- aizen:phase investigate -->\n<!-- aizen:phase implement -->"
    );
    assert_eq!(parse(reordered.as_bytes()).unwrap(), expected);
}

#[test]
fn phases_keep_document_order_not_title_identity() {
    let parsed = parse(
        document("<!-- aizen:phase implement -->\n## Arbitrary renamed title\n<!-- aizen:phase investigate -->\n## Same title\n")
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(parsed.id, ID);
    assert_eq!(parsed.phases, ["implement", "investigate"]);
}

#[test]
fn rejects_duplicate_missing_unknown_and_unsupported_header_fields() {
    for header in [
        format!("aizen_plan: 1\naizen_plan: 1\nid: {ID}"),
        format!("aizen_plan: 1\nid: {ID}\nid: {ID}"),
        format!("aizen_plan: 1\nid: {ID}\nextra: value"),
        format!("aizen_plan: 2\nid: {ID}"),
        format!("aizen_plan: 0\nid: {ID}"),
        format!("aizen_plan: 01\nid: {ID}"),
        format!("aizen_plan: 1.0\nid: {ID}"),
        format!("id: {ID}"),
        "aizen_plan: 1".into(),
        "aizen_plan: 1\nid: wrong".into(),
        String::new(),
    ] {
        let text = format!("---\n{header}\n---\n<!-- aizen:phase work -->\n");
        assert!(parse(text.as_bytes()).is_err(), "{header:?}");
    }
}

#[test]
fn rejects_yaml_features_and_malformed_scalar_syntax() {
    for header in [
        format!("aizen_plan: \"1\"\nid: {ID}"),
        format!("aizen_plan: 1 # note\nid: {ID}"),
        format!("aizen_plan:1\nid: {ID}"),
        format!("aizen_plan:\t1\nid: {ID}"),
        format!(" aizen_plan: 1\nid: {ID}"),
        format!("aizen_plan: 1\n\nid: {ID}"),
        format!("aizen_plan: 1\nid: '{ID}'"),
        format!("aizen_plan: 1\nid: &identity {ID}"),
        format!("aizen_plan: 1\nid: !!str {ID}"),
        format!("aizen_plan: 1\nid: [{ID}]"),
        format!("aizen_plan: 1\nid: |\n  {ID}"),
        "aizen_plan: 1\nid: *identity".into(),
        format!("aizen_plan: 1\n\"id\": {ID}"),
        format!("aizen_plan: 1\nid: {ID}\t"),
    ] {
        let text = format!("---\n{header}\n---\n<!-- aizen:phase work -->\n");
        assert!(parse(text.as_bytes()).is_err(), "{header:?}");
    }
}

#[test]
fn rejects_missing_delimiters_bom_invalid_utf8_and_bare_cr() {
    for bytes in [
        Vec::new(),
        b"# Legacy plan\n".to_vec(),
        format!("aizen_plan: 1\nid: {ID}\n---\n").into_bytes(),
        format!("---\naizen_plan: 1\nid: {ID}\n").into_bytes(),
        format!("---\naizen_plan: 1\nid: {ID}\n...\n").into_bytes(),
        format!("\u{feff}{}", one_phase()).into_bytes(),
        format!("\n{}", one_phase()).into_bytes(),
        one_phase().replace('\n', "\r").into_bytes(),
        [one_phase().as_bytes(), &[0xff]].concat(),
        format!("{}bad\rtext", one_phase()).into_bytes(),
    ] {
        assert!(parse(&bytes).is_err(), "{bytes:?}");
    }
}

#[test]
fn header_bound_includes_delimiters_and_line_endings() {
    let header = format!("---\naizen_plan: 1\nid: {ID}\n---\n");
    for newline in ["\n", "\r\n"] {
        let header = header.replace('\n', newline);
        let padding = " ".repeat(MAX_HEADER_BYTES - header.len());
        let exact = header.replacen("aizen_plan: 1", &format!("aizen_plan: {padding}1"), 1);
        assert_eq!(exact.len(), MAX_HEADER_BYTES);
        let body = "<!-- aizen:phase work -->";
        assert!(parse(format!("{exact}{body}").as_bytes()).is_ok());
        let overflow = exact.replacen("aizen_plan: ", "aizen_plan:  ", 1);
        assert!(parse(format!("{overflow}{body}").as_bytes()).is_err());
    }
    let huge_line = format!("---\n{}", "x".repeat(MAX_DOCUMENT_BYTES - 4));
    assert!(parse(huge_line.as_bytes()).is_err());
}

#[test]
fn whole_document_bound_is_inclusive_and_covers_prose() {
    let mut text = one_phase();
    text.push_str(&"x".repeat(MAX_DOCUMENT_BYTES - text.len()));
    assert_eq!(text.len(), MAX_DOCUMENT_BYTES);
    assert!(parse(text.as_bytes()).is_ok());
    text.push('x');
    assert!(parse(text.as_bytes()).is_err());
}

#[test]
fn phase_count_bounds_duplicates_and_invalid_markers() {
    assert!(parse(document("# No phase\n").as_bytes()).is_err());
    let body: String = (0..MAX_PHASES)
        .map(|i| format!("<!-- aizen:phase step-{i} -->\n"))
        .collect();
    assert_eq!(parse(document(&body).as_bytes()).unwrap().phases.len(), 64);
    assert!(parse(document(&format!("{body}<!-- aizen:phase overflow -->")).as_bytes()).is_err());
    assert!(
        parse(document("<!-- aizen:phase same -->\n<!-- aizen:phase same -->").as_bytes()).is_err()
    );
    for marker in [
        "<!-- aizen:phase Upper -->",
        "<!-- aizen:phase 1step -->",
        "<!-- aizen:phase with_space -->",
        "<!-- aizen:phase -->",
        "<!-- aizen:phase two words -->",
        " <!-- aizen:phase leading -->",
        "<!-- aizen:phase trailing --> ",
        "prefix <!-- aizen:phase inline -->",
        "<!-- aizen:phase inline --> suffix",
        "<!--aizen:phase spacing-->",
        "<!-- aizen:phase unclosed",
        "<!-- aizen: phase malformed -->",
        "<!-- aizen:future-contract unknown -->",
    ] {
        assert!(
            parse(document(&format!("<!-- aizen:phase okay -->\n{marker}")).as_bytes()).is_err(),
            "{marker:?}"
        );
    }
    assert!(
        parse(document(&format!("<!-- aizen:phase {} -->", "a".repeat(33))).as_bytes()).is_err()
    );
}

#[test]
fn ignores_fenced_markers_with_correct_closing_length_semantics() {
    let body = concat!(
        "````rust\n",
        "<!-- aizen:phase hidden -->\n",
        "```\n", // Too short; not a close.
        "<!-- aizen:phase hidden -->\n",
        "~~~~\n", // Wrong delimiter; not a close.
        "<!-- aizen:phase INVALID -->\n",
        "```` trailing\n", // Closing fences cannot have an info string.
        "<!-- aizen:phase hidden -->\n",
        "  ````` \t\n",
        "<!-- aizen:phase visible -->\n",
        "   ~~~~ arbitrary `info`\n",
        "<!-- aizen:phase visible -->\n",
        "~~~\n",
        "<!-- aizen:phase hidden -->\n",
        "    ~~~~\n", // Four leading spaces; not a close.
        "<!-- aizen:phase hidden -->\n",
        "  ~~~~~\t\n",
        "<!-- aizen:phase second -->\n",
    );
    for text in [document(body), document(body).replace('\n', "\r\n")] {
        assert_eq!(
            parse(text.as_bytes()).unwrap().phases,
            ["visible", "second"]
        );
    }
}

#[test]
fn unclosed_fences_hide_remaining_markers_and_invalid_openers_do_not() {
    assert!(parse(document("```\n<!-- aizen:phase hidden -->\n").as_bytes()).is_err());
    let visible_first =
        document("<!-- aizen:phase visible -->\n~~~\n<!-- aizen:phase hidden -->\n");
    assert_eq!(parse(visible_first.as_bytes()).unwrap().phases, ["visible"]);
    for opener in ["``", "~~", "    ```", "```bad`info", "    ~~~"] {
        let text = document(&format!("{opener}\n<!-- aizen:phase visible -->\n"));
        assert_eq!(parse(text.as_bytes()).unwrap().phases, ["visible"]);
    }
}

#[test]
fn validates_nested_markdown_paths_without_inventing_identity() {
    for path in [
        "docs/plans/task.md",
        "docs/plans/release/step-1.md",
        "docs/plans/a b.md",
    ] {
        assert!(validate_relative_path(path).is_ok(), "{path:?}");
    }
}

#[test]
fn rejects_traversal_aliases_and_non_markdown_paths_on_every_host() {
    for path in [
        "",
        "plan.md",
        "docs/plan.md",
        "docs/plans.md",
        "docs/plans/",
        "docs/plans/a.txt",
        "docs/plans/a.MD",
        "docs/plans/a.md/",
        "docs/plans//a.md",
        "docs/plans/./a.md",
        "docs/plans/../a.md",
        "docs/plans/sub/../../a.md",
        "./docs/plans/a.md",
        "Docs/plans/a.md",
        "/docs/plans/a.md",
        "C:/docs/plans/a.md",
        "C:docs/plans/a.md",
        "//server/share/docs/plans/a.md",
        "docs\\plans\\a.md",
        "docs/plans/sub\\a.md",
        "docs/plans/a.md:stream",
        "docs/plans/a?.md",
        "docs/plans/a*.md",
        "docs/plans/<a>.md",
        "docs/plans/a|b.md",
        "docs/plans/a\"b.md",
        "docs/plans/a[1].md",
        "docs/plans/{a}.md",
        "docs/plans/trailing./a.md",
        "docs/plans/trailing /a.md",
        "docs/plans/a\0.md",
        "docs/plans/a\n.md",
        "docs/plans/a\t.md",
        "docs/plans/a\u{7f}.md",
    ] {
        assert!(validate_relative_path(path).is_err(), "{path:?}");
    }
}

#[test]
fn rejects_windows_devices_even_on_unix() {
    for component in [
        "CON", "con.txt", "NUL.md", "PRN", "AUX", "COM1", "com9.txt", "LPT1", "lpt9.md", "COM¹",
        "LPT²", "LPT³", "CONIN$", "CONOUT$", "con .txt",
    ] {
        let path = format!("docs/plans/{component}/task.md");
        assert!(validate_relative_path(&path).is_err(), "{path:?}");
    }
    assert!(validate_relative_path("docs/plans/COM10.md").is_ok());
}

#[test]
fn path_and_component_bounds_are_bytes_not_characters() {
    let exact = format!("docs/plans/{}/{}.md", "a".repeat(250), "b".repeat(247));
    assert_eq!(exact.len(), MAX_PATH_BYTES);
    assert!(validate_relative_path(&exact).is_ok());
    let overflow = exact.replacen("/b", "/bb", 1);
    assert_eq!(overflow.len(), MAX_PATH_BYTES + 1);
    assert!(validate_relative_path(&overflow).is_err());
    assert!(validate_relative_path(&format!("docs/plans/{}.md", "a".repeat(252))).is_ok());
    assert!(validate_relative_path(&format!("docs/plans/{}.md", "a".repeat(253))).is_err());
    assert!(validate_relative_path(&format!("docs/plans/{}.md", "é".repeat(127))).is_err());
}

#[test]
fn checked_read_returns_exact_byte_hash_and_bounded_utf8_excerpt() {
    let workspace = Workspace::new();
    let mut text = one_phase().replace('\n', "\r\n");
    text.push_str(&"x".repeat(MAX_EXCERPT_BYTES - 1 - text.len()));
    text.push_str("界more prose\r\n");
    workspace.write("docs/plans/sub/task.md", text.as_bytes());
    let observed = read(&workspace.root, "docs/plans/sub/task.md").unwrap();
    assert_eq!(observed.parsed.id, ID);
    assert_eq!(
        observed.fingerprint,
        format!("{:x}", Sha256::digest(text.as_bytes()))
    );
    assert_eq!(observed.fingerprint.len(), 64);
    assert!(observed
        .fingerprint
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert_eq!(observed.excerpt.len(), MAX_EXCERPT_BYTES - 1);
    assert!(text.starts_with(&observed.excerpt));
    assert_ne!(
        observed.fingerprint,
        format!(
            "{:x}",
            Sha256::digest(text.replace("\r\n", "\n").as_bytes())
        )
    );
}

#[test]
fn checked_bytes_and_hashes_distinguish_repeated_invalid_observations() {
    let workspace = Workspace::new();
    let relative = "docs/plans/broken.md";
    workspace.write(relative, b"---\naizen_plan: 9\n");
    let first = read_bytes(&workspace.root, relative).unwrap();
    assert!(parse(&first).is_err());
    let repeated = read_bytes(&workspace.root, relative).unwrap();
    assert_eq!(fingerprint(&first), fingerprint(&repeated));
    workspace.write(relative, b"---\naizen_plan: 10\n");
    assert_ne!(
        fingerprint(&first),
        fingerprint(&read_bytes(&workspace.root, relative).unwrap())
    );
    workspace.write(relative, [0xff, 0xfe]);
    assert_eq!(read_bytes(&workspace.root, relative).unwrap(), [0xff, 0xfe]);
    assert!(read(&workspace.root, relative).is_err());
    assert_eq!(excerpt(&[0xff]), "");
    assert_eq!(excerpt(b"short"), "short");
    assert_eq!(
        fingerprint(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        excerpt(&vec![b'x'; MAX_EXCERPT_BYTES + 1]).len(),
        MAX_EXCERPT_BYTES
    );
}

#[test]
fn checked_read_validates_markers_beyond_excerpt() {
    let workspace = Workspace::new();
    let mut text = one_phase();
    text.push_str(&"x".repeat(MAX_EXCERPT_BYTES));
    text.push_str("\n<!-- aizen:phase investigate -->\n");
    workspace.write("docs/plans/task.md", text);
    assert!(read(&workspace.root, "docs/plans/task.md").is_err());
}

#[test]
fn checked_read_refuses_missing_directory_malformed_and_oversized_files() {
    let workspace = Workspace::new();
    assert!(read(&workspace.root, "docs/plans/missing.md").is_err());
    assert!(!workspace.root.join("docs/plans/missing.md").exists());
    assert!(read(&workspace.root, "docs/plans/missing/sub.md").is_err());
    assert!(!workspace.root.join("docs/plans/missing").exists());
    fs::create_dir(workspace.root.join("docs/plans/directory.md")).unwrap();
    assert!(read(&workspace.root, "docs/plans/directory.md").is_err());
    workspace.write("docs/plans/invalid.md", b"# Not a contract\n");
    assert!(read(&workspace.root, "docs/plans/invalid.md").is_err());
    let mut text = one_phase();
    text.push_str(&"x".repeat(MAX_DOCUMENT_BYTES - text.len()));
    workspace.write("docs/plans/large.md", &text);
    assert!(read(&workspace.root, "docs/plans/large.md").is_ok());
    text.push('x');
    workspace.write("docs/plans/large.md", &text);
    assert!(read(&workspace.root, "docs/plans/large.md").is_err());
    assert!(read(Path::new("relative"), "docs/plans/large.md").is_err());
    assert!(read(&workspace.root, "docs/plans/../large.md").is_err());
}

#[test]
fn same_id_editor_atomic_replacement_is_a_fresh_observation() {
    let workspace = Workspace::new();
    let path = workspace.write("docs/plans/task.md", one_phase());
    let first = read(&workspace.root, "docs/plans/task.md").unwrap();
    let replacement = document("<!-- aizen:phase implement -->\n## Edited independently\n");
    crate::core::persist::atomic_write(&path, replacement.as_bytes()).unwrap();
    let second = read(&workspace.root, "docs/plans/task.md").unwrap();
    assert_eq!(first.parsed.id, second.parsed.id);
    assert_ne!(first.fingerprint, second.fingerprint);
    assert_eq!(second.parsed.phases, ["implement"]);
    assert_eq!(read(&workspace.root, "docs/plans/task.md").unwrap(), second);
    // The reader observes identity; attachment policy, not this API, rejects an ID change.
    crate::core::persist::atomic_write(&path, replacement.replace(ID, OTHER_ID).as_bytes())
        .unwrap();
    assert_eq!(
        read(&workspace.root, "docs/plans/task.md")
            .unwrap()
            .parsed
            .id,
        OTHER_ID
    );
}

#[test]
fn checked_read_refuses_hardlinked_files() {
    let workspace = Workspace::new();
    let source = workspace.write("outside.md", one_phase());
    let target = workspace.root.join("docs/plans/hardlink.md");
    match fs::hard_link(source, target) {
        Ok(()) => assert!(read(&workspace.root, "docs/plans/hardlink.md").is_err()),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
            eprintln!("SKIP hardlink fixture: filesystem does not support hard links: {error}");
        }
        Err(error) => panic!("cannot create hardlink fixture: {error}"),
    }
}

#[cfg(any(unix, windows))]
fn symlink(target: &Path, link: &Path, directory: bool) -> bool {
    #[cfg(unix)]
    let result = {
        let _ = directory;
        std::os::unix::fs::symlink(target, link)
    };
    #[cfg(windows)]
    let result = if directory {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    };
    match result {
        Ok(()) => true,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) || cfg!(windows) && error.raw_os_error() == Some(1314) =>
        {
            eprintln!("SKIP symlink fixture: unavailable privileges/filesystem support: {error}");
            false
        }
        Err(error) => panic!("cannot create symlink fixture: {error}"),
    }
}

#[cfg(any(unix, windows))]
#[test]
fn checked_read_refuses_leaf_and_ancestor_links_even_inside_workspace() {
    let workspace = Workspace::new();
    let outside = workspace.write("elsewhere/task.md", one_phase());
    let leaf = workspace.root.join("docs/plans/link.md");
    if !symlink(&outside, &leaf, false) {
        return;
    }
    assert!(read(&workspace.root, "docs/plans/link.md").is_err());
    let nested = workspace.root.join("docs/plans/linked");
    if !symlink(outside.parent().unwrap(), &nested, true) {
        return;
    }
    assert!(read(&workspace.root, "docs/plans/linked/task.md").is_err());
}

#[cfg(any(unix, windows))]
#[test]
fn checked_read_refuses_workspace_root_link_before_canonicalizing() {
    let parent = Workspace::new();
    let workspace = Workspace::new();
    workspace.write("docs/plans/task.md", one_phase());
    let alias = parent.root.join("workspace-alias");
    if !symlink(&workspace.root, &alias, true) {
        return;
    }
    assert!(read(&alias, "docs/plans/task.md").is_err());
}

#[test]
fn explicit_migration_preserves_every_original_byte_and_adds_one_phase() {
    for original in [
        "",
        "# Old plan\n\n* Investigate\n* Ship\n",
        "# Kế hoạch cũ\r\n\r\nKeep trailing spaces.  \r\nNo final newline",
        "---\ntitle: Legacy document\n---\n\nPreserve existing frontmatter.\n",
        "\u{feff}Original BOM stays in the body.\n",
        "~~~\nordinary fenced prose\n~~~\n",
    ] {
        let migrated = migrate_legacy(original.as_bytes(), ID).unwrap();
        assert!(migrated.as_bytes().ends_with(original.as_bytes()));
        let prefix =
            format!("---\naizen_plan: 1\nid: {ID}\n---\n\n<!-- aizen:phase legacy -->\n\n");
        assert_eq!(migrated, format!("{prefix}{original}"));
        assert_eq!(
            parse(migrated.as_bytes()).unwrap(),
            ParsedPlan {
                id: ID.into(),
                phases: vec!["legacy".into()],
            }
        );
    }
}

#[test]
fn migration_never_repairs_or_overwrites_existing_contract_identity() {
    for original in [
        one_phase(),
        one_phase().replace("aizen_plan: 1", "aizen_plan: 2"),
        one_phase().replace(ID, "invalid"),
        format!("---\nid: {ID}\n---\n# Damaged contract\n"),
        format!("---\n\"id\": {ID}\n---\n"),
        "---\naizen_plan 1\n---\n".into(),
        "# Old\n<!-- aizen:phase bad marker -->\n".into(),
        "# Old\n<!--aizen:phase broken\n".into(),
        "# Old\n<!-- aizen: phase malformed -->\n".into(),
        "# Old\n<!-- aizen:future-contract unknown -->\n".into(),
        "```\n<!-- aizen:phase example -->\n```\n".into(),
        "```\naizen_plan: 1\n```\n".into(),
    ] {
        assert!(
            migrate_legacy(original.as_bytes(), OTHER_ID).is_err(),
            "{original:?}"
        );
    }
}

#[test]
fn migration_bounds_include_added_header_and_reject_invalid_text_or_id() {
    assert!(migrate_legacy(b"prose", "invalid").is_err());
    assert!(migrate_legacy(&[0xff], ID).is_err());
    assert!(migrate_legacy(b"bare\rcarriage return", ID).is_err());
    let overhead = migrate_legacy(b"", ID).unwrap().len();
    let exact = "x".repeat(MAX_DOCUMENT_BYTES - overhead);
    assert_eq!(
        migrate_legacy(exact.as_bytes(), ID).unwrap().len(),
        MAX_DOCUMENT_BYTES
    );
    assert!(migrate_legacy(format!("{exact}x").as_bytes(), ID).is_err());
    assert!(migrate_legacy(&vec![b'x'; MAX_DOCUMENT_BYTES + 1], ID).is_err());
}
