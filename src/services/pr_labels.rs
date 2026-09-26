//! The repository's own label catalog, shared by the Explain and Split PR
//! commands.
//!
//! GitHub refuses `--label`/`--add-label` for a label the repository does not
//! have, which used to abort the whole pull request: the prompts listed a
//! hard-coded set of labels the AI then picked from, whether or not this
//! repository actually had them. The catalog is now fetched deterministically
//! (`gh label list`), rendered into the prompt in place of that fixed list, and
//! every AI choice is resolved back against it before it reaches `gh`.

use serde::Deserialize;

/// Labels whose name contains one of these (case-insensitive) describe the
/// change itself rather than its process state, so they lead the rendered
/// catalog and the prompt names them as the preferred picks.
pub const PRIORITY_SUBSTRINGS: [&str; 4] = ["user story", "bug", "technical debt", "documentation"];

/// How many labels reach the prompt. Priority labels are rendered first, so
/// this only clips the long tail of process labels on repositories that keep
/// dozens of them, and keeps the prompt from growing with the label list.
const MAX_CATALOG_LABELS: usize = 30;

/// How many labels one pull request gets, however many the AI returns.
const MAX_SELECTED_LABELS: usize = 2;

#[derive(Deserialize)]
struct GhLabel {
    #[serde(default)]
    name: String,
}

/// Parse `gh label list --json name` output into label names, in the order
/// GitHub returned them.
pub fn parse_label_names(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<GhLabel>>(json)
        .unwrap_or_default()
        .into_iter()
        .map(|label| label.name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Render the catalog for substitution into a prompt: priority labels first,
/// one per line, clipped to [`MAX_CATALOG_LABELS`]. An empty catalog (no `gh`,
/// no labels, or a failed fetch) renders the instruction to pick none, so the
/// AI never invents one from memory.
pub fn render_label_catalog(catalog: &[String]) -> String {
    if catalog.is_empty() {
        return "This repository's labels could not be read, so select no labels at all."
            .to_string();
    }
    let mut names = catalog.iter().collect::<Vec<_>>();
    names.sort_by_key(|name| priority_rank(name));
    let list = names
        .into_iter()
        .take(MAX_CATALOG_LABELS)
        .map(|name| format!("- {name}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("This repository's labels, the only allowed values:\n{list}")
}

/// Keep only the choices this repository can actually apply, spelled the way
/// GitHub spells them, capped at [`MAX_SELECTED_LABELS`]. Anything that does
/// not match a catalog entry is discarded rather than failing the pull request.
pub fn resolve_labels(chosen: &[String], catalog: &[String]) -> Vec<String> {
    let mut resolved: Vec<String> = Vec::new();
    for want in chosen {
        if want.trim().is_empty() {
            continue;
        }
        let Some(canonical) = catalog.iter().find(|name| same_label(name, want)) else {
            continue;
        };
        if !resolved.contains(canonical) {
            resolved.push(canonical.clone());
        }
        if resolved.len() == MAX_SELECTED_LABELS {
            break;
        }
    }
    resolved
}

/// Two spellings of one label. Comparing on alphanumerics alone resolves an
/// emoji or a case the AI added or dropped in either direction, so `bug 🐛`
/// and `bug` are the same label whichever side of the comparison carries the
/// emoji. A label written only in emoji has no alphanumerics left to compare,
/// so it falls back to its exact text.
fn same_label(a: &str, b: &str) -> bool {
    let (left, right) = (normalize(a), normalize(b));
    if left.is_empty() || right.is_empty() {
        return a.trim() == b.trim();
    }
    left == right
}

/// Position of the first [`PRIORITY_SUBSTRINGS`] entry the name contains, or
/// one past the list for a label that matches none of them.
fn priority_rank(name: &str) -> usize {
    let lowercase = name.to_lowercase();
    PRIORITY_SUBSTRINGS
        .iter()
        .position(|substring| lowercase.contains(substring))
        .unwrap_or(PRIORITY_SUBSTRINGS.len())
}

/// Lowercase alphanumerics only: `bug 🐛`, `Bug`, and `bug` all normalize to
/// `bug`, while `bugfix` stays distinct.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_label_names_reads_gh_json_and_drops_blanks() {
        let json = r#"[{"name":"bug 🐛"},{"name":"  "},{"name":" user story 💬 "}]"#;
        assert_eq!(
            parse_label_names(json),
            vec!["bug 🐛".to_string(), "user story 💬".to_string()]
        );
    }

    #[test]
    fn parse_label_names_returns_empty_for_unusable_output() {
        assert!(parse_label_names("gh: command failed").is_empty());
        assert!(parse_label_names("[]").is_empty());
    }

    #[test]
    fn render_label_catalog_leads_with_priority_labels() {
        let catalog = vec![
            "WIP 🚧".to_string(),
            "documentation 📖".to_string(),
            "bug 🐛".to_string(),
            "user story 💬".to_string(),
            "technical debt 🛠️".to_string(),
            "blocked 🚫".to_string(),
        ];
        let rendered = render_label_catalog(&catalog);
        let order = rendered
            .lines()
            .skip(1)
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                "- user story 💬",
                "- bug 🐛",
                "- technical debt 🛠️",
                "- documentation 📖",
                "- WIP 🚧",
                "- blocked 🚫",
            ]
        );
    }

    #[test]
    fn render_label_catalog_clips_long_label_lists() {
        let mut catalog = (0..MAX_CATALOG_LABELS + 5)
            .map(|index| format!("process-{index}"))
            .collect::<Vec<_>>();
        catalog.push("bug 🐛".to_string());
        let rendered = render_label_catalog(&catalog);
        assert_eq!(rendered.lines().count(), MAX_CATALOG_LABELS + 1);
        assert!(rendered.contains("- bug 🐛"));
    }

    #[test]
    fn render_label_catalog_tells_the_ai_to_pick_none_when_unknown() {
        assert_eq!(
            render_label_catalog(&[]),
            "This repository's labels could not be read, so select no labels at all."
        );
    }

    #[test]
    fn resolve_labels_canonicalizes_and_drops_unknown_choices() {
        let catalog = vec![
            "user story 💬".to_string(),
            "bug 🐛".to_string(),
            "technical debt 🛠️".to_string(),
        ];
        assert_eq!(
            resolve_labels(
                &[
                    "bug".to_string(),
                    "enhancement".to_string(),
                    "User Story 💬".to_string(),
                ],
                &catalog
            ),
            vec!["bug 🐛".to_string(), "user story 💬".to_string()]
        );
    }

    /// The emoji can be missing on either side: the AI may remember `bug 🐛`
    /// from a repository that only has `bug`, or write `bug` where the label is
    /// `bug 🐛`. Both resolve, and `gh` always receives GitHub's spelling.
    #[test]
    fn resolve_labels_matches_whichever_side_carries_the_emoji() {
        assert_eq!(
            resolve_labels(&["bug 🐛".to_string()], &["bug".to_string()]),
            vec!["bug".to_string()]
        );
        assert_eq!(
            resolve_labels(&["bug".to_string()], &["bug 🐛".to_string()]),
            vec!["bug 🐛".to_string()]
        );
        assert_eq!(
            resolve_labels(
                &["Technical Debt".to_string()],
                &["technical debt 🛠️".to_string()]
            ),
            vec!["technical debt 🛠️".to_string()]
        );
    }

    /// A label with no letters or digits at all can only be matched verbatim.
    #[test]
    fn resolve_labels_matches_an_emoji_only_label_exactly() {
        let catalog = vec!["🐛".to_string()];
        assert_eq!(
            resolve_labels(&["🐛".to_string()], &catalog),
            vec!["🐛".to_string()]
        );
        assert!(resolve_labels(&["bug".to_string()], &catalog).is_empty());
    }

    #[test]
    fn resolve_labels_dedupes_and_caps_the_selection() {
        let catalog = vec![
            "bug 🐛".to_string(),
            "user story 💬".to_string(),
            "documentation 📖".to_string(),
        ];
        assert_eq!(
            resolve_labels(
                &[
                    "bug 🐛".to_string(),
                    "bug".to_string(),
                    "documentation 📖".to_string(),
                    "user story 💬".to_string(),
                ],
                &catalog
            ),
            vec!["bug 🐛".to_string(), "documentation 📖".to_string()]
        );
    }

    #[test]
    fn resolve_labels_returns_nothing_without_a_catalog() {
        assert!(resolve_labels(&["bug 🐛".to_string()], &[]).is_empty());
    }

    #[test]
    fn resolve_labels_does_not_match_a_different_label_by_prefix() {
        let catalog = vec!["bugfix 🐛".to_string()];
        assert!(resolve_labels(&["bug".to_string()], &catalog).is_empty());
    }
}
