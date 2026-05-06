use std::path::PathBuf;

use wisetree::utils::path::{
    get_worktree_path, repository_base_name, resolve_template, TemplateVariables,
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
