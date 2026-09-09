//! Deterministic contracts for Split preflight and semantic planning.
//!
//! Git and GitHub I/O remains in `DashboardService`. This module inventories
//! an immutable diff, renders the planning prompt, strictly validates the AI's
//! semantic assignment, and renders the harness-owned durable artifact.

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::errors::{Result, WisetreeError};

pub const SPLIT_DIRECTORY: &str = ".wisetree";
pub const SPLIT_PLAN_FILE: &str = ".wisetree/split_plan.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPreflightRequest {
    pub worktree_path: String,
    pub source_branch: String,
    pub pr_number: Option<u64>,
    pub pr_base_ref: Option<String>,
    pub max: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitIdentity {
    pub repository: String,
    pub remote: String,
    pub base_ref: String,
    pub base_sha: String,
    pub source_branch: String,
    pub source_head: String,
    pub max: u64,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeUnitKind {
    TextHunk,
    FileChange,
    Rename,
    ModeChange,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeUnit {
    pub id: String,
    pub path: String,
    pub old_path: Option<String>,
    pub kind: ChangeUnitKind,
    pub old_start: Option<u64>,
    pub old_lines: Option<u64>,
    pub new_start: Option<u64>,
    pub new_lines: Option<u64>,
    pub additions: u64,
    pub deletions: u64,
    pub line_counts_available: bool,
}

impl ChangeUnit {
    pub fn changed_lines(&self) -> u64 {
        self.additions.saturating_add(self.deletions)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPreflight {
    pub worktree_path: String,
    pub identity: SplitIdentity,
    pub units: Vec<ChangeUnit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitResponsibility {
    pub order: usize,
    pub name: String,
    pub branch_slug: String,
    pub rationale: String,
    pub units: Vec<String>,
    pub test_units: Vec<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitPlan {
    pub responsibilities: Vec<SplitResponsibility>,
}

pub fn inventory_diff(diff: &str) -> Result<Vec<ChangeUnit>> {
    let mut pending = Vec::new();
    for block in diff.split("diff --git ").skip(1) {
        let path = diff_path(block).ok_or_else(|| {
            WisetreeError::validation("Split could not identify a path in the committed diff.")
        })?;
        let old_path = block
            .lines()
            .find_map(|line| line.strip_prefix("rename from "))
            .map(unquote_git_path);
        let renamed_path = block
            .lines()
            .find_map(|line| line.strip_prefix("rename to "))
            .map(unquote_git_path);
        let path = renamed_path.unwrap_or(path);
        let is_binary = block
            .lines()
            .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch");
        let is_rename = old_path.is_some();
        let is_mode = block
            .lines()
            .any(|line| line.starts_with("old mode ") || line.starts_with("new mode "));
        let hunks = parse_hunks(block);
        if is_binary || is_rename || is_mode || hunks.is_empty() {
            let (additions, deletions) = hunks
                .iter()
                .fold((0u64, 0u64), |(a, d), h| (a + h.4, d + h.5));
            pending.push(ChangeUnit {
                id: String::new(),
                path,
                old_path,
                kind: if is_binary {
                    ChangeUnitKind::Binary
                } else if is_rename {
                    ChangeUnitKind::Rename
                } else if is_mode {
                    ChangeUnitKind::ModeChange
                } else {
                    ChangeUnitKind::FileChange
                },
                old_start: None,
                old_lines: None,
                new_start: None,
                new_lines: None,
                additions,
                deletions,
                line_counts_available: !is_binary,
            });
        } else {
            for (old_start, old_lines, new_start, new_lines, additions, deletions) in hunks {
                pending.push(ChangeUnit {
                    id: String::new(),
                    path: path.clone(),
                    old_path: None,
                    kind: ChangeUnitKind::TextHunk,
                    old_start: Some(old_start),
                    old_lines: Some(old_lines),
                    new_start: Some(new_start),
                    new_lines: Some(new_lines),
                    additions,
                    deletions,
                    line_counts_available: true,
                });
            }
        }
    }
    for (index, unit) in pending.iter_mut().enumerate() {
        unit.id = format!("CU{:04}", index + 1);
    }
    Ok(pending)
}

/// Sum the independent `git diff --numstat -z` stream. Binary records use
/// `-` and therefore contribute no line counts, matching Git's own totals.
pub fn parse_numstat_totals(numstat: &str) -> (u64, u64) {
    numstat
        .split('\0')
        .filter_map(|record| {
            let mut fields = record.split('\t');
            let additions = fields.next()?.parse::<u64>().ok()?;
            let deletions = fields.next()?.parse::<u64>().ok()?;
            Some((additions, deletions))
        })
        .fold((0u64, 0u64), |(a, d), (additions, deletions)| {
            (a + additions, d + deletions)
        })
}

type Hunk = (u64, u64, u64, u64, u64, u64);

fn parse_hunks(block: &str) -> Vec<Hunk> {
    let header =
        Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("static hunk regex");
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    for line in block.lines() {
        if let Some(captures) = header.captures(line) {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            let number = |index: usize, default: u64| {
                captures
                    .get(index)
                    .and_then(|value| value.as_str().parse().ok())
                    .unwrap_or(default)
            };
            current = Some((number(1, 0), number(2, 1), number(3, 0), number(4, 1), 0, 0));
        } else if let Some(hunk) = current.as_mut() {
            if line.starts_with('+') {
                hunk.4 += 1;
            } else if line.starts_with('-') {
                hunk.5 += 1;
            }
        }
    }
    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    hunks
}

fn diff_path(block: &str) -> Option<String> {
    block
        .lines()
        .find_map(|line| line.strip_prefix("+++ "))
        .filter(|path| *path != "/dev/null")
        .map(|path| unquote_git_path(path.strip_prefix("b/").unwrap_or(path)))
        .or_else(|| {
            let header = block.lines().next()?;
            let marker = header.rfind(" b/")?;
            Some(unquote_git_path(&header[marker + 3..]))
        })
}

fn unquote_git_path(path: &str) -> String {
    let path = path.trim();
    if !(path.starts_with('"') && path.ends_with('"')) {
        return path.to_string();
    }
    let mut result = String::new();
    let mut chars = path[1..path.len() - 1].chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            result.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => result.push('\n'),
            Some('t') => result.push('\t'),
            Some('r') => result.push('\r'),
            Some('"') => result.push('"'),
            Some('\\') => result.push('\\'),
            Some(other) => result.push(other),
            None => result.push('\\'),
        }
    }
    result
}

pub fn validate_manifest(identity: &SplitIdentity, units: &[ChangeUnit]) -> Result<()> {
    if units.is_empty() {
        return Err(WisetreeError::validation(
            "Split requires a non-empty committed base-to-source diff.",
        ));
    }
    let (additions, deletions) = units.iter().fold((0u64, 0u64), |(a, d), unit| {
        (a + unit.additions, d + unit.deletions)
    });
    if (additions, deletions) != (identity.additions, identity.deletions) {
        return Err(WisetreeError::validation(format!(
            "Split manifest totals (+{additions} -{deletions}) do not match the source diff (+{} -{}).",
            identity.additions, identity.deletions
        )));
    }
    if let Some(unit) = units
        .iter()
        .find(|unit| unit.changed_lines() > identity.max)
    {
        return Err(WisetreeError::validation(format!(
            "Indivisible change {} for `{}` is {} lines (+{} -{}), exceeding MAX {}. No valid split can satisfy this limit.",
            unit.id,
            unit.path,
            unit.changed_lines(),
            unit.additions,
            unit.deletions,
            identity.max
        )));
    }
    Ok(())
}

pub fn build_plan_prompt(
    preflight: &SplitPreflight,
    previous_proposal: Option<&str>,
    feedback: Option<&str>,
) -> String {
    let revision = previous_proposal.zip(feedback);
    let manifest = preflight
        .units
        .iter()
        .map(|unit| {
            let range = match (
                unit.old_start,
                unit.old_lines,
                unit.new_start,
                unit.new_lines,
            ) {
                (Some(os), Some(ol), Some(ns), Some(nl)) => format!(" old={os},{ol} new={ns},{nl}"),
                _ => String::new(),
            };
            let counts = if unit.line_counts_available {
                format!("+{} -{}", unit.additions, unit.deletions)
            } else {
                "binary-counts-unavailable".to_string()
            };
            format!(
                "{} | {:?} | {} | {}{}",
                unit.id, unit.kind, unit.path, counts, range
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let identity = &preflight.identity;
    include_str!("../../prompts/split_plan.md")
        .replace("REPOSITORY", &identity.repository)
        .replace("REMOTE", &identity.remote)
        .replace("BASE_REF", &identity.base_ref)
        .replace("BASE_SHA", &identity.base_sha)
        .replace("SOURCE_BRANCH", &identity.source_branch)
        .replace("SOURCE_HEAD", &identity.source_head)
        .replace("SOURCE_ADDITIONS", &identity.additions.to_string())
        .replace("SOURCE_DELETIONS", &identity.deletions.to_string())
        .replace("MAX", &identity.max.to_string())
        .replace("CHANGE_UNITS", &manifest)
        .replace(
            "PREVIOUS_PROPOSAL",
            revision.map(|(proposal, _)| proposal).unwrap_or(""),
        )
        .replace(
            "USER_FEEDBACK",
            revision.map(|(_, feedback)| feedback).unwrap_or(""),
        )
}

pub fn parse_split_plan(response: &str, preflight: &SplitPreflight) -> Result<SplitPlan> {
    let plan: SplitPlan = serde_json::from_str(response).map_err(|error| {
        WisetreeError::validation(format!(
            "Split plan must be exactly one JSON response matching the contract: {error}"
        ))
    })?;
    if plan.responsibilities.len() < 2 {
        return Err(WisetreeError::validation(
            "Split plan must contain at least two responsibilities.",
        ));
    }
    let slug = Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*$").expect("static slug regex");
    let manifest = preflight
        .units
        .iter()
        .map(|unit| (unit.id.as_str(), unit))
        .collect::<BTreeMap<_, _>>();
    let mut assigned = BTreeSet::new();
    let mut slugs = BTreeSet::new();
    let mut total_additions = 0u64;
    let mut total_deletions = 0u64;
    for (index, responsibility) in plan.responsibilities.iter().enumerate() {
        if responsibility.order != index + 1 {
            return Err(WisetreeError::validation(
                "Split responsibilities must be numbered consecutively from bottom to top.",
            ));
        }
        if responsibility.name.trim().is_empty()
            || responsibility.name.lines().count() != 1
            || responsibility.rationale.trim().is_empty()
            || responsibility.units.is_empty()
        {
            return Err(WisetreeError::validation(format!(
                "Split responsibility {} has an empty or invalid required field.",
                responsibility.order
            )));
        }
        if !slug.is_match(&responsibility.branch_slug)
            || responsibility.branch_slug.len() > 48
            || !slugs.insert(responsibility.branch_slug.as_str())
        {
            return Err(WisetreeError::validation(format!(
                "Split responsibility {} has an invalid or duplicate branch slug `{}`.",
                responsibility.order, responsibility.branch_slug
            )));
        }
        let mut layer_additions = 0u64;
        let mut layer_deletions = 0u64;
        let mut layer_paths = BTreeSet::new();
        for id in &responsibility.units {
            let unit = manifest.get(id.as_str()).ok_or_else(|| {
                WisetreeError::validation(format!(
                    "Split plan assigned unknown change unit `{id}`."
                ))
            })?;
            if !assigned.insert(id.as_str()) {
                return Err(WisetreeError::validation(format!(
                    "Split plan assigned change unit `{id}` more than once."
                )));
            }
            layer_additions += unit.additions;
            layer_deletions += unit.deletions;
            layer_paths.insert(unit.path.as_str());
        }
        let declared_paths = responsibility
            .paths
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if declared_paths != layer_paths || responsibility.paths.len() != declared_paths.len() {
            return Err(WisetreeError::validation(format!(
                "Split responsibility {} contains an unknown, duplicate, or missing path.",
                responsibility.order
            )));
        }
        if responsibility.test_units.is_empty() {
            return Err(WisetreeError::validation(format!(
                "Split responsibility {} is missing associated changed tests.",
                responsibility.order
            )));
        }
        let layer_ids = responsibility.units.iter().collect::<BTreeSet<_>>();
        let distinct_test_ids = responsibility.test_units.iter().collect::<BTreeSet<_>>();
        if distinct_test_ids.len() != responsibility.test_units.len() {
            return Err(WisetreeError::validation(format!(
                "Split responsibility {} contains a duplicate test change unit.",
                responsibility.order
            )));
        }
        for test_id in &responsibility.test_units {
            let unit = manifest.get(test_id.as_str()).ok_or_else(|| {
                WisetreeError::validation(format!(
                    "Split plan named unknown test unit `{test_id}`."
                ))
            })?;
            if !layer_ids.contains(test_id) || !is_test_path(&unit.path) {
                return Err(WisetreeError::validation(format!(
                    "Split responsibility {} identifies `{test_id}` as a test, but it is not an assigned test change.",
                    responsibility.order
                )));
            }
        }
        let changed = layer_additions.saturating_add(layer_deletions);
        if changed > preflight.identity.max {
            return Err(WisetreeError::validation(format!(
                "Split responsibility {} is {changed} lines (+{layer_additions} -{layer_deletions}), exceeding MAX {}.",
                responsibility.order, preflight.identity.max
            )));
        }
        total_additions += layer_additions;
        total_deletions += layer_deletions;
    }
    let expected = manifest.keys().copied().collect::<BTreeSet<_>>();
    if assigned != expected {
        let missing = expected.difference(&assigned).copied().collect::<Vec<_>>();
        return Err(WisetreeError::validation(format!(
            "Split plan omitted change units: {}.",
            missing.join(", ")
        )));
    }
    if (total_additions, total_deletions)
        != (preflight.identity.additions, preflight.identity.deletions)
    {
        return Err(WisetreeError::validation(
            "Split plan totals do not match the frozen source diff.",
        ));
    }
    Ok(plan)
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("tests/")
        || lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.ends_with("_test.rs")
        || lower.ends_with(".test.js")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".spec.js")
        || lower.ends_with(".spec.ts")
}

pub fn render_split_plan(preflight: &SplitPreflight, plan: &SplitPlan, status: &str) -> String {
    let identity = &preflight.identity;
    let mut output = format!(
        "# Split Plan\n\n## Input identity\n\n| Field | Value |\n| --- | --- |\n| Repository | `{}` |\n| Remote | `{}` |\n| Base | `{}` (`{}`) |\n| Source | `{}` (`{}`) |\n| MAX | {} changed lines |\n| Source total | +{} -{} = {} |\n\n## Lifecycle\n\nStatus: **{}**\n\n## Proposal (bottom to top)\n",
        identity.repository,
        identity.remote,
        identity.base_ref,
        identity.base_sha,
        identity.source_branch,
        identity.source_head,
        identity.max,
        identity.additions,
        identity.deletions,
        identity.additions + identity.deletions,
        status.trim()
    );
    let manifest = preflight
        .units
        .iter()
        .map(|unit| (unit.id.as_str(), unit))
        .collect::<BTreeMap<_, _>>();
    output.push_str("\n| Layer | Responsibility | Branch slug | Units | + | - | Total |\n| ---: | --- | --- | --- | ---: | ---: | ---: |\n");
    for layer in &plan.responsibilities {
        let (additions, deletions) = layer.units.iter().fold((0u64, 0u64), |(a, d), id| {
            let unit = manifest[id.as_str()];
            (a + unit.additions, d + unit.deletions)
        });
        output.push_str(&format!(
            "| {} | {} | `{}` | {} | {} | {} | {} |\n",
            layer.order,
            layer.name,
            layer.branch_slug,
            layer.units.join(", "),
            additions,
            deletions,
            additions + deletions
        ));
        output.push_str(&format!("\nDependency rationale: {}\n\n", layer.rationale));
    }
    output.push_str("## Deterministic integrity\n\n| Unit | Kind | Path | Old range | New range | + | - | Layer |\n| --- | --- | --- | --- | --- | ---: | ---: | ---: |\n");
    let owners = plan
        .responsibilities
        .iter()
        .flat_map(|layer| layer.units.iter().map(move |id| (id.as_str(), layer.order)))
        .collect::<BTreeMap<_, _>>();
    for unit in &preflight.units {
        let range = |start: Option<u64>, lines: Option<u64>| match (start, lines) {
            (Some(start), Some(lines)) => format!("{start},{lines}"),
            _ => "atomic".to_string(),
        };
        output.push_str(&format!(
            "| {} | {:?} | `{}` | {} | {} | {} | {} | {} |\n",
            unit.id,
            unit.kind,
            unit.path,
            range(unit.old_start, unit.old_lines),
            range(unit.new_start, unit.new_lines),
            unit.additions,
            unit.deletions,
            owners[unit.id.as_str()]
        ));
    }
    output
}
