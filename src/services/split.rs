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
pub const SPLIT_PLAN_ARCHIVE_PREFIX: &str = ".wisetree/split_plan.";
pub const SPLIT_DRAFT_DIRECTORY: &str = ".wisetree/split_drafts";
const MATERIALIZATION_MARKER: &str = "<!-- wisetree-split-materialization ";
const PUBLICATION_MARKER: &str = "<!-- wisetree-split-publication ";
const RUN_MARKER: &str = "<!-- wisetree-split-run ";
const DRAFTING_MARKER: &str = "<!-- wisetree-split-drafting ";
const SPLIT_RUN_VERSION: u32 = 1;

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
    /// Derived by [`parse_split_plan`] from the manifest, never trusted from
    /// the planning AI: it is recorded for the plan file and the review screen.
    #[serde(default)]
    pub test_units: Vec<String>,
    pub paths: Vec<String>,
    /// Proposed by the planning AI and demoted by [`parse_split_plan`] whenever
    /// the deterministic path-disjointness test fails. Stacked is the safe
    /// default: independence is a promotion that has to be proved.
    #[serde(default)]
    pub independent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitPlan {
    pub responsibilities: Vec<SplitResponsibility>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitRepositorySnapshot {
    pub status: String,
    pub head: String,
    pub refs: String,
    pub files: Vec<(String, Option<Vec<u8>>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlanResult {
    pub plan: SplitPlan,
    pub snapshot: SplitRepositorySnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitMaterializedLayer {
    pub order: usize,
    pub responsibility: String,
    pub branch: String,
    pub worktree_path: String,
    pub parent_branch: String,
    pub parent_sha: String,
    pub commit_sha: String,
    pub tree_sha: String,
    pub units: Vec<String>,
    pub additions: u64,
    pub deletions: u64,
    pub ready_for_publication: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitMaterialization {
    pub source_branch: String,
    pub source_head: String,
    pub base_sha: String,
    pub layers: Vec<SplitMaterializedLayer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitPublishedPullRequest {
    pub order: usize,
    pub branch: String,
    pub expected_base: String,
    pub number: u64,
    pub url: String,
    pub provisional_title: String,
    pub provisional_title_applied: bool,
    /// Mirrors the approved plan: this pull request targets the trunk and is
    /// mergeable without any sibling in the split.
    #[serde(default)]
    pub independent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitPublication {
    pub repository: String,
    pub trunk: String,
    pub source_branch: String,
    pub stack_link_completed: bool,
    pub status: String,
    pub diagnostics: Option<String>,
    pub pull_requests: Vec<SplitPublishedPullRequest>,
}

/// The deliberately small contract returned by one Split drafting call.
/// Stack bookkeeping and template assembly remain harness-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitDraft {
    pub title_summary: String,
    pub body_content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitDraftRecord {
    pub job_id: String,
    pub source_head: String,
    pub order: usize,
    pub pr_number: u64,
    pub pr_url: String,
    pub correction_attempted: bool,
    pub draft: Option<SplitDraft>,
    pub final_title: Option<String>,
    pub final_body: Option<String>,
    pub applied: bool,
    pub error: Option<String>,
}

/// Machine-readable resume identity embedded in the human-readable plan.
/// Materialization and publication have their own records because they are
/// updated independently after approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitRunRecord {
    pub version: u32,
    pub identity: SplitIdentity,
    pub units: Vec<ChangeUnit>,
    pub plan: SplitPlan,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitDraftingRecord {
    pub records: Vec<SplitDraftRecord>,
    pub completed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDraftJobStatus {
    Pending,
    Drafting,
    Correcting,
    Drafted,
    Applying,
    Applied,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitDraftProgress {
    pub operation_id: u64,
    pub generation: u64,
    pub layer: usize,
    pub pr_number: u64,
    pub status: SplitDraftJobStatus,
    pub activity: Option<String>,
    pub error: Option<String>,
}

pub fn split_draft_job_id(source_head: &str, order: usize, pr_number: u64) -> String {
    let short_head = source_head.chars().take(12).collect::<String>();
    format!("{}-layer-{order}-pr-{pr_number}", short_head)
}

pub fn split_draft_cache_path(worktree: &str, job_id: &str) -> std::path::PathBuf {
    std::path::Path::new(worktree)
        .join(SPLIT_DRAFT_DIRECTORY)
        .join(format!("{job_id}.json"))
}

/// The base branch every published pull request must target, in plan order.
///
/// Publication and the gate that verifies it share this, because computing the
/// same chain twice is precisely how the two drifted apart.
pub fn split_publication_bases(plan: &SplitPlan, branches: &[String], trunk: &str) -> Vec<String> {
    let mut bases = vec![trunk.to_string(); plan.responsibilities.len()];
    for chain in split_chains(plan) {
        // The root of every chain targets the trunk; the rest stack on the
        // branch directly below them, whether or not the chain is long enough
        // for `gh stack link` to publish it.
        for pair in chain.windows(2) {
            if let (Some(base), Some(slot)) = (branches.get(pair[0]), bases.get_mut(pair[1])) {
                *slot = base.clone();
            }
        }
    }
    bases
}

pub fn validate_split_publication(
    preflight: &SplitPreflight,
    plan: &SplitPlan,
    publication: &SplitPublication,
) -> Result<()> {
    let total = plan.responsibilities.len();
    if !publication.stack_link_completed
        || publication.repository != preflight.identity.repository
        || publication.source_branch != preflight.identity.source_branch
        || publication.pull_requests.len() != total
        || publication.diagnostics.is_some()
        || !publication
            .status
            .starts_with("published, verified, and provisionally titled")
    {
        return Err(WisetreeError::validation(
            "Split drafting requires the complete persisted and verified publication.",
        ));
    }
    let mut numbers = BTreeSet::new();
    let mut urls = BTreeSet::new();
    let branches = publication
        .pull_requests
        .iter()
        .map(|pull_request| pull_request.branch.clone())
        .collect::<Vec<_>>();
    let expected_bases = split_publication_bases(plan, &branches, &publication.trunk);
    for (index, pull_request) in publication.pull_requests.iter().enumerate() {
        let expected_order = index + 1;
        let expected_base = expected_bases[index].as_str();
        let expected_url = format!(
            "https://github.com/{}/pull/{}",
            publication.repository, pull_request.number
        );
        if pull_request.order != expected_order
            || pull_request.expected_base != expected_base
            || pull_request.independent != plan.responsibilities[index].independent
            || pull_request.url != expected_url
            || pull_request.number == 0
            || !pull_request.provisional_title_applied
            || !numbers.insert(pull_request.number)
            || !urls.insert(pull_request.url.as_str())
        {
            return Err(WisetreeError::validation(format!(
                "Split PR layer {expected_order} is unresolved, unordered, or unverified."
            )));
        }
    }
    if publication
        .pull_requests
        .iter()
        .any(|pull_request| pull_request.branch == preflight.identity.source_branch)
    {
        return Err(WisetreeError::validation(
            "Split drafting cannot reuse the source branch for a generated pull request.",
        ));
    }
    Ok(())
}

pub fn build_split_open_prompt(
    responsibility: &SplitResponsibility,
    ticket: &str,
    commit_log: &str,
    diff: &str,
    template: &str,
) -> String {
    include_str!("../../prompts/split_open.md")
        .replace("RESPONSIBILITY", &responsibility.name)
        .replace("RATIONALE", &responsibility.rationale)
        .replace("TICKET", ticket)
        .replace("COMMIT_LOG", commit_log)
        .replace("VERIFIED_DIFF", diff)
        .replace("PR_TEMPLATE", template)
}

pub fn build_corrective_split_open_prompt(prompt: &str, error: &str) -> String {
    format!(
        "{prompt}\n\nYour previous response failed the output contract: {}\nCorrect only that contract violation and return exactly the two-field JSON object.",
        error.trim()
    )
}

pub fn parse_split_draft(response: &str) -> Result<SplitDraft> {
    let draft: SplitDraft = serde_json::from_str(response).map_err(|error| {
        WisetreeError::validation(format!(
            "Split draft must be exactly one JSON object matching the contract: {error}"
        ))
    })?;
    let title = draft.title_summary.trim();
    let body = draft.body_content.trim();
    if title.is_empty() || title.lines().count() != 1 || body.is_empty() {
        return Err(WisetreeError::validation(
            "Split draft requires a non-empty one-line title_summary and body_content.",
        ));
    }
    let forbidden =
        Regex::new(r"(?i)https?://|###?\s*split plan").expect("static Split draft regex");
    if forbidden.is_match(title) || forbidden.is_match(body) {
        return Err(WisetreeError::validation(
            "Split draft must not contain links or the harness-owned Split Plan heading.",
        ));
    }
    if body
        .lines()
        .filter(|line| is_description_heading(line))
        .count()
        != 1
        || !body
            .lines()
            .find(|line| !line.trim().is_empty())
            .is_some_and(is_description_heading)
    {
        return Err(WisetreeError::validation(
            "Split body_content must begin with exactly one Description heading.",
        ));
    }
    Ok(SplitDraft {
        title_summary: title.to_string(),
        body_content: body.to_string(),
    })
}

pub fn final_split_title(
    source_branch: &str,
    title_summary: &str,
    order: usize,
    total: usize,
) -> Result<String> {
    if order == 0 || total == 0 || order > total {
        return Err(WisetreeError::validation(
            "Split title requires a valid immutable layer suffix.",
        ));
    }
    let numbering =
        Regex::new(r"(?i)^\s*(?:#+\s*)?(?:\d+[.):\-]\s*)?").expect("static numbering regex");
    let suffix = Regex::new(r"\s*\(\d+\s*/\s*\d+\)\s*").expect("static suffix regex");
    let mut summary = numbering.replace(title_summary, "").to_string();
    summary = suffix.replace_all(&summary, " ").to_string();
    let ticket = normalized_ticket(source_branch).or_else(|| normalized_ticket(title_summary));
    if let Some(ticket) = &ticket {
        let prefix = Regex::new(&format!(r"(?i)^{}\s*[:\-–—]?\s*", regex::escape(ticket)))
            .expect("escaped ticket regex");
        summary = prefix.replace(&summary, "").to_string();
    }
    summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if summary.is_empty() {
        return Err(WisetreeError::validation(
            "Split draft title is empty after deterministic normalization.",
        ));
    }
    let stem = ticket.map_or(summary.clone(), |ticket| format!("{ticket} {summary}"));
    Ok(format!("{stem} ({order}/{total})"))
}

pub fn compose_split_body(
    template: &str,
    filled_body: &str,
    pull_requests: &[SplitPublishedPullRequest],
    current_order: usize,
) -> Result<String> {
    if filled_body.trim().is_empty() || current_order == 0 || current_order > pull_requests.len() {
        return Err(WisetreeError::validation(
            "Split body requires description prose and a valid current PR.",
        ));
    }
    if Regex::new(r"(?i)https?://").unwrap().is_match(filled_body) {
        return Err(WisetreeError::validation(
            "Split AI description prose must not invent links.",
        ));
    }
    let lines = filled_body.lines().collect::<Vec<_>>();
    let descriptions = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_description_heading(line))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if descriptions.len() > 1 {
        return Err(WisetreeError::validation(
            "Split body_content must contain exactly one Description heading.",
        ));
    }
    if descriptions.len() != 1
        || !lines
            .iter()
            .copied()
            .find(|line| !line.trim().is_empty())
            .is_some_and(is_description_heading)
    {
        return Err(WisetreeError::validation(
            "Split body_content must begin with exactly one Description heading.",
        ));
    }
    if filled_body
        .lines()
        .filter(|line| {
            line.trim()
                .to_ascii_lowercase()
                .starts_with("### split plan")
        })
        .count()
        > 0
    {
        return Err(WisetreeError::validation(
            "Split body_content must not contain the harness-owned Split Plan subsection.",
        ));
    }
    validate_split_draft_template(
        template,
        &SplitDraft {
            title_summary: "validated separately".to_string(),
            body_content: filled_body.to_string(),
        },
    )?;
    let mut plan = String::from("### Split Plan 📋\n\n");
    let chains = published_chain_ids(pull_requests);
    for (index, pull_request) in pull_requests.iter().enumerate() {
        plan.push_str(&split_plan_entry(
            index,
            pull_request,
            current_order,
            &chains,
        ));
        plan.push('\n');
    }
    let description_index = descriptions[0];
    let after_description = lines[description_index + 1..].join("\n");
    let body = [
        "# Description ✍️".to_string(),
        plan.trim_end().to_string(),
        after_description.trim().to_string(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    validate_split_body(&body, pull_requests, current_order)?;
    Ok(format!("{}\n", body.trim_end()))
}

pub fn validate_split_draft_template(template: &str, draft: &SplitDraft) -> Result<()> {
    let lines = draft.body_content.lines().collect::<Vec<_>>();
    for heading in template.lines().filter(|line| {
        let trimmed = line.trim();
        trimmed.starts_with('#')
            && !is_description_heading(trimmed)
            && !trimmed.to_ascii_lowercase().contains("ticket")
    }) {
        if lines
            .iter()
            .filter(|line| line.trim() == heading.trim())
            .count()
            != 1
        {
            return Err(WisetreeError::validation(format!(
                "Split body_content must fill the template section `{}` exactly once.",
                heading.trim()
            )));
        }
    }
    for placeholder in template
        .lines()
        .map(str::trim)
        .filter(|line| looks_like_template_placeholder(line))
    {
        if lines.iter().any(|line| line.trim() == placeholder) {
            return Err(WisetreeError::validation(format!(
                "Split body_content left the template placeholder `{placeholder}` unchanged."
            )));
        }
    }
    Ok(())
}

fn looks_like_template_placeholder(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    !line.is_empty()
        && (line.contains("{{")
            || lower.contains("placeholder")
            || lower.contains("brief explanation")
            || lower.contains("overview of the feature")
            || lower.contains("step-by-step process")
            || lower == "- [ ] example"
            || lower == "todo")
}

/// Chain membership for each published pull request, derived from the same rule
/// [`split_chains`] uses: a chain starts at the first entry and at every
/// independent one, and runs contiguously from there.
fn published_chain_ids(pull_requests: &[SplitPublishedPullRequest]) -> Vec<usize> {
    let mut chain = 0;
    pull_requests
        .iter()
        .enumerate()
        .map(|(index, pull_request)| {
            if index > 0 && pull_request.independent {
                chain += 1;
            }
            chain
        })
        .collect()
}

/// One line of the harness-owned Split Plan list. Composition and validation
/// share it so the rendered body and the contract can never drift.
///
/// The marker states the relationship to the pull request being described. Only
/// work stacked above it in the *same* chain is "future": a pull request in
/// another chain targets the trunk on its own schedule, and one below it in the
/// same chain is a prerequisite rather than pending work.
fn split_plan_entry(
    index: usize,
    pull_request: &SplitPublishedPullRequest,
    current_order: usize,
    chains: &[usize],
) -> String {
    let number = index + 1;
    let same_chain = chains
        .get(index)
        .zip(chains.get(current_order.saturating_sub(1)))
        .is_some_and(|(entry, current)| entry == current);
    let marker = if number == current_order {
        " **(current PR)**"
    } else if !same_chain {
        " **(independent PR)**"
    } else if number > current_order {
        " **(future PR)**"
    } else {
        ""
    };
    format!("{number}. {}{marker}", pull_request.url)
}

pub fn validate_split_body(
    body: &str,
    pull_requests: &[SplitPublishedPullRequest],
    current_order: usize,
) -> Result<()> {
    let descriptions = body
        .lines()
        .filter(|line| is_description_heading(line))
        .count();
    let plans = body
        .lines()
        .filter(|line| line.trim() == "### Split Plan 📋")
        .count();
    if descriptions != 1 || plans != 1 {
        return Err(WisetreeError::validation(
            "Split body must contain exactly one Description and one Split Plan heading.",
        ));
    }
    let chains = published_chain_ids(pull_requests);
    for (index, pull_request) in pull_requests.iter().enumerate() {
        if body.matches(&pull_request.url).count() != 1 {
            return Err(WisetreeError::validation(format!(
                "Split body must contain PR URL {} exactly once.",
                pull_request.url
            )));
        }
        let expected = split_plan_entry(index, pull_request, current_order, &chains);
        if !body.lines().any(|line| line == expected) {
            return Err(WisetreeError::validation(
                "Split body contains an incorrect current/future marker or URL order.",
            ));
        }
    }
    Ok(())
}

fn normalized_ticket(branch: &str) -> Option<String> {
    let ticket = Regex::new(r"(?i)([a-z]+)-?(\d+)").expect("static ticket regex");
    let captures = ticket.captures(branch)?;
    Some(format!(
        "{}-{}",
        captures.get(1)?.as_str().to_ascii_uppercase(),
        captures.get(2)?.as_str()
    ))
}

fn is_h1_heading(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("# ") && !trimmed.starts_with("## ")
}

fn is_description_heading(line: &str) -> bool {
    let trimmed = line.trim();
    is_h1_heading(trimmed)
        && trimmed[2..]
            .trim()
            .to_ascii_lowercase()
            .starts_with("description")
}

pub fn split_branch_name(source: &str, order: usize, slug: &str) -> String {
    format!("{source}.{order}_{slug}")
}

/// Select complete atomic changes and individual zero-context hunks from the
/// frozen base-to-source diff. IDs are assigned with the same traversal used
/// by [`inventory_diff`], so the AI never handles patch text.
pub fn patch_for_units(diff: &str, selected: &BTreeSet<String>) -> Result<String> {
    let mut output = String::new();
    let mut next_id = 1usize;
    let mut found = BTreeSet::new();
    for raw_block in diff.split("diff --git ").skip(1) {
        let block = format!("diff --git {raw_block}");
        let hunk_offsets = block
            .match_indices("\n@@ ")
            .map(|(offset, _)| offset + 1)
            .collect::<Vec<_>>();
        let atomic = block.lines().any(|line| {
            line.starts_with("Binary files ")
                || line == "GIT binary patch"
                || line.starts_with("rename from ")
                || line.starts_with("old mode ")
                || line.starts_with("new mode ")
        }) || hunk_offsets.is_empty();
        if atomic {
            let id = format!("CU{next_id:04}");
            next_id += 1;
            if selected.contains(&id) {
                output.push_str(&block);
                if !block.ends_with('\n') {
                    output.push('\n');
                }
                found.insert(id);
            }
            continue;
        }

        let header_end = hunk_offsets[0];
        let mut chosen = Vec::new();
        for (index, start) in hunk_offsets.iter().copied().enumerate() {
            let end = hunk_offsets.get(index + 1).copied().unwrap_or(block.len());
            let id = format!("CU{next_id:04}");
            next_id += 1;
            if selected.contains(&id) {
                chosen.push(&block[start..end]);
                found.insert(id);
            }
        }
        if !chosen.is_empty() {
            output.push_str(&block[..header_end]);
            for hunk in chosen {
                output.push_str(hunk);
            }
            if !output.ends_with('\n') {
                output.push('\n');
            }
        }
    }
    if &found != selected {
        let missing = selected.difference(&found).cloned().collect::<Vec<_>>();
        return Err(WisetreeError::validation(format!(
            "Split could not reconstruct selected change units: {}.",
            missing.join(", ")
        )));
    }
    Ok(output)
}

/// Ignore object hashes and line offsets when comparing a selected source
/// patch with Git's parent-to-child rendering after earlier layers shifted
/// line numbers. All paths, metadata, and changed bytes remain significant.
pub fn normalized_patch(patch: &str) -> String {
    let normalized = patch
        .lines()
        .filter(|line| !line.starts_with("index "))
        .map(|line| {
            if line.starts_with("@@ ") {
                "@@".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    normalized.trim_end().to_string()
}

pub fn parse_materialization(document: &str) -> Result<Option<SplitMaterialization>> {
    let Some(start) = document.find(MATERIALIZATION_MARKER) else {
        return Ok(None);
    };
    let json_start = start + MATERIALIZATION_MARKER.len();
    let json_end = document[json_start..]
        .find(" -->")
        .map(|offset| json_start + offset)
        .ok_or_else(|| WisetreeError::validation("Split materialization record is truncated."))?;
    serde_json::from_str(&document[json_start..json_end])
        .map(Some)
        .map_err(Into::into)
}

fn parse_marker<T: for<'de> Deserialize<'de>>(
    document: &str,
    marker: &str,
    label: &str,
) -> Result<Option<T>> {
    let Some(start) = document.find(marker) else {
        return Ok(None);
    };
    let json_start = start + marker.len();
    let json_end = document[json_start..]
        .find(" -->")
        .map(|offset| json_start + offset)
        .ok_or_else(|| WisetreeError::validation(format!("Split {label} record is truncated.")))?;
    serde_json::from_str(&document[json_start..json_end])
        .map(Some)
        .map_err(|error| {
            WisetreeError::validation(format!("Split {label} record is corrupt: {error}"))
        })
}

pub fn parse_split_run(document: &str) -> Result<Option<SplitRunRecord>> {
    let record = parse_marker::<SplitRunRecord>(document, RUN_MARKER, "run")?;
    if let Some(record) = &record {
        if record.version != SPLIT_RUN_VERSION {
            return Err(WisetreeError::validation(format!(
                "Split run version {} is unsupported (expected {}). Start over only after reconciling the recorded artifacts.",
                record.version, SPLIT_RUN_VERSION
            )));
        }
    }
    Ok(record)
}

pub fn validate_split_resume(
    document: &str,
    preflight: &SplitPreflight,
) -> Result<Option<SplitRunRecord>> {
    let Some(record) = parse_split_run(document)? else {
        return Ok(None);
    };
    if record.identity != preflight.identity || record.units != preflight.units {
        return Err(WisetreeError::validation(
            "Persisted Split input does not match the live repository, source branch, source HEAD, base SHA, remote, MAX, or change inventory. Reconcile or archive .wisetree/split_plan.md before starting over; Wisetree did not touch recorded artifacts.",
        ));
    }
    parse_split_plan(&serde_json::to_string(&record.plan)?, preflight)?;
    Ok(Some(record))
}

pub fn parse_split_drafting(document: &str) -> Result<Option<SplitDraftingRecord>> {
    parse_marker(document, DRAFTING_MARKER, "drafting")
}

pub fn render_split_drafting(drafting: &SplitDraftingRecord) -> Result<String> {
    let mut output = String::from(
        "\n## PR metadata jobs\n\n| Layer | PR | Draft | Metadata update | Error |\n| ---: | ---: | --- | --- | --- |\n",
    );
    for record in &drafting.records {
        output.push_str(&format!(
            "| {} | #{} | {} | {} | {} |\n",
            record.order,
            record.pr_number,
            if record.draft.is_some() {
                "cached"
            } else {
                "pending"
            },
            if record.applied { "applied" } else { "pending" },
            record.error.as_deref().unwrap_or("")
        ));
    }
    output.push_str(&format!(
        "\nCompletion status: **{}**\n\n{DRAFTING_MARKER}{} -->\n",
        if drafting.completed {
            "complete"
        } else {
            "incomplete; retry resumes cached work"
        },
        serde_json::to_string(drafting)?
    ));
    Ok(output)
}

pub fn render_materialization(materialization: &SplitMaterialization) -> Result<String> {
    let mut output = String::from(
        "\n## Materialized stack (bottom to top)\n\n| Layer | Responsibility | Parent | Parent commit | Branch | Worktree | Commit | Tree | + | - | Ready |\n| ---: | --- | --- | --- | --- | --- | --- | --- | ---: | ---: | --- |\n",
    );
    for layer in &materialization.layers {
        output.push_str(&format!(
            "| {} | {} | `{}` | `{}` | `{}` | `{}` | `{}` | `{}` | {} | {} | {} |\n",
            layer.order,
            layer.responsibility,
            layer.parent_branch,
            layer.parent_sha,
            layer.branch,
            layer.worktree_path,
            layer.commit_sha,
            layer.tree_sha,
            layer.additions,
            layer.deletions,
            if layer.ready_for_publication {
                "yes"
            } else {
                "no"
            }
        ));
        output.push_str(&format!("\nAssigned units: {}\n", layer.units.join(", ")));
    }
    output.push_str(&format!(
        "\n{MATERIALIZATION_MARKER}{} -->\n",
        serde_json::to_string(materialization)?
    ));
    Ok(output)
}

pub fn parse_publication(document: &str) -> Result<Option<SplitPublication>> {
    let Some(start) = document.find(PUBLICATION_MARKER) else {
        return Ok(None);
    };
    let json_start = start + PUBLICATION_MARKER.len();
    let json_end = document[json_start..]
        .find(" -->")
        .map(|offset| json_start + offset)
        .ok_or_else(|| WisetreeError::validation("Split publication record is truncated."))?;
    serde_json::from_str(&document[json_start..json_end])
        .map(Some)
        .map_err(Into::into)
}

pub fn render_publication(publication: &SplitPublication) -> Result<String> {
    let mut output = String::from(
        "\n## Published stack (bottom to top)\n\n| Layer | Branch | Expected base | PR | URL | Provisional title | Applied |\n| ---: | --- | --- | ---: | --- | --- | --- |\n",
    );
    for pull_request in &publication.pull_requests {
        output.push_str(&format!(
            "| {} | `{}` | `{}` | #{} | {} | {} | {} |\n",
            pull_request.order,
            pull_request.branch,
            pull_request.expected_base,
            pull_request.number,
            pull_request.url,
            pull_request.provisional_title,
            if pull_request.provisional_title_applied {
                "yes"
            } else {
                "no"
            }
        ));
    }
    output.push_str(&format!(
        "\nPublication status: **{}**\n\nStack link completed: **{}**\n",
        publication.status,
        if publication.stack_link_completed {
            "yes"
        } else {
            "no"
        }
    ));
    if let Some(diagnostics) = &publication.diagnostics {
        output.push_str(&format!(
            "\nExact diagnostics:\n\n```text\n{diagnostics}\n```\n"
        ));
    }
    output.push_str(&format!(
        "\n{PUBLICATION_MARKER}{} -->\n",
        serde_json::to_string(publication)?
    ));
    Ok(output)
}

pub fn provisional_split_title(source_branch: &str, order: usize, total: usize) -> String {
    let ticket = Regex::new(r"(?i)([a-z]+)-?(\d+)").expect("static ticket regex");
    let (ticket, remainder) = if let Some(found) = ticket.captures(source_branch) {
        let complete = found.get(0).expect("complete regex capture");
        let prefix = found
            .get(1)
            .expect("ticket prefix")
            .as_str()
            .to_ascii_uppercase();
        let number = found.get(2).expect("ticket number").as_str();
        (
            Some(format!("{prefix}-{number}")),
            format!(
                "{} {}",
                &source_branch[..complete.start()],
                &source_branch[complete.end()..]
            ),
        )
    } else {
        (None, source_branch.to_string())
    };
    let summary = remainder
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut characters = word.chars();
            characters
                .next()
                .map(|first| {
                    format!(
                        "{}{}",
                        first.to_uppercase(),
                        characters.as_str().to_ascii_lowercase()
                    )
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    let stem = [ticket, (!summary.is_empty()).then_some(summary)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    format!("{stem} ({order}/{total})")
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
    Ok(())
}

/// One layer's deterministic size, and whether it runs past the soft `MAX`
/// ceiling. The harness computes this for the review screen, the plan file and
/// the materialization record so the AI never does arithmetic — and so an
/// oversized layer is always presented with its exact overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitLayerSize {
    pub order: usize,
    pub additions: u64,
    pub deletions: u64,
    pub changed: u64,
    pub max: u64,
}

impl SplitLayerSize {
    pub fn over_max(&self) -> bool {
        self.changed > self.max
    }

    pub fn overflow(&self) -> u64 {
        self.changed.saturating_sub(self.max)
    }
}

/// Size every layer of a validated plan against the frozen manifest.
pub fn split_layer_sizes(preflight: &SplitPreflight, plan: &SplitPlan) -> Vec<SplitLayerSize> {
    let manifest = preflight
        .units
        .iter()
        .map(|unit| (unit.id.as_str(), unit))
        .collect::<BTreeMap<_, _>>();
    plan.responsibilities
        .iter()
        .map(|layer| {
            let (additions, deletions) = layer.units.iter().fold((0u64, 0u64), |(a, d), id| {
                manifest
                    .get(id.as_str())
                    .map_or((a, d), |unit| (a + unit.additions, d + unit.deletions))
            });
            SplitLayerSize {
                order: layer.order,
                additions,
                deletions,
                changed: additions.saturating_add(deletions),
                max: preflight.identity.max,
            }
        })
        .collect()
}

/// Where one materialized layer is rooted.
///
/// A layer is either stacked on the layer below it, or independent: cut
/// straight from the resolved base so its pull request targets the trunk and
/// can merge without waiting for any sibling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitLayerBase {
    ResolvedBase,
    Layer(usize),
}

/// Resolve every layer's parent from the validated plan.
///
/// Independent layers are leaves rooted at the resolved base. The remaining
/// layers keep their bottom-to-top chain, skipping over the independent ones.
pub fn split_layer_bases(plan: &SplitPlan) -> Vec<SplitLayerBase> {
    let mut chain_tip: Option<usize> = None;
    plan.responsibilities
        .iter()
        .map(|layer| {
            // An independent layer roots a *new* chain rather than ending the
            // previous one: later layers may still stack on it.
            if layer.independent {
                chain_tip = Some(layer.order);
                return SplitLayerBase::ResolvedBase;
            }
            let base = chain_tip.map_or(SplitLayerBase::ResolvedBase, SplitLayerBase::Layer);
            chain_tip = Some(layer.order);
            base
        })
        .collect()
}

/// Group the plan into chains: maximal runs of layers that stack on each other,
/// each rooted at the resolved base. Returned as 0-based indexes, bottom to top,
/// in plan order.
///
/// A layer is either a continuation of the chain below it or the root of a new
/// one, so a chain is always a contiguous run — which is exactly what
/// `gh stack link` can publish and what the review screen presents.
pub fn split_chains(plan: &SplitPlan) -> Vec<Vec<usize>> {
    let mut chains: Vec<Vec<usize>> = Vec::new();
    for (index, layer) in plan.responsibilities.iter().enumerate() {
        if index == 0 || layer.independent || chains.is_empty() {
            chains.push(vec![index]);
        } else {
            chains.last_mut().expect("non-empty chains").push(index);
        }
    }
    chains
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
                "{} | {:?} | {} | {} | {}{}",
                unit.id,
                unit.kind,
                unit.path,
                if is_test_path(&unit.path) {
                    "test"
                } else {
                    "implementation"
                },
                counts,
                range
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

pub fn build_corrective_plan_prompt(prompt: &str, parser_error: &str) -> String {
    format!(
        "{prompt}\n\nYour previous response failed the unchanged JSON contract: {}\nCorrect only the contract violation. Return exactly one complete JSON object with the same schema and no prose.",
        parser_error.trim()
    )
}

pub fn describe_snapshot_changes(
    before: &SplitRepositorySnapshot,
    after: &SplitRepositorySnapshot,
) -> Vec<String> {
    let mut changed = Vec::new();
    if before.head != after.head {
        changed.push(format!("HEAD: {} -> {}", before.head, after.head));
    }
    if before.refs != after.refs {
        changed.push("repository refs changed".to_string());
    }
    if before.status != after.status {
        changed.push("Git status changed".to_string());
    }
    let before_files = before.files.iter().cloned().collect::<BTreeMap<_, _>>();
    let after_files = after.files.iter().cloned().collect::<BTreeMap<_, _>>();
    for path in before_files
        .keys()
        .chain(after_files.keys())
        .collect::<BTreeSet<_>>()
    {
        if before_files.get(path) != after_files.get(path) {
            changed.push(format!("file `{path}` changed"));
        }
    }
    changed
}

/// Parse a plan out of a live planning transcript. The embedded terminal shows
/// the harness's own chrome, so the contract object can arrive wrapped in a
/// fenced block or trailed by a closing remark: isolate the JSON object, then
/// apply the identical strict contract.
pub fn parse_split_plan_transcript(
    transcript: &str,
    preflight: &SplitPreflight,
) -> Result<SplitPlan> {
    let fenced = transcript
        .split("```")
        .map(|block| block.strip_prefix("json").unwrap_or(block).trim())
        .find(|block| block.starts_with('{') && block.ends_with('}'));
    let candidate = fenced.or_else(|| {
        let start = transcript.find('{')?;
        let end = transcript.rfind('}')?;
        (end > start).then(|| transcript[start..=end].trim())
    });
    let candidate = candidate.ok_or_else(|| {
        WisetreeError::validation(
            "Split plan must be exactly one JSON response matching the contract: the planning transcript contains no JSON object.",
        )
    })?;
    parse_split_plan(candidate, preflight)
}

pub fn parse_split_plan(response: &str, preflight: &SplitPreflight) -> Result<SplitPlan> {
    let mut plan: SplitPlan = serde_json::from_str(response).map_err(|error| {
        WisetreeError::validation(format!(
            "Split plan must be exactly one JSON response matching the contract: {error}"
        ))
    })?;
    if plan.responsibilities.len() < 2 {
        return Err(WisetreeError::validation(
            "Split plan must contain at least two responsibilities.",
        ));
    }
    // Generated branches and worktrees join their parts with `_`, so a
    // hyphenated slug from the planner is normalized rather than rejected:
    // `feature.1_add-equinor-sync` becomes `feature.1_add_equinor_sync`.
    for responsibility in &mut plan.responsibilities {
        responsibility.branch_slug = responsibility.branch_slug.replace('-', "_");
    }
    let slug = Regex::new(r"^[a-z0-9]+(?:_[a-z0-9]+)*$").expect("static slug regex");
    let manifest = preflight
        .units
        .iter()
        .map(|unit| (unit.id.as_str(), unit))
        .collect::<BTreeMap<_, _>>();
    let mut assigned = BTreeSet::new();
    let mut slugs = BTreeSet::new();
    let mut total_additions = 0u64;
    let mut total_deletions = 0u64;
    let mut derived_test_units = Vec::with_capacity(plan.responsibilities.len());
    let mut touched: Vec<BTreeSet<&str>> = Vec::with_capacity(plan.responsibilities.len());
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
        let mut touched_paths = BTreeSet::new();
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
            // A rename retires its old path too, so independence has to be
            // judged against both sides of the change.
            touched_paths.insert(unit.path.as_str());
            if let Some(old_path) = unit.old_path.as_deref() {
                touched_paths.insert(old_path);
            }
        }
        touched.push(touched_paths);
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
        // The AI never declares which units are tests: whether a path is a test
        // is deterministic, so the harness derives it and echoing it back would
        // only be a way for the model to be wrong.
        derived_test_units.push(
            responsibility
                .units
                .iter()
                .filter(|id| is_test_path(&manifest[id.as_str()].path))
                .cloned()
                .collect::<Vec<_>>(),
        );
        // MAX is a soft ceiling. A responsibility that only fits under it by
        // cutting across a semantic boundary is a worse pull request than an
        // oversized coherent one, so an overflow is surfaced to the reviewer
        // (see `split_layer_sizes`) instead of failing the plan.
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
    // Independence is a promotion the harness has to prove, never a claim it
    // accepts. Layers inside one chain may share paths freely — stacking is
    // exactly what makes that safe. Two different chains may not: each is rooted
    // at the resolved base, so a shared path would leave neither branch carrying
    // that file's final content, and no ordering of the merges reproduces the
    // source. When two chains collide, the later one is absorbed into the chain
    // below it and the check runs again, until the chains are pairwise disjoint.
    // Absorbing only ever over-stacks, so the loop is safe and terminates: it
    // removes one chain each time round.
    loop {
        let chains = split_chains(&plan);
        let chain_paths = chains
            .iter()
            .map(|chain| {
                chain
                    .iter()
                    .flat_map(|index| touched[*index].iter().copied())
                    .collect::<BTreeSet<&str>>()
            })
            .collect::<Vec<_>>();
        let collision = (1..chains.len())
            .find(|j| (0..*j).any(|i| !chain_paths[i].is_disjoint(&chain_paths[*j])));
        match collision {
            Some(j) => plan.responsibilities[chains[j][0]].independent = false,
            None => break,
        }
    }
    for (responsibility, test_units) in plan.responsibilities.iter_mut().zip(derived_test_units) {
        responsibility.test_units = test_units;
    }
    Ok(plan)
}

/// Directory names that hold tests in every ecosystem Wisetree splits.
const TEST_DIRECTORIES: [&str; 7] = [
    "test",
    "tests",
    "spec",
    "specs",
    "__tests__",
    "e2e",
    "cypress",
];

/// Deterministic test classification for a changed path.
///
/// The harness — not the planning AI — decides which change units are tests:
/// the manifest labels each unit and [`parse_split_plan`] derives every
/// layer's `test_units` from this function. It therefore has to cover the
/// languages a real repository uses (RSpec `spec/**/*_spec.rb`, pytest
/// `test_*.py`, Go `*_test.go`, JVM `FooTest.java`, `*.test.tsx`, …), not just
/// this repository's own `tests/*_test.rs` layout.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut segments = lower.split('/').collect::<Vec<_>>();
    let Some(file) = segments.pop() else {
        return false;
    };
    if segments
        .iter()
        .any(|segment| TEST_DIRECTORIES.contains(segment))
    {
        return true;
    }
    // `foo.test.tsx` and `user_spec.rb` both reduce to a stem that carries the
    // marker; `latest.rb` must not, so only suffixed markers count.
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    if ["_test", "_tests", "_spec", "_specs", ".test", ".spec"]
        .iter()
        .any(|marker| stem.ends_with(marker))
    {
        return true;
    }
    if file.starts_with("test_") || file == "conftest.py" || file.ends_with(".feature") {
        return true;
    }
    // JVM/.NET/Swift name test classes in camel case (`PaymentTest.java`,
    // `PaymentSpec.scala`), which the lowercase stem cannot distinguish from
    // words like "latest" — match the original casing instead.
    let original = path.rsplit('/').next().unwrap_or(path);
    let original_stem = original.rsplit_once('.').map_or(original, |(stem, _)| stem);
    ["Test", "Tests", "Spec", "Specs"]
        .iter()
        .any(|marker| original_stem.ends_with(marker) && original_stem.len() > marker.len())
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
    let sizes = split_layer_sizes(preflight, plan);
    let bases = split_layer_bases(plan);
    output.push_str("\n| Layer | Responsibility | Branch slug | Targets | Units | + | - | Total | MAX |\n| ---: | --- | --- | --- | --- | ---: | ---: | ---: | --- |\n");
    for ((layer, size), base) in plan.responsibilities.iter().zip(&sizes).zip(&bases) {
        output.push_str(&format!(
            "| {} | {} | `{}` | {} | {} | {} | {} | {} | {} |\n",
            layer.order,
            layer.name,
            layer.branch_slug,
            match base {
                SplitLayerBase::ResolvedBase => format!("`{}`", identity.base_ref),
                SplitLayerBase::Layer(order) => format!("layer {order}"),
            },
            layer.units.join(", "),
            size.additions,
            size.deletions,
            size.changed,
            if size.over_max() {
                format!("over by {}", size.overflow())
            } else {
                "within".to_string()
            }
        ));
        if layer.independent {
            output.push_str(&format!(
                "\nDependency: independent — starts a new chain on `{}`, so it does not wait for any earlier pull request.\n\n",
                identity.base_ref
            ));
        } else {
            output.push_str(&format!("\nDependency: {}\n\n", layer.rationale));
        }
        if size.over_max() {
            output.push_str(&format!(
                "> [!WARNING]\n> This responsibility is {} changed lines, {} over the MAX of {}. It was kept whole because splitting it would break the single responsibility described above.\n\n",
                size.changed,
                size.overflow(),
                size.max
            ));
        }
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
    let run = SplitRunRecord {
        version: SPLIT_RUN_VERSION,
        identity: preflight.identity.clone(),
        units: preflight.units.clone(),
        plan: plan.clone(),
        status: status.trim().to_string(),
    };
    output.push_str(&format!(
        "\n{RUN_MARKER}{} -->\n",
        serde_json::to_string(&run).expect("Split run record is serializable")
    ));
    output
}
