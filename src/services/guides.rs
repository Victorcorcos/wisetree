//! Repository-local implementation guides.
//!
//! A guide is a hand-written markdown note about internal logic the code
//! alone does not make obvious — an access-control model with N permission
//! levels, a tenancy scheme, a legacy data-migration path. Guides are
//! per-repository and personal: they live in `.wisetree/guides/` inside the
//! working tree, which the user gitignores, so nothing here is ever
//! committed for the team.
//!
//! The AI commands (Develop plan/implement, Bugkill investigate/fix) never
//! embed a guide's body in their prompt. They embed only the index rendered
//! by [`render_index`] — one entry per guide with a "Use when" line and its
//! path — and the harness's own file-reading tool pulls the body in when,
//! and only when, the activity at hand matches. That keeps the prompt cost
//! flat (a few dozen tokens per guide) no matter how long the guides are,
//! and lets a bug that has nothing to do with authentication skip the
//! authentication guide entirely.
//!
//! Guides are resolved from the mother worktree first, then the current
//! worktree, mirroring how [`crate::config::ConfigService::load_for_worktree`]
//! resolves the project config. A guide written in a worktree shadows the
//! mother's guide of the same name.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Directory holding the guides, relative to a worktree root.
pub const GUIDES_DIR: &str = ".wisetree/guides";

/// Rendered in place of the index when the repository has no guides.
pub const NO_GUIDES: &str = "(this repository has no guides — rely on the code alone)";

/// Guides beyond this many are dropped from the index: past this point the
/// list stops being a menu the model can reason about.
const MAX_GUIDES: usize = 24;

/// Per-field caps, so one runaway guide header cannot dominate the prompt.
const MAX_WHEN_BYTES: usize = 240;
const MAX_APPLIES_BYTES: usize = 200;

/// One guide's header — never its body, which the AI reads from disk itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guide {
    /// Identifier shown in the index; defaults to the file stem.
    pub name: String,
    /// The "Use when" line: what activity should pull this guide in.
    pub when: String,
    /// Optional path hints (globs) the guide covers.
    pub applies_to: Vec<String>,
    /// Path as the AI must open it — relative to the worktree when the guide
    /// lives in it, absolute when it lives in the mother.
    pub display_path: String,
    /// Whole days since the file was last modified, when readable. Guides are
    /// gitignored, so file mtime is the only staleness signal available.
    pub age_days: Option<u64>,
}

/// Render the guide index for a worktree, ready to be substituted into a
/// prompt's `REPOSITORY_GUIDES` placeholder.
pub fn index_for(worktree_path: &Path) -> String {
    render_index(&discover(worktree_path))
}

/// Collect the guides visible from `worktree_path`, sorted by name.
pub fn discover(worktree_path: &Path) -> Vec<Guide> {
    let mut found: BTreeMap<String, Guide> = BTreeMap::new();

    // Mother first, then the worktree: a same-named guide in the worktree
    // shadows the mother's.
    if let Some(mother) = mother_worktree(worktree_path) {
        if mother != worktree_path {
            collect_into(&mother.join(GUIDES_DIR), true, &mut found);
        }
    }
    collect_into(&worktree_path.join(GUIDES_DIR), false, &mut found);

    found.into_values().take(MAX_GUIDES).collect()
}

/// Render the index block embedded in the AI prompts. Returns [`NO_GUIDES`]
/// when there is nothing to offer, so the prompt stays well-formed.
pub fn render_index(guides: &[Guide]) -> String {
    if guides.is_empty() {
        return NO_GUIDES.to_string();
    }

    let mut out = String::from(
        "Each entry below is a note about internal logic in THIS repository that the code \
         does not make obvious. Open a guide with your file-reading tool ONLY when its \
         \"Use when\" line matches what you are about to do, and skip every other one — \
         reading them all wastes the context this task needs.\n\n",
    );

    for guide in guides {
        out.push_str(&format!("- {} — `{}`", guide.name, guide.display_path));
        if let Some(days) = guide.age_days {
            out.push_str(&format!(" ({})", humanize_age(days)));
        }
        out.push('\n');
        out.push_str(&format!("  Use when: {}\n", guide.when));
        if !guide.applies_to.is_empty() {
            out.push_str(&format!("  Applies to: {}\n", guide.applies_to.join(", ")));
        }
    }

    out.push_str(
        "\nThese guides are hand-written and are not kept in sync with the code \
         automatically, so an old one can be wrong. If a guide contradicts the code you \
         are reading, the CODE WINS: follow the code, and state the contradiction \
         explicitly in your output so the human can fix the guide.",
    );
    out
}

/// Read every `*.md` in `dir` into `found`.
fn collect_into(dir: &Path, absolute: bool, found: &mut BTreeMap<String, Guide>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let display_path = if absolute {
            path.display().to_string()
        } else {
            format!(
                "{GUIDES_DIR}/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        };
        if let Some(guide) = parse_guide(&path, &contents, display_path) {
            found.insert(guide.name.clone(), guide);
        }
    }
}

/// Parse a guide's header. A guide needs a `when:` (or `description:`) line
/// in its frontmatter — without one the AI has no basis to decide whether to
/// read it, so the file is skipped rather than offered blindly.
fn parse_guide(path: &Path, contents: &str, display_path: String) -> Option<Guide> {
    let fields = parse_frontmatter(contents)?;
    let when = fields
        .get("when")
        .or_else(|| fields.get("description"))
        .map(|v| truncate(v, MAX_WHEN_BYTES))
        .filter(|v| !v.is_empty())?;

    let name = fields
        .get("name")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

    let applies_to = fields
        .get("applies_to")
        .or_else(|| fields.get("paths"))
        .map(|v| {
            truncate(v, MAX_APPLIES_BYTES)
                .split(',')
                .map(|p| p.trim().trim_matches(['"', '\'', '[', ']']).to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();

    Some(Guide {
        name,
        when,
        applies_to,
        display_path,
        age_days: age_days(path),
    })
}

/// Read the `key: value` pairs of a leading `---` fenced block. Deliberately
/// not YAML: guide headers are three flat string fields, and a real parser
/// would be a dependency bought for nothing.
fn parse_frontmatter(contents: &str) -> Option<BTreeMap<String, String>> {
    let body = contents
        .strip_prefix("---")?
        .trim_start_matches(['\r', '\n']);
    let end = body.find("\n---")?;
    let mut fields = BTreeMap::new();
    for line in body[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            fields.insert(key.trim().to_ascii_lowercase(), value.to_string());
        }
    }
    Some(fields)
}

/// Whole days since `path` was last written.
fn age_days(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let elapsed = SystemTime::now().duration_since(modified).ok()?;
    Some(elapsed.as_secs() / 86_400)
}

fn humanize_age(days: u64) -> String {
    match days {
        0 => "updated today".to_string(),
        1 => "updated 1 day ago".to_string(),
        2..=60 => format!("updated {days} days ago"),
        _ => format!("updated {} months ago", days / 30),
    }
}

fn truncate(value: &str, max_bytes: usize) -> String {
    let value = value.trim();
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// The mother worktree of `worktree_path`, derived from its `.git` entry: a
/// directory means this *is* the mother, a file points at
/// `<mother>/.git/worktrees/<name>`.
fn mother_worktree(worktree_path: &Path) -> Option<PathBuf> {
    let dot_git = worktree_path.join(".git");
    if dot_git.is_dir() {
        return Some(worktree_path.to_path_buf());
    }
    let pointer = fs::read_to_string(&dot_git).ok()?;
    let gitdir = Path::new(pointer.trim().strip_prefix("gitdir:")?.trim());
    // <mother>/.git/worktrees/<name> → <mother>
    let worktrees = gitdir.parent()?;
    let git_dir = worktrees.parent()?;
    (git_dir.file_name()? == ".git" && worktrees.file_name()? == "worktrees")
        .then(|| git_dir.parent().map(Path::to_path_buf))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    fn write_guide(dir: &Path, file: &str, contents: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(file), contents).unwrap();
    }

    fn mother_repo() -> TempDir {
        let repo = TempDir::new().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        repo
    }

    #[test]
    fn a_guide_with_frontmatter_becomes_an_index_entry() {
        let repo = mother_repo();
        write_guide(
            &repo.path().join(GUIDES_DIR),
            "auth.md",
            "---\nname: auth-permissions\nwhen: touching authentication or permission levels\n\
             applies_to: app/policies/**, app/models/user*.rb\n---\n\nLong body the index never carries.\n",
        );

        let guides = discover(repo.path());

        assert_eq!(guides.len(), 1);
        assert_eq!(guides[0].name, "auth-permissions");
        assert_eq!(
            guides[0].when,
            "touching authentication or permission levels"
        );
        assert_eq!(
            guides[0].applies_to,
            vec![
                "app/policies/**".to_string(),
                "app/models/user*.rb".to_string()
            ]
        );
        assert_eq!(guides[0].display_path, ".wisetree/guides/auth.md");

        let index = render_index(&guides);
        assert!(index.contains("auth-permissions — `.wisetree/guides/auth.md`"));
        assert!(index.contains("Use when: touching authentication or permission levels"));
        assert!(index.contains("Applies to: app/policies/**, app/models/user*.rb"));
        assert!(index.contains("updated today"));
        // The body stays on disk — the whole point of the index.
        assert!(!index.contains("Long body"));
        // Staleness contract travels with every index.
        assert!(index.contains("CODE WINS"));
    }

    #[test]
    fn the_name_falls_back_to_the_file_stem_and_paths_is_an_accepted_alias() {
        let repo = mother_repo();
        write_guide(
            &repo.path().join(GUIDES_DIR),
            "multitenancy.md",
            "---\ndescription: scoping queries per tenant\npaths: [\"app/models/**\"]\n---\n",
        );

        let guides = discover(repo.path());

        assert_eq!(guides[0].name, "multitenancy");
        assert_eq!(guides[0].when, "scoping queries per tenant");
        assert_eq!(guides[0].applies_to, vec!["app/models/**".to_string()]);
    }

    #[test]
    fn files_without_a_usable_header_are_skipped() {
        let repo = mother_repo();
        let dir = repo.path().join(GUIDES_DIR);
        write_guide(&dir, "no_frontmatter.md", "# Just notes\n");
        write_guide(&dir, "no_when.md", "---\nname: orphan\n---\nbody\n");
        write_guide(&dir, "notes.txt", "---\nwhen: ignored, not markdown\n---\n");

        assert!(discover(repo.path()).is_empty());
    }

    #[test]
    fn a_repository_without_guides_renders_the_empty_marker() {
        let repo = mother_repo();

        assert_eq!(index_for(repo.path()), NO_GUIDES);
    }

    #[test]
    fn a_worktree_sees_the_mothers_guides_by_absolute_path() {
        let mother = mother_repo();
        write_guide(
            &mother.path().join(GUIDES_DIR),
            "auth.md",
            "---\nwhen: authentication\n---\n",
        );
        let worktree = TempDir::new().unwrap();
        fs::write(
            worktree.path().join(".git"),
            format!(
                "gitdir: {}/.git/worktrees/feature\n",
                mother.path().display()
            ),
        )
        .unwrap();

        let guides = discover(worktree.path());

        assert_eq!(guides.len(), 1);
        assert_eq!(
            guides[0].display_path,
            mother
                .path()
                .join(GUIDES_DIR)
                .join("auth.md")
                .display()
                .to_string()
        );
    }

    #[test]
    fn a_worktree_guide_shadows_the_mothers_guide_of_the_same_name() {
        let mother = mother_repo();
        write_guide(
            &mother.path().join(GUIDES_DIR),
            "auth.md",
            "---\nname: auth\nwhen: the mother version\n---\n",
        );
        let worktree = TempDir::new().unwrap();
        fs::write(
            worktree.path().join(".git"),
            format!(
                "gitdir: {}/.git/worktrees/feature\n",
                mother.path().display()
            ),
        )
        .unwrap();
        write_guide(
            &worktree.path().join(GUIDES_DIR),
            "auth.md",
            "---\nname: auth\nwhen: the worktree version\n---\n",
        );

        let guides = discover(worktree.path());

        assert_eq!(guides.len(), 1);
        assert_eq!(guides[0].when, "the worktree version");
        assert_eq!(guides[0].display_path, ".wisetree/guides/auth.md");
    }

    #[test]
    fn a_detached_git_pointer_resolves_to_no_mother() {
        let worktree = TempDir::new().unwrap();
        File::create(worktree.path().join(".git")).unwrap();
        fs::write(worktree.path().join(".git"), "gitdir: /somewhere/odd\n").unwrap();

        assert!(discover(worktree.path()).is_empty());
    }

    #[test]
    fn the_index_is_capped_and_sorted_by_name() {
        let repo = mother_repo();
        let dir = repo.path().join(GUIDES_DIR);
        for i in 0..MAX_GUIDES + 5 {
            write_guide(&dir, &format!("g{i:02}.md"), "---\nwhen: something\n---\n");
        }

        let guides = discover(repo.path());

        assert_eq!(guides.len(), MAX_GUIDES);
        assert_eq!(guides[0].name, "g00");
        assert_eq!(
            guides[MAX_GUIDES - 1].name,
            format!("g{:02}", MAX_GUIDES - 1)
        );
    }

    #[test]
    fn oversized_header_fields_are_truncated() {
        let repo = mother_repo();
        write_guide(
            &repo.path().join(GUIDES_DIR),
            "huge.md",
            &format!("---\nwhen: {}\n---\n", "é".repeat(MAX_WHEN_BYTES)),
        );

        let guides = discover(repo.path());

        assert!(guides[0].when.ends_with('…'));
        assert!(guides[0].when.len() <= MAX_WHEN_BYTES + '…'.len_utf8());
    }

    #[test]
    fn age_is_humanized_by_magnitude() {
        assert_eq!(humanize_age(0), "updated today");
        assert_eq!(humanize_age(1), "updated 1 day ago");
        assert_eq!(humanize_age(12), "updated 12 days ago");
        assert_eq!(humanize_age(90), "updated 3 months ago");
    }
}
