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
///
/// Single-pass scan: when a `$` is followed by a recognised key we substitute
/// the value verbatim into the output, then continue from the character after
/// the key. This makes substitution order-independent — values may safely
/// contain `$OTHER_KEY` literals without being re-expanded by a later
/// iteration.
pub fn resolve_template(template: &str, vars: &TemplateVariables) -> String {
    let pairs = vars.pairs();
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let rest = &template[i + 1..];
            if let Some((key, value)) = pairs.iter().find(|(k, _)| rest.starts_with(*k)) {
                out.push_str(value);
                i += 1 + key.len();
                continue;
            }
        }
        // Push a full UTF-8 char (templates may contain multi-byte chars).
        let ch = template[i..].chars().next().expect("non-empty slice");
        out.push(ch);
        i += ch.len_utf8();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(
        base_path: &str,
        worktree_path: &str,
        branch_name: &str,
        source_branch: &str,
    ) -> TemplateVariables {
        TemplateVariables {
            base_path: base_path.to_string(),
            worktree_path: worktree_path.to_string(),
            branch_name: branch_name.to_string(),
            source_branch: source_branch.to_string(),
        }
    }

    #[test]
    fn substitutes_known_keys_and_leaves_unknown_ones_alone() {
        let v = vars("/repo", "/repo/wt", "feat/x", "main");
        let out = resolve_template("$BASE_PATH | $WORKTREE_PATH | $UNKNOWN", &v);
        assert_eq!(out, "/repo | /repo/wt | $UNKNOWN");
    }

    #[test]
    fn does_not_recurse_into_substituted_values() {
        // BRANCH_NAME contains the literal `$SOURCE_BRANCH`. The previous
        // implementation expanded it in a later pass because substitutions
        // ran sequentially. Single-pass scanning keeps the value verbatim.
        let v = vars("", "", "$SOURCE_BRANCH", "main");
        let out = resolve_template("$BRANCH_NAME", &v);
        assert_eq!(out, "$SOURCE_BRANCH");
    }

    #[test]
    fn value_with_dollar_other_key_is_not_reexpanded() {
        let v = vars("a$WORKTREE_PATHb", "WTVAL", "", "");
        let out = resolve_template("$BASE_PATH", &v);
        assert_eq!(out, "a$WORKTREE_PATHb");
    }

    #[test]
    fn handles_adjacent_and_repeated_keys() {
        let v = vars("X", "Y", "Z", "Q");
        let out = resolve_template("$BASE_PATH$BASE_PATH$WORKTREE_PATH", &v);
        assert_eq!(out, "XXY");
    }

    #[test]
    fn bare_dollar_is_preserved() {
        let v = vars("", "", "", "");
        assert_eq!(resolve_template("$ alone $", &v), "$ alone $");
    }
}
