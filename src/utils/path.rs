//! Template resolution + worktree path computation.
//!
//! Mirrors `branchlet/src/utils/path-utils.ts` exactly: variables are
//! substituted as `$KEY` tokens, and `getWorktreePath` joins the parent
//! directory of the git root with the resolved template, then with the
//! directory name.

use std::path::{Path, PathBuf};

/// Variables available in user-supplied path templates and post-create
/// commands. Names match the upstream TS interface character-for-character so
/// existing `.wisetree.json` configs work unchanged.
#[derive(Debug, Clone, Default)]
pub struct TemplateVariables {
    pub base_path: String,
    pub worktree_path: String,
    pub branch_name: String,
    pub source_branch: String,
}

impl TemplateVariables {
    fn pairs(&self) -> [(&'static str, &str); 4] {
        [
            ("BASE_PATH", &self.base_path),
            ("WORKTREE_PATH", &self.worktree_path),
            ("BRANCH_NAME", &self.branch_name),
            ("SOURCE_BRANCH", &self.source_branch),
        ]
    }
}

/// Replace every `$KEY` occurrence in `template` with the matching value.
/// Unknown keys are left as-is (matches upstream).
pub fn resolve_template(template: &str, vars: &TemplateVariables) -> String {
    let mut out = template.to_string();
    for (key, value) in vars.pairs() {
        let needle = format!("${key}");
        out = out.replace(&needle, value);
    }
    out
}

/// Last path component of `git_root` — the repository's directory name.
pub fn repository_base_name(git_root: &Path) -> String {
    git_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Compute the absolute path for a new worktree.
///
/// `template` is the user-configured `worktreePathTemplate`. The result is
/// `<parent of git_root>/<resolved template>/<directory_name>`.
pub fn get_worktree_path(
    git_root: &Path,
    directory_name: &str,
    template: &str,
    branch_name: Option<&str>,
    source_branch: Option<&str>,
) -> PathBuf {
    let base_name = repository_base_name(git_root);
    let parent_dir = git_root.parent().map(Path::to_path_buf).unwrap_or_default();

    let vars = TemplateVariables {
        base_path: base_name,
        worktree_path: parent_dir
            .join(directory_name)
            .to_string_lossy()
            .into_owned(),
        branch_name: branch_name.unwrap_or("").to_string(),
        source_branch: source_branch.unwrap_or("").to_string(),
    };

    let resolved = resolve_template(template, &vars);
    parent_dir.join(resolved).join(directory_name)
}
