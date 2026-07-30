//! Deterministic relationship graph and focus-budget grouping for PR review.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

const STRONG_EDGE: u8 = 50;
const HUB_DEGREE_LIMIT: usize = 8;

#[derive(Debug, Clone)]
pub(crate) struct ReviewRouteFile<'a> {
    pub path: &'a str,
    pub evidence: &'a str,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewRouteGroup {
    pub indices: Vec<usize>,
    pub cross_group_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Edge {
    id: String,
    left: usize,
    right: usize,
    kind: &'static str,
    strength: u8,
    uncertain: bool,
}

pub(crate) fn relationship_groups(
    files: &[ReviewRouteFile<'_>],
    focus_bytes: usize,
) -> Vec<ReviewRouteGroup> {
    if files.is_empty() {
        return Vec::new();
    }
    let edges = build_edges(files);
    let mut degrees = vec![0usize; files.len()];
    for edge in &edges {
        if edge.strength >= STRONG_EDGE {
            degrees[edge.left] += 1;
            degrees[edge.right] += 1;
        }
    }
    let active = edges
        .iter()
        .filter(|edge| {
            edge.strength >= STRONG_EDGE
                && ((degrees[edge.left] <= HUB_DEGREE_LIMIT
                    && degrees[edge.right] <= HUB_DEGREE_LIMIT)
                    || edge.strength >= 80)
        })
        .collect::<Vec<_>>();

    let mut adjacency = vec![Vec::new(); files.len()];
    for edge in &active {
        adjacency[edge.left].push(edge.right);
        adjacency[edge.right].push(edge.left);
    }
    for neighbors in &mut adjacency {
        neighbors.sort_unstable();
        neighbors.dedup();
    }

    let mut visited = vec![false; files.len()];
    let mut raw_groups = Vec::<Vec<usize>>::new();
    let mut current = Vec::new();
    let mut bytes = 0usize;
    for start in 0..files.len() {
        if visited[start] {
            continue;
        }
        let mut queue = VecDeque::from([start]);
        visited[start] = true;
        let mut component = Vec::new();
        while let Some(index) = queue.pop_front() {
            component.push(index);
            for &neighbor in &adjacency[index] {
                if !visited[neighbor] {
                    visited[neighbor] = true;
                    queue.push_back(neighbor);
                }
            }
        }
        component.sort_unstable();
        for index in component {
            let next = files[index].bytes;
            if !current.is_empty() && bytes.saturating_add(next) > focus_bytes {
                raw_groups.push(std::mem::take(&mut current));
                bytes = 0;
            }
            current.push(index);
            bytes = bytes.saturating_add(next);
            if next > focus_bytes {
                raw_groups.push(std::mem::take(&mut current));
                bytes = 0;
            }
        }
    }
    if !current.is_empty() {
        raw_groups.push(current);
    }

    let mut owner = vec![usize::MAX; files.len()];
    for (group_index, group) in raw_groups.iter().enumerate() {
        for index in group {
            owner[*index] = group_index;
        }
    }
    raw_groups
        .into_iter()
        .enumerate()
        .map(|(group_index, indices)| {
            let mut summaries = BTreeSet::new();
            for edge in &edges {
                if owner[edge.left] == owner[edge.right]
                    || (owner[edge.left] != group_index && owner[edge.right] != group_index)
                {
                    continue;
                }
                let other = if owner[edge.left] == group_index {
                    edge.right
                } else {
                    edge.left
                };
                let local = if other == edge.right {
                    edge.left
                } else {
                    edge.right
                };
                let certainty = if edge.uncertain { "uncertain " } else { "" };
                summaries.insert(format!(
                    "- {}: `{}` ↔ `{}` ({certainty}{})",
                    edge.id, files[local].path, files[other].path, edge.kind
                ));
            }
            ReviewRouteGroup {
                indices,
                cross_group_summary: summaries.into_iter().collect::<Vec<_>>().join("\n"),
            }
        })
        .collect()
}

fn build_edges(files: &[ReviewRouteFile<'_>]) -> Vec<Edge> {
    let nodes = files.iter().map(Node::new).collect::<Vec<_>>();
    let mut edges = Vec::new();
    for left in 0..nodes.len() {
        for right in left + 1..nodes.len() {
            if let Some((kind, strength, uncertain)) = relationship(&nodes[left], &nodes[right]) {
                let hash = blake3::hash(
                    format!("{}\0{}\0{kind}", files[left].path, files[right].path).as_bytes(),
                );
                edges.push(Edge {
                    id: format!("edge:{}", &hash.to_hex()[..12]),
                    left,
                    right,
                    kind,
                    strength,
                    uncertain,
                });
            }
        }
    }
    edges
}

struct Node {
    aliases: BTreeSet<String>,
    directory: String,
    evidence: String,
    identifiers: BTreeSet<String>,
    domain_tokens: BTreeSet<String>,
    role: &'static str,
}

impl Node {
    fn new(file: &ReviewRouteFile<'_>) -> Self {
        let path = file.path.replace('\\', "/").to_ascii_lowercase();
        let stem = Path::new(&path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string();
        let directory = Path::new(&path)
            .parent()
            .and_then(Path::to_str)
            .unwrap_or_default()
            .to_string();
        let evidence = file.evidence.to_ascii_lowercase();
        let mut aliases = BTreeSet::from([stem.clone()]);
        for line in evidence.lines() {
            if let Some(previous) = line.strip_prefix("rename from ") {
                if let Some(alias) = Path::new(previous.trim())
                    .file_stem()
                    .and_then(|value| value.to_str())
                {
                    aliases.insert(alias.to_string());
                }
            }
        }
        let identifiers = identifiers(file.evidence);
        let (domain_tokens, role) = domain_tokens(&stem);
        Self {
            aliases,
            directory,
            evidence,
            identifiers,
            domain_tokens,
            role,
        }
    }
}

fn relationship(left: &Node, right: &Node) -> Option<(&'static str, u8, bool)> {
    let left_ref = left
        .aliases
        .iter()
        .any(|alias| meaningful_stem(alias) && mentions(&right.evidence, alias));
    let right_ref = right
        .aliases
        .iter()
        .any(|alias| meaningful_stem(alias) && mentions(&left.evidence, alias));
    if left_ref || right_ref {
        let evidence = if left_ref {
            &right.evidence
        } else {
            &left.evidence
        };
        let uncertain = evidence.contains("import(") || evidence.contains("require(");
        return Some(("import/call/module reference", 100, uncertain));
    }

    let shared_domain = left
        .domain_tokens
        .intersection(&right.domain_tokens)
        .next()
        .is_some();
    if shared_domain && (left.role != "plain" || right.role != "plain") {
        let kind = if matches!(left.role, "schema" | "model" | "migration" | "config")
            || matches!(right.role, "schema" | "model" | "migration" | "config")
        {
            "schema/configuration consumer"
        } else if left.role == "test" || right.role == "test" {
            "implementation-to-test"
        } else {
            "cross-layer domain"
        };
        return Some((kind, 85, false));
    }

    if left
        .identifiers
        .intersection(&right.identifiers)
        .next()
        .is_some()
    {
        return Some(("shared type/constant", 60, false));
    }

    if left.directory == right.directory {
        return Some(("directory proximity only", 5, false));
    }
    None
}

fn meaningful_stem(stem: &str) -> bool {
    !matches!(
        stem,
        "" | "lib" | "main" | "mod" | "index" | "util" | "utils" | "common"
    )
}

fn mentions(evidence: &str, stem: &str) -> bool {
    evidence.contains(stem)
        || evidence.contains(&stem.replace('_', "::"))
        || evidence.contains(&stem.replace('_', "-"))
}

fn identifiers(evidence: &str) -> BTreeSet<String> {
    evidence
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|word| word.len() >= 4)
        .filter(|word| {
            word.chars().next().is_some_and(char::is_uppercase)
                || word
                    .chars()
                    .all(|character| character.is_uppercase() || character == '_')
        })
        .filter(|word| !matches!(*word, "FILE" | "CHANGE" | "SYMBOL" | "LINE"))
        .map(str::to_string)
        .collect()
}

fn domain_tokens(stem: &str) -> (BTreeSet<String>, &'static str) {
    let roles = BTreeMap::from([
        ("schema", "schema"),
        ("model", "model"),
        ("migration", "migration"),
        ("config", "config"),
        ("controller", "controller"),
        ("service", "service"),
        ("worker", "worker"),
        ("consumer", "consumer"),
        ("handler", "handler"),
        ("test", "test"),
        ("spec", "test"),
    ]);
    let mut role = "plain";
    let tokens = stem
        .split(['_', '-', '.'])
        .filter(|token| {
            if let Some(found) = roles.get(*token) {
                role = found;
                false
            } else {
                token.len() >= 3
            }
        })
        .map(str::to_string)
        .collect();
    (tokens, role)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file<'a>(path: &'a str, evidence: &'a str, bytes: usize) -> ReviewRouteFile<'a> {
        ReviewRouteFile {
            path,
            evidence,
            bytes,
        }
    }

    #[test]
    fn groups_cross_directory_references_before_filling_the_remaining_budget() {
        let files = [
            file(
                "api/user_controller.rs",
                "use crate::domain::user_service;",
                10,
            ),
            file("domain/user_service.rs", "pub struct UserService;", 10),
            file("api/unrelated.rs", "pub fn health() {}", 10),
        ];
        let groups = relationship_groups(&files, 20);
        assert_eq!(groups[0].indices, vec![0, 1]);
        assert_eq!(groups[1].indices, vec![2]);
    }

    #[test]
    fn connects_schema_model_service_controller_chain_and_cycles() {
        let files = [
            file("db/user_schema.rs", "UserRecord", 10),
            file("models/user_model.rs", "UserRecord UserService", 10),
            file("services/user_service.rs", "UserModel UserController", 10),
            file("api/user_controller.rs", "UserService UserSchema", 10),
        ];
        assert_eq!(
            relationship_groups(&files, 100)[0].indices,
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn oversized_components_split_with_stable_cross_group_edges() {
        let files = [
            file("a/user_model.rs", "UserService", 8),
            file("b/user_service.rs", "UserModel UserController", 8),
            file("c/user_controller.rs", "UserService", 8),
        ];
        let first = relationship_groups(&files, 10);
        let second = relationship_groups(&files, 10);
        assert_eq!(first, second);
        assert_eq!(first.len(), 3);
        assert!(first
            .iter()
            .all(|group| group.cross_group_summary.contains("edge:")));
    }

    #[test]
    fn unresolved_dynamic_import_is_visible_as_uncertain() {
        let files = [
            file("plugins/payment.rs", "pub fn payment() {}", 10),
            file("runtime/loader.rs", "import(payment_name)", 10),
        ];
        let groups = relationship_groups(&files, 5);
        assert!(groups
            .iter()
            .any(|group| group.cross_group_summary.contains("uncertain")));
    }

    #[test]
    fn disconnected_file_stays_on_focused_path() {
        let groups = relationship_groups(&[file("src/one.rs", "fn one() {}", 10)], 100);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].indices, vec![0]);
        assert!(groups[0].cross_group_summary.is_empty());
    }

    #[test]
    fn packs_unrelated_files_to_the_focus_budget_without_dropping_tail_files() {
        let files = [
            file("src/one.rs", "", 25),
            file("src/two.rs", "", 25),
            file("src/three.rs", "", 25),
            file("src/tail.rs", "", 25),
        ];
        let groups = relationship_groups(&files, 75);
        assert_eq!(groups[0].indices, vec![0, 1, 2]);
        assert_eq!(groups[1].indices, vec![3]);
        assert_eq!(
            groups
                .into_iter()
                .flat_map(|group| group.indices)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn high_degree_utility_does_not_merge_weak_neighbors() {
        let mut owned = Vec::new();
        for index in 0..12 {
            owned.push((format!("src/feature_{index}.rs"), "SharedThing".to_string()));
        }
        let files = owned
            .iter()
            .map(|(path, evidence)| file(path, evidence, 60))
            .collect::<Vec<_>>();
        let groups = relationship_groups(&files, 100);
        assert!(groups.len() > 1);
    }

    #[test]
    fn rename_history_keeps_old_consumers_connected_for_partial_migrations() {
        let files = [
            file(
                "services/account_service.rs",
                "rename from services/user_service.rs\nrename to services/account_service.rs",
                10,
            ),
            file(
                "api/user_controller.rs",
                "use crate::services::user_service;",
                10,
            ),
        ];
        let groups = relationship_groups(&files, 100);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].indices, vec![0, 1]);
    }
}
