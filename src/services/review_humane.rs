//! The "humane" voice of the Review command: comments and the review summary
//! written the way a teammate types them, plus a real before/after smoke test
//! for the summary. Discovery is untouched — only the wording of what gets
//! posted changes. Everything here is deterministic (prompt assembly,
//! parsing, rendering); the AI calls live on `DashboardService`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::errors::Result;
use crate::services::dashboard::{is_test_file, substitute_review_prompt, ReviewFinding};
use crate::services::review_telemetry::ReviewScanTelemetry;
use crate::worktree::CreateOutcome;

const HUMANIZE_PROMPT: &str = include_str!("../../prompts/reviewer_humanize.md");
const HUMANE_SUMMARY_PROMPT: &str = include_str!("../../prompts/reviewer_summary_humane.md");
const SMOKE_TEST_PROMPT: &str = include_str!("../../prompts/reviewer_smoke_test.md");
const VOICE_PROMPT: &str = include_str!("../../prompts/reviewer_voice.md");
const SMOKE_REFORMAT_PROMPT: &str = include_str!("../../prompts/reviewer_smoke_reformat.md");

const HUMANIZE_BEGIN: &str = "===WISETREE-HUMANIZE-BEGIN===";
const HUMANIZE_END: &str = "===WISETREE-HUMANIZE-END===";
const SUMMARY_BEGIN: &str = "===WISETREE-SUMMARY-BEGIN===";
const SUMMARY_END: &str = "===WISETREE-SUMMARY-END===";
const SMOKE_BEGIN: &str = "===WISETREE-SMOKE-BEGIN===";
const SMOKE_END: &str = "===WISETREE-SMOKE-END===";

/// Findings rewritten per humanize call. Every call pays the harness's fixed
/// system-prompt overhead (~10k tokens) while each rewritten comment is only
/// ~150 output tokens, so one call covers a typical review; only a very
/// large one is split.
pub const HUMANIZE_BATCH_SIZE: usize = 25;

/// Placeholder the smoke test leaves where a screenshot belongs. The Summary
/// page warns while one is still in the body.
pub const SCREENSHOT_TODO_MARKER: &str = "TODO(screenshot)";

/// How the posted comments and the summary are worded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewVoice {
    /// Conversational comments, a short human summary with the key concerns,
    /// and a real before/after smoke test.
    #[default]
    Humane,
    /// The structured format: titled comments with category/severity badges
    /// and a summary table with charts.
    Robotic,
}

impl ReviewVoice {
    pub fn toggled(self) -> Self {
        match self {
            Self::Humane => Self::Robotic,
            Self::Robotic => Self::Humane,
        }
    }
}

/// What the smoke test showed. Every variant is a real result: a PR whose
/// "after" looks like its "before", or that breaks something new, is exactly
/// what the review must say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SmokeOutcome {
    /// Before shows the problem; after shows what the PR promises.
    AsDescribed,
    /// Both sides behave the same: the PR's claim is not visible.
    Unchanged,
    /// After shows a new error or broken behavior.
    Regression,
    /// One side never got to run the scenario.
    Inconclusive,
}

impl SmokeOutcome {
    fn parse(keyword: &str) -> Option<Self> {
        match keyword
            .trim()
            .trim_matches('`')
            .to_ascii_lowercase()
            .as_str()
        {
            "as-described" | "as described" => Some(Self::AsDescribed),
            "unchanged" => Some(Self::Unchanged),
            "regression" => Some(Self::Regression),
            "inconclusive" => Some(Self::Inconclusive),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AsDescribed => "as described",
            Self::Unchanged => "unchanged",
            Self::Regression => "regression",
            Self::Inconclusive => "inconclusive",
        }
    }
}

/// The smoke test's result: its outcome (for the reviewer and the summary
/// writer, never posted) plus the three parts of the `# Smoke Test` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSmokeTest {
    /// `None` when the model did not state one; the summary writer then
    /// judges from Before/After itself.
    pub outcome: Option<SmokeOutcome>,
    pub outcome_note: String,
    pub steps: String,
    pub before: String,
    pub after: String,
}

impl ReviewSmokeTest {
    pub fn markdown(&self) -> String {
        format!(
            "# Smoke Test\n\n## Steps\n\n{}\n\n### Before\n\n{}\n\n### After\n\n{}",
            self.steps.trim(),
            self.before.trim(),
            self.after.trim()
        )
    }
}

/// Inputs of the smoke-test run, resolved during Review preparation.
#[derive(Debug, Clone)]
pub struct ReviewSmokeTestRequest {
    pub number: u64,
    pub title: String,
    /// The PR branch, checked out in the reviewed worktree: names the
    /// smoke-test worktree, and is its starting point only when `head_sha`
    /// is not available locally.
    pub branch: String,
    pub base_ref_name: String,
    /// The PR head as GitHub reports it: the commit under review, and so
    /// the smoke test's "after" side.
    pub head_sha: String,
    pub changed_files: Vec<String>,
}

/// One smoke-test run: the parsed result, the telemetry of every AI call it
/// took, whether its worktree could be removed afterwards, and whether the
/// result was reused from the cache instead of run.
#[derive(Debug)]
pub struct ReviewSmokeTestAttempt {
    pub result: Result<ReviewSmokeTest>,
    pub telemetry: Vec<ReviewScanTelemetry>,
    pub cleanup_error: Option<String>,
    pub reused: bool,
}

/// Smoke tests kept in the cache file; older ones are dropped.
const SMOKE_TEST_CACHE_LIMIT: usize = 20;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SmokeTestCache {
    entries: Vec<CachedSmokeTest>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedSmokeTest {
    before_sha: String,
    after_sha: String,
    smoke_test: ReviewSmokeTest,
}

/// A smoke test already run for exactly these two commits. The same code on
/// both sides gives the same evidence, so re-reviewing an unchanged PR (after
/// an abort, or to post more comments) never pays for it again. A missing or
/// unreadable cache is simply a miss.
pub fn load_cached_smoke_test(
    cache_file: &Path,
    before_sha: &str,
    after_sha: &str,
) -> Option<ReviewSmokeTest> {
    let cache: SmokeTestCache =
        serde_json::from_str(&std::fs::read_to_string(cache_file).ok()?).ok()?;
    cache
        .entries
        .into_iter()
        .rev()
        .find(|entry| entry.before_sha == before_sha && entry.after_sha == after_sha)
        .map(|entry| entry.smoke_test)
}

/// Remember a finished smoke test for these two commits. Inconclusive runs
/// are not kept: what blocked them (a missing runtime, a service that was
/// down) may be gone next time, so they deserve another try.
pub fn store_smoke_test(
    cache_file: &Path,
    before_sha: &str,
    after_sha: &str,
    smoke_test: &ReviewSmokeTest,
) -> std::io::Result<()> {
    if smoke_test.outcome == Some(SmokeOutcome::Inconclusive) {
        return Ok(());
    }
    let mut cache: SmokeTestCache = std::fs::read_to_string(cache_file)
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    cache
        .entries
        .retain(|entry| entry.before_sha != before_sha || entry.after_sha != after_sha);
    cache.entries.push(CachedSmokeTest {
        before_sha: before_sha.to_string(),
        after_sha: after_sha.to_string(),
        smoke_test: smoke_test.clone(),
    });
    let excess = cache.entries.len().saturating_sub(SMOKE_TEST_CACHE_LIMIT);
    cache.entries.drain(..excess);
    if let Some(dir) = cache_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(cache_file, serde_json::to_string_pretty(&cache)?)
}

/// Root manifests whose presence says which runtimes a smoke test needs,
/// with the version command for each runtime.
pub const SMOKE_TEST_MANIFESTS: &[(&str, &[(&str, &str)])] = &[
    (
        "package.json",
        &[("node", "--version"), ("npm", "--version")],
    ),
    ("Gemfile", &[("ruby", "--version"), ("bundle", "--version")]),
    ("Cargo.toml", &[("cargo", "--version")]),
    ("pyproject.toml", &[("python3", "--version")]),
    ("requirements.txt", &[("python3", "--version")]),
    ("go.mod", &[("go", "version")]),
    ("pom.xml", &[("java", "--version"), ("mvn", "--version")]),
    ("build.gradle", &[("java", "--version")]),
    ("composer.json", &[("php", "--version")]),
    ("mix.exs", &[("elixir", "--version")]),
    ("Makefile", &[("make", "--version")]),
];

/// One file's diff is inlined only up to this size; a bigger one (usually a
/// generated artifact) is listed as omitted for the AI to inspect on demand.
const SMOKE_FACTS_FILE_DIFF_LIMIT: usize = 12_000;
/// Total inlined diff across files.
const SMOKE_FACTS_DIFF_LIMIT: usize = 48_000;
/// Each inlined manifest.
const SMOKE_FACTS_MANIFEST_LIMIT: usize = 6_000;

/// The discovery facts the smoke-test AI would otherwise spend its first
/// turns collecting (every turn re-sends the whole conversation): the diff
/// stat, the diff itself within a byte budget, submodule state, installed
/// runtimes, and the root manifests.
pub fn build_smoke_test_facts(
    diff_stat: &str,
    diff: &str,
    submodules: &str,
    runtimes: &[(String, String)],
    manifests: &[(String, String)],
) -> String {
    let mut out = format!(
        "#### Diff stat\n\n~~~text\n{}\n~~~\n",
        diff_stat.trim_matches('\n').trim_end()
    );
    let mut inlined = String::new();
    let mut omitted = Vec::new();
    for chunk in split_diff_by_file(diff) {
        let path = chunk
            .lines()
            .next()
            .and_then(|header| header.rsplit_once(" b/"))
            .map_or("?", |(_, path)| path);
        if chunk.len() <= SMOKE_FACTS_FILE_DIFF_LIMIT
            && inlined.len() + chunk.len() <= SMOKE_FACTS_DIFF_LIMIT
        {
            inlined.push_str(chunk);
        } else {
            omitted.push(format!("- `{path}` ({} diff lines)", chunk.lines().count()));
        }
    }
    out.push_str(&format!(
        "\n#### Diff\n\n~~~diff\n{}\n~~~\n",
        inlined.trim_end()
    ));
    if !omitted.is_empty() {
        out.push_str(&format!(
            "\nOmitted from the diff above (too large, usually generated; never print their full diff):\n{}\n",
            omitted.join("\n")
        ));
    }
    let submodules = submodules.trim();
    out.push_str(&format!(
        "\n#### Submodules\n\n{}\n",
        if submodules.is_empty() {
            "none".to_string()
        } else {
            format!("~~~text\n{submodules}\n~~~ (a leading `-` means not initialized)")
        }
    ));
    out.push_str("\n#### Runtimes\n\n");
    if runtimes.is_empty() {
        out.push_str("No root manifest names a runtime.\n");
    }
    for (tool, version) in runtimes {
        out.push_str(&format!("- `{tool}`: {version}\n"));
    }
    for (name, content) in manifests {
        out.push_str(&format!(
            "\n#### `{name}`\n\n~~~\n{}\n~~~\n",
            truncate_chars(content.trim(), SMOKE_FACTS_MANIFEST_LIMIT)
        ));
    }
    out
}

/// Split a unified diff into one chunk per `diff --git` section.
fn split_diff_by_file(diff: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = diff
        .match_indices("diff --git ")
        .filter(|(at, _)| *at == 0 || diff.as_bytes()[at - 1] == b'\n')
        .map(|(at, _)| at)
        .collect();
    starts.push(diff.len());
    starts
        .windows(2)
        .map(|pair| &diff[pair[0]..pair[1]])
        .collect()
}

/// Why this PR has nothing to smoke-test, or `None` when it does. A diff
/// that only touches tests, documentation, or CI configuration has no
/// runtime behavior to compare before and after, so the whole smoke test
/// (worktree, post-create commands, AI run) is skipped. Lockfiles and other
/// dependency manifests still run: a version bump can change behavior.
pub fn smoke_test_skip_reason(changed_paths: &[String]) -> Option<&'static str> {
    let nothing_runnable = !changed_paths.is_empty()
        && changed_paths
            .iter()
            .all(|path| is_test_file(path) || is_doc_path(path) || is_ci_path(path));
    nothing_runnable.then_some("only tests, docs, or CI configuration changed")
}

fn is_doc_path(path: &str) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or_default();
    let in_docs_dir = lower
        .split('/')
        .rev()
        .skip(1)
        .any(|dir| matches!(dir, "doc" | "docs" | "documentation"));
    let doc_extension = matches!(
        name.rsplit_once('.').map(|(_, ext)| ext),
        Some("md" | "mdx" | "markdown" | "rst" | "adoc")
    );
    let doc_name = [
        "license",
        "licence",
        "changelog",
        "authors",
        "contributing",
        "codeowners",
        "notice",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix));
    in_docs_dir || doc_extension || doc_name
}

fn is_ci_path(path: &str) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    [".github/", ".gitlab/", ".circleci/", ".buildkite/"]
        .iter()
        .any(|dir| lower.starts_with(dir))
        || matches!(
            lower.as_str(),
            ".gitlab-ci.yml"
                | ".travis.yml"
                | "azure-pipelines.yml"
                | "bitbucket-pipelines.yml"
                | "appveyor.yml"
                | "jenkinsfile"
        )
}

/// The smoke-test worktree's name and branch: `smoke_test_{BRANCH}`, with
/// `/` flattened so it stays one directory and one branch segment.
pub fn smoke_test_worktree_name(branch: &str) -> String {
    format!("smoke_test_{}", branch.trim().replace('/', "-"))
}

/// One humanize call: a rewritten comment per supplied finding (`None` keeps
/// that finding's original explanation).
#[derive(Debug)]
pub struct ReviewHumanizeAttempt {
    pub result: Result<Vec<Option<String>>>,
    pub telemetry: ReviewScanTelemetry,
}

impl ReviewFinding {
    /// The exact markdown posted for this finding in `voice`.
    pub fn comment_body_for(&self, voice: ReviewVoice) -> String {
        match voice {
            ReviewVoice::Humane => self.humane_comment_body(),
            ReviewVoice::Robotic => self.comment_body(),
        }
    }

    /// A comment that reads like a person wrote it: the prose alone, no
    /// title heading and no category/severity badge, followed by the
    /// one-click suggestion. File-level comments name their file, since
    /// GitHub shows them on the conversation tab without an anchor.
    pub fn humane_comment_body(&self) -> String {
        let mut text = self.explanation.trim();
        if text.is_empty() && self.suggestion.is_none() {
            text = self.title.trim();
        }
        let mut body = String::new();
        if self.line.is_none() && !text.contains(self.file.as_str()) {
            body.push_str(&format!("In `{}`:", self.file));
            if !text.is_empty() {
                body.push_str("\n\n");
            }
        }
        body.push_str(text);
        if let Some(suggestion) = &self.suggestion {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            let fence = if self.line.is_some() {
                "suggestion"
            } else {
                ""
            };
            body.push_str(&format!("```{fence}\n{suggestion}\n```"));
        }
        body
    }
}

/// The humanize prompt for one batch, numbering findings from 1.
pub fn build_humanize_prompt(findings: &[ReviewFinding]) -> String {
    let rendered = findings
        .iter()
        .enumerate()
        .map(|(i, finding)| {
            let anchor = match finding.line {
                Some(_) => format!("inline comment on `{}`", finding.descriptor()),
                None => format!("general comment about the file `{}`", finding.file),
            };
            let mut text = format!(
                "### Finding {}\n- Anchor: {anchor}\n- Severity: {}\n- Category: {}\n- Title: {}\n\
                 - A suggestion block follows the comment: {}\n\nExplanation:\n{}\n",
                i + 1,
                finding.severity.label(),
                finding.category,
                finding.title.trim(),
                if finding.suggestion.is_some() {
                    "yes"
                } else {
                    "no"
                },
                finding.explanation.trim(),
            );
            if let Some(suggestion) = &finding.suggestion {
                text.push_str(&format!(
                    "\nSuggested code (context only, the harness appends it):\n~~~\n{suggestion}\n~~~\n"
                ));
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n");
    substitute_review_prompt(
        HUMANIZE_PROMPT,
        &[
            ("VOICE_RULES", VOICE_PROMPT.trim()),
            ("HUMANIZE_FINDINGS", &rendered),
        ],
    )
}

/// The "Other" revision's voice section. Humane revisions come back already
/// written as the posted comment, so they never need a second humanize call;
/// the robotic voice keeps the revision prompt unchanged.
pub fn revision_voice_section(voice: ReviewVoice) -> String {
    match voice {
        ReviewVoice::Robotic => String::new(),
        ReviewVoice::Humane => format!(
            "## Voice\n\nThe EXPLANATION is posted verbatim as the PR comment, so write it the \
             way a senior engineer on the team types a review comment by hand. TITLE is an \
             internal label and is never posted. Keep every fact; never state the severity or \
             the category as a label.\n\n{}",
            VOICE_PROMPT.trim()
        ),
    }
}

/// Parse the humanize block into one entry per supplied finding. `None`
/// when the block is missing; a missing or unusable comment is `None` in its
/// slot so that finding keeps its original explanation.
pub fn parse_humanized_comments(output: &str, count: usize) -> Option<Vec<Option<String>>> {
    let block = marker_block(output, HUMANIZE_BEGIN, HUMANIZE_END)?;
    let mut comments = vec![None; count];
    let mut current: Option<usize> = None;
    let mut buffer = String::new();
    for line in block.lines() {
        if let Some(number) = comment_marker_number(line) {
            if let Some(slot) = current.and_then(|index| comments.get_mut(index)) {
                *slot = usable_humanized_comment(&buffer);
            }
            current = number.checked_sub(1).filter(|index| *index < count);
            buffer.clear();
            continue;
        }
        if current.is_some() {
            buffer.push_str(line);
            buffer.push('\n');
        }
    }
    if let Some(slot) = current.and_then(|index| comments.get_mut(index)) {
        *slot = usable_humanized_comment(&buffer);
    }
    Some(comments)
}

fn comment_marker_number(line: &str) -> Option<usize> {
    line.trim()
        .strip_prefix("---COMMENT ")?
        .strip_suffix("---")?
        .trim()
        .parse()
        .ok()
}

/// A rewritten comment is used only when it stays plain prose: the harness
/// owns the suggestion block, and a heading would bring back the robotic
/// title the voice exists to drop.
fn usable_humanized_comment(text: &str) -> Option<String> {
    let text = text.trim();
    let rejected = text.is_empty()
        || text.starts_with('#')
        || text.contains("```suggestion")
        || text.contains("===WISETREE");
    (!rejected).then(|| typed_punctuation(text))
}

/// People type straight quotes; models emit typographic ones (`’`, `“`),
/// which is one of the quickest tells that a comment was generated.
fn typed_punctuation(text: &str) -> String {
    text.replace(['\u{2018}', '\u{2019}'], "'")
        .replace(['\u{201C}', '\u{201D}'], "\"")
}

/// The humane summary prompt: every posted finding with its posted wording,
/// and the smoke-test outcome when one ran.
pub fn build_humane_summary_prompt(
    posted: &[ReviewFinding],
    smoke_test: Option<&ReviewSmokeTest>,
    pr_title: &str,
) -> String {
    let mut facts = format!("Pull request title: {}\n\n", pr_title.trim());
    if posted.is_empty() {
        facts.push_str("Approved findings: none, no inline comments were posted.\n");
    } else {
        facts.push_str("Approved findings (each is already posted as an inline comment):\n");
        for (i, finding) in posted.iter().enumerate() {
            facts.push_str(&format!(
                "{}. [{}] [{}] {} ({})\n   Posted comment: {}\n",
                i + 1,
                finding.severity.label(),
                finding.category,
                finding.title.trim(),
                finding.descriptor(),
                truncate_chars(&one_line(&finding.explanation), 800),
            ));
        }
    }
    facts.push('\n');
    match smoke_test {
        Some(smoke) => facts.push_str(&format!(
            "Smoke test (run on the base branch and on this branch):\nOutcome: {}\nSteps:\n{}\n\nBefore:\n{}\n\nAfter:\n{}\n",
            match smoke.outcome {
                Some(outcome) => format!("{} ({})", outcome.label(), smoke.outcome_note.trim()),
                None => "not stated, judge it from Before and After".to_string(),
            },
            truncate_chars(smoke.steps.trim(), 1500),
            truncate_chars(smoke.before.trim(), 1500),
            truncate_chars(smoke.after.trim(), 1500),
        )),
        None => facts.push_str("Smoke test: not available.\n"),
    }
    substitute_review_prompt(HUMANE_SUMMARY_PROMPT, &[("SUMMARY_FACTS", &facts)])
}

/// Accept the model's summary opening only from its marker block (so no
/// stray CLI output can reach the PR) and only when it is plain markdown
/// prose. Anything after a `Smoke Test` heading is dropped: the harness
/// appends the real section.
pub fn validate_humane_summary_overview(output: &str) -> Option<String> {
    let block = marker_block(output, SUMMARY_BEGIN, SUMMARY_END)?;
    let mut kept = Vec::new();
    for line in block.trim().lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') && trimmed.to_lowercase().contains("smoke test") {
            break;
        }
        kept.push(line);
    }
    let text = typed_punctuation(kept.join("\n").trim());
    let rejected = text.is_empty()
        || text.len() > 6000
        || text.starts_with('#')
        || text.starts_with("```")
        || text.contains("===WISETREE");
    (!rejected).then_some(text)
}

/// Fallback opening when the summary call fails or returns something
/// unusable — short, like a person would leave it.
pub fn deterministic_humane_overview(posted: usize) -> String {
    match posted {
        0 => "Looks good to me 👍".to_string(),
        1 => "Left one comment inline.".to_string(),
        _ => "Left a few comments inline.".to_string(),
    }
}

/// The posted review body: the opening, then the smoke test when it ran.
pub fn build_humane_review_summary(overview: &str, smoke_test: Option<&ReviewSmokeTest>) -> String {
    match smoke_test {
        Some(smoke) => format!("{}\n\n{}", overview.trim(), smoke.markdown()),
        None => overview.trim().to_string(),
    }
}

/// The smoke-test prompt. The AI runs inside the `smoke_branch` worktree and
/// switches between `before_sha` and the branch itself; it is told never to
/// mention either in its output. `setup_report` says how the worktree's
/// copy patterns and post-create commands went.
pub fn build_smoke_test_prompt(
    request: &ReviewSmokeTestRequest,
    smoke_branch: &str,
    before_sha: &str,
    after_sha: &str,
    setup_report: &str,
    repo_facts: &str,
) -> String {
    let files = if request.changed_files.is_empty() {
        "(see the diff)".to_string()
    } else {
        request
            .changed_files
            .iter()
            .map(|path| format!("- `{path}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let facts = format!(
        "- Number: #{}\n- Title: {}\n- Base branch: {}\n- Changed files:\n{files}",
        request.number,
        request.title.trim(),
        request.base_ref_name,
    );
    substitute_review_prompt(
        SMOKE_TEST_PROMPT,
        &[
            ("SETUP_REPORT", setup_report),
            ("SMOKE_BRANCH", smoke_branch),
            ("BASE_BRANCH", &request.base_ref_name),
            ("BEFORE_SHA", before_sha),
            ("AFTER_SHA", after_sha),
            ("PR_FACTS", &facts),
            ("REPO_FACTS", repo_facts),
        ],
    )
}

/// How the smoke worktree's wisetree setup went, for the smoke-test prompt:
/// what the copy/link patterns brought in and how each post-create command
/// ended (a failure is something the AI may need to work around).
pub fn smoke_test_setup_report(outcome: &CreateOutcome) -> String {
    let mut lines = Vec::new();
    if let Some(report) = &outcome.copy_report {
        let copied = if report.copied.is_empty() {
            "nothing matched".to_string()
        } else {
            report
                .copied
                .iter()
                .map(|path| format!("`{path}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(format!("- Copied from the main checkout: {copied}"));
        lines.extend(
            report
                .errors
                .iter()
                .map(|err| format!("- Copy error: {err}")),
        );
    }
    if let Some(report) = &outcome.link_report {
        lines.extend(
            report
                .errors
                .iter()
                .map(|err| format!("- Link error: {err}")),
        );
    }
    for run in &outcome.command_runs {
        if run.success {
            lines.push(format!(
                "- Post-create command `{}`: succeeded",
                run.command
            ));
        } else {
            let detail = run.error.as_deref().unwrap_or(run.output.trim());
            lines.push(format!(
                "- Post-create command `{}`: FAILED ({})",
                run.command,
                truncate_chars(&one_line(detail), 300)
            ));
        }
    }
    if lines.is_empty() {
        "- No copy patterns or post-create commands are configured.".to_string()
    } else {
        lines.join("\n")
    }
}

/// Repair prompt for a smoke test that ran but whose report missed the
/// contract: the utility model re-emits it without re-running anything.
pub fn build_smoke_reformat_prompt(previous_output: &str) -> String {
    let start = SMOKE_TEST_PROMPT
        .find("## Output contract")
        .expect("embedded smoke-test prompt has an output contract");
    let tail = &SMOKE_TEST_PROMPT[start..];
    let contract = tail[..tail.find("\n## Inputs").unwrap_or(tail.len())].trim_end();
    substitute_review_prompt(
        SMOKE_REFORMAT_PROMPT,
        &[
            ("OUTPUT_CONTRACT", contract),
            ("PREVIOUS_OUTPUT", previous_output),
        ],
    )
}

/// Parse the smoke-test block. Steps, Before, and After must be present and
/// non-empty, in order; the leading outcome line is read when it is there.
pub fn parse_smoke_test(output: &str) -> Option<ReviewSmokeTest> {
    let block = marker_block(output, SMOKE_BEGIN, SMOKE_END)?;
    let (head, rest) = block.split_once("---STEPS---")?;
    let (steps, rest) = rest.split_once("---BEFORE---")?;
    let (before, after) = rest.split_once("---AFTER---")?;
    let outcome_line = head
        .split_once("---OUTCOME---")
        .map(|(_, line)| line.trim())
        .unwrap_or_default();
    let (keyword, note) = outcome_line.split_once(':').unwrap_or((outcome_line, ""));
    let outcome = SmokeOutcome::parse(keyword);
    let smoke = ReviewSmokeTest {
        outcome,
        outcome_note: if outcome.is_some() { note.trim() } else { "" }.to_string(),
        steps: steps.trim().to_string(),
        before: before.trim().to_string(),
        after: after.trim().to_string(),
    };
    let complete = !smoke.steps.is_empty() && !smoke.before.is_empty() && !smoke.after.is_empty();
    complete.then_some(smoke)
}

/// The text between the last `begin` marker and the `end` marker after it.
/// The last one wins so an echoed contract example never shadows the answer.
fn marker_block<'a>(output: &'a str, begin: &str, end: &str) -> Option<&'a str> {
    let start = output.rfind(begin)? + begin.len();
    let rest = &output[start..];
    Some(&rest[..rest.find(end)?])
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::dashboard::ReviewSeverity;

    fn finding(line: Option<u64>, suggestion: Option<&str>) -> ReviewFinding {
        ReviewFinding {
            category: "Test".to_string(),
            severity: ReviewSeverity::High,
            file: "tests/evalraw.test.js".to_string(),
            start_line: None,
            line,
            title: "Assertion accepts any error".to_string(),
            explanation: "`.toThrow()` with no argument accepts any error, so this passed on \
                          `main` too. Asserting the parser message would catch that:"
                .to_string(),
            suggestion: suggestion.map(str::to_string),
        }
    }

    #[test]
    fn humane_inline_comment_is_prose_plus_suggestion_without_title_or_badge() {
        let body =
            finding(Some(255), Some("expect(f).toThrow(/Unexpected/)")).humane_comment_body();
        assert_eq!(
            body,
            "`.toThrow()` with no argument accepts any error, so this passed on `main` too. \
             Asserting the parser message would catch that:\n\n\
             ```suggestion\nexpect(f).toThrow(/Unexpected/)\n```"
        );
        assert!(!body.contains("###"));
        assert!(!body.contains("<p align=\"center\">"));
    }

    #[test]
    fn humane_file_level_comment_names_the_file_and_uses_a_plain_code_block() {
        let body = finding(None, Some("assert!(x);")).humane_comment_body();
        assert!(body.starts_with("In `tests/evalraw.test.js`:\n\n`.toThrow()`"));
        assert!(body.ends_with("```\nassert!(x);\n```"));
        assert!(!body.contains("```suggestion"));
    }

    #[test]
    fn humane_comment_falls_back_to_the_title_when_there_is_nothing_else_to_say() {
        let mut empty = finding(Some(3), None);
        empty.explanation = "  ".to_string();
        assert_eq!(empty.humane_comment_body(), "Assertion accepts any error");
    }

    #[test]
    fn robotic_voice_keeps_the_structured_comment() {
        let finding = finding(Some(255), None);
        assert_eq!(
            finding.comment_body_for(ReviewVoice::Robotic),
            finding.comment_body()
        );
        assert_eq!(
            finding.comment_body_for(ReviewVoice::Humane),
            finding.humane_comment_body()
        );
    }

    #[test]
    fn humanize_prompt_numbers_each_finding_and_flags_its_suggestion() {
        let prompt = build_humanize_prompt(&[finding(Some(255), Some("x()")), finding(None, None)]);
        assert!(prompt
            .contains("### Finding 1\n- Anchor: inline comment on `tests/evalraw.test.js:255`"));
        assert!(prompt.contains("- A suggestion block follows the comment: yes"));
        assert!(prompt.contains("~~~\nx()\n~~~"));
        assert!(prompt.contains("### Finding 2\n- Anchor: general comment about the file"));
        assert!(!prompt.contains("HUMANIZE_FINDINGS"));
    }

    #[test]
    fn humanized_comments_land_in_their_numbered_slots() {
        let output = "thinking...\n===WISETREE-HUMANIZE-BEGIN===\n---COMMENT 2---\nSecond, \
                      rewritten.\n---COMMENT 1---\nFirst one\n\nwith two paragraphs.\n\
                      ---COMMENT 9---\nOut of range.\n===WISETREE-HUMANIZE-END===\n";
        let parsed = parse_humanized_comments(output, 3).unwrap();
        assert_eq!(
            parsed,
            vec![
                Some("First one\n\nwith two paragraphs.".to_string()),
                Some("Second, rewritten.".to_string()),
                None,
            ]
        );
    }

    #[test]
    fn humanized_comments_and_summary_use_typed_quotes() {
        let output = "===WISETREE-HUMANIZE-BEGIN===\n---COMMENT 1---\nThe parser\u{2019}s \
                      \u{201C}error\u{201D} field\n===WISETREE-HUMANIZE-END===";
        assert_eq!(
            parse_humanized_comments(output, 1).unwrap(),
            vec![Some("The parser's \"error\" field".to_string())]
        );
        assert_eq!(
            validate_humane_summary_overview(&summary_block("It\u{2019}s fine")).as_deref(),
            Some("It's fine")
        );
    }

    #[test]
    fn unusable_humanized_comments_keep_the_original() {
        let output = "===WISETREE-HUMANIZE-BEGIN===\n---COMMENT 1---\n### A heading\n\
                      ---COMMENT 2---\nUse this:\n```suggestion\nx\n```\n---COMMENT 3---\n\n\
                      ===WISETREE-HUMANIZE-END===";
        assert_eq!(
            parse_humanized_comments(output, 3).unwrap(),
            vec![None, None, None]
        );
        assert_eq!(parse_humanized_comments("no block here", 1), None);
    }

    fn smoke() -> ReviewSmokeTest {
        ReviewSmokeTest {
            outcome: Some(SmokeOutcome::AsDescribed),
            outcome_note: "The crash is gone.".to_string(),
            steps: "1. Run it:\n\n```bash\nnode smoke.mjs\n```".to_string(),
            before: "It crashes.\n\n```text\nRuntimeError\n```".to_string(),
            after: "It works.\n\n```text\n4\n```".to_string(),
        }
    }

    #[test]
    fn smoke_test_markdown_follows_the_review_template() {
        assert_eq!(
            smoke().markdown(),
            "# Smoke Test\n\n## Steps\n\n1. Run it:\n\n```bash\nnode smoke.mjs\n```\n\n\
             ### Before\n\nIt crashes.\n\n```text\nRuntimeError\n```\n\n\
             ### After\n\nIt works.\n\n```text\n4\n```"
        );
    }

    fn smoke_block(outcome: &str) -> String {
        format!(
            "===WISETREE-SMOKE-BEGIN===\n{outcome}---STEPS---\n1. Run it:\n\n```bash\nnode \
             smoke.mjs\n```\n---BEFORE---\nIt crashes.\n\n```text\nRuntimeError\n```\n\
             ---AFTER---\nIt works.\n\n```text\n4\n```\n===WISETREE-SMOKE-END==="
        )
    }

    #[test]
    fn smoke_test_block_parses_its_outcome_and_three_sections() {
        let output = smoke_block("---OUTCOME---\nas-described: The crash is gone.\n");
        assert_eq!(parse_smoke_test(&output), Some(smoke()));
    }

    #[test]
    fn every_smoke_outcome_is_a_result_and_a_missing_one_is_left_to_the_summary() {
        for (line, outcome) in [
            (
                "unchanged: Same crash on both sides.",
                SmokeOutcome::Unchanged,
            ),
            (
                "`regression`: After throws a new error.",
                SmokeOutcome::Regression,
            ),
            (
                "Inconclusive: needs a database.",
                SmokeOutcome::Inconclusive,
            ),
        ] {
            let parsed = parse_smoke_test(&smoke_block(&format!("---OUTCOME---\n{line}\n")))
                .expect("a smoke test with any outcome is a result");
            assert_eq!(parsed.outcome, Some(outcome), "{line}");
            assert!(!parsed.outcome_note.is_empty());
        }
        let unstated = parse_smoke_test(&smoke_block("")).unwrap();
        assert_eq!(unstated.outcome, None);
        let unknown = parse_smoke_test(&smoke_block("---OUTCOME---\nworked fine\n")).unwrap();
        assert_eq!((unknown.outcome, unknown.outcome_note.as_str()), (None, ""));
    }

    #[test]
    fn smoke_test_block_with_a_missing_section_is_rejected() {
        let output = "===WISETREE-SMOKE-BEGIN===\n---STEPS---\n1. x\n---BEFORE---\n\n\
                      ---AFTER---\nok\n===WISETREE-SMOKE-END===";
        assert_eq!(parse_smoke_test(output), None);
        assert_eq!(parse_smoke_test("nothing"), None);
    }

    #[test]
    fn smoke_test_prompt_carries_the_worktree_both_sides_and_its_setup() {
        let request = ReviewSmokeTestRequest {
            number: 16,
            title: "Prevent WASM corruption".to_string(),
            branch: "fix/wasm".to_string(),
            base_ref_name: "main".to_string(),
            head_sha: "c3cbac3".to_string(),
            changed_files: vec!["build.sh".to_string()],
        };
        let prompt = build_smoke_test_prompt(
            &request,
            "smoke_test_fix-wasm",
            "4e691a7",
            "c3cbac3",
            "- Post-create command `npm ci`: FAILED (exit 1)",
            "#### Diff stat\n\n~~~text\n build.sh | 2 +-\n~~~",
        );
        assert!(prompt.contains("### Repository facts\n\n#### Diff stat"));
        assert!(prompt.contains("Do not re-run those commands"));
        assert!(prompt.contains("on the branch `smoke_test_fix-wasm` at the PR head (`c3cbac3`)"));
        assert!(prompt.contains("`git checkout --quiet --detach 4e691a7`"));
        assert!(prompt.contains("`git checkout --quiet smoke_test_fix-wasm`"));
        assert!(prompt.contains("`git diff 4e691a7 c3cbac3`"));
        assert!(prompt.contains("- Post-create command `npm ci`: FAILED (exit 1)"));
        assert!(prompt.contains("- Number: #16\n- Title: Prevent WASM corruption"));
        assert!(prompt.contains("- `build.sh`"));
        for token in [
            "REPO_FACTS",
            "PR_FACTS",
            "SETUP_REPORT",
            "SMOKE_BRANCH",
            "BEFORE_SHA",
            "AFTER_SHA",
            "BASE_BRANCH",
        ] {
            assert!(!prompt.contains(token), "{token} left unsubstituted");
        }
    }

    #[test]
    fn smoke_test_worktree_is_named_after_the_pr_branch() {
        assert_eq!(
            smoke_test_worktree_name("fix/wasm-exception-handling"),
            "smoke_test_fix-wasm-exception-handling"
        );
        assert_eq!(smoke_test_worktree_name("feature"), "smoke_test_feature");
    }

    #[test]
    fn smoke_reformat_prompt_carries_the_contract_and_the_previous_output() {
        let prompt = build_smoke_reformat_prompt("ran it, crash before, 4 after");
        assert!(prompt.contains("===WISETREE-SMOKE-BEGIN===\n---OUTCOME---"));
        assert!(prompt.contains("ran it, crash before, 4 after"));
        assert!(!prompt.contains("PR_FACTS"));
        assert!(!prompt.contains("OUTPUT_CONTRACT"));
    }

    #[test]
    fn smoke_setup_report_lists_copies_and_post_create_results() {
        use crate::files::{CommandRun, CopyReport};
        let outcome = CreateOutcome {
            copy_report: Some(CopyReport {
                copied: vec![".env".to_string()],
                skipped: Vec::new(),
                errors: Vec::new(),
            }),
            command_runs: vec![
                CommandRun {
                    command: "bundle install".to_string(),
                    success: true,
                    output: String::new(),
                    error: None,
                },
                CommandRun {
                    command: "npm ci".to_string(),
                    success: false,
                    output: "npm ERR! missing lockfile".to_string(),
                    error: None,
                },
            ],
            ..CreateOutcome::default()
        };
        assert_eq!(
            smoke_test_setup_report(&outcome),
            "- Copied from the main checkout: `.env`\n\
             - Post-create command `bundle install`: succeeded\n\
             - Post-create command `npm ci`: FAILED (npm ERR! missing lockfile)"
        );
        assert_eq!(
            smoke_test_setup_report(&CreateOutcome::default()),
            "- No copy patterns or post-create commands are configured."
        );
    }

    #[test]
    fn humane_summary_prompt_lists_posted_wording_and_the_smoke_outcome() {
        let posted = [finding(Some(255), None)];
        let prompt = build_humane_summary_prompt(&posted, Some(&smoke()), "Fix WASM");
        assert!(prompt.contains("Pull request title: Fix WASM"));
        assert!(prompt
            .contains("1. [High] [Test] Assertion accepts any error (tests/evalraw.test.js:255)"));
        assert!(prompt.contains("Posted comment: `.toThrow()` with no argument"));
        assert!(prompt.contains("Outcome: as described (The crash is gone.)"));
        assert!(prompt.contains("Before:\nIt crashes."));
        let bare = build_humane_summary_prompt(&[], None, "Fix WASM");
        assert!(bare.contains("Approved findings: none"));
        assert!(bare.contains("Smoke test: not available."));
    }

    fn summary_block(body: &str) -> String {
        format!(
            "> read tool output\n===WISETREE-SUMMARY-BEGIN===\n{body}\n===WISETREE-SUMMARY-END===\n"
        )
    }

    #[test]
    fn humane_summary_overview_keeps_only_the_marker_block() {
        assert_eq!(
            validate_humane_summary_overview(&summary_block("Nice fix 👏")).as_deref(),
            Some("Nice fix 👏")
        );
        // Prose without the block could be CLI noise; never post it.
        assert_eq!(
            validate_humane_summary_overview("Here you go: Nice fix"),
            None
        );
    }

    #[test]
    fn humane_summary_overview_drops_a_self_written_smoke_section() {
        let overview = "Nice fix 👏\n\n> [!NOTE]\n> **Slower.**\n\n# Smoke Test\n\nmade up";
        assert_eq!(
            validate_humane_summary_overview(&summary_block(overview)).as_deref(),
            Some("Nice fix 👏\n\n> [!NOTE]\n> **Slower.**")
        );
        assert_eq!(
            validate_humane_summary_overview(&summary_block("## Review Summary\nx")),
            None
        );
        assert_eq!(
            validate_humane_summary_overview(&summary_block("   ")),
            None
        );
    }

    #[test]
    fn humane_summary_appends_the_smoke_test_after_the_opening() {
        let body = build_humane_review_summary("Nice fix 👏", Some(&smoke()));
        assert!(body.starts_with("Nice fix 👏\n\n# Smoke Test\n\n## Steps"));
        assert_eq!(
            build_humane_review_summary(" Left one comment inline. ", None),
            "Left one comment inline."
        );
        assert_eq!(deterministic_humane_overview(0), "Looks good to me 👍");
        assert_eq!(
            deterministic_humane_overview(3),
            "Left a few comments inline."
        );
    }

    // ── token savings ───────────────────────────────────────────────────

    #[test]
    fn humanize_and_revision_share_one_voice() {
        let prompt = build_humanize_prompt(&[finding(Some(1), None)]);
        assert!(prompt.contains("## How a human reviewer writes"));
        assert!(prompt.contains("## Words and habits that give a bot away"));
        assert!(!prompt.contains("VOICE_RULES"));

        let humane = revision_voice_section(ReviewVoice::Humane);
        assert!(humane.starts_with("## Voice\n\nThe EXPLANATION is posted verbatim"));
        assert!(humane.contains("## Words and habits that give a bot away"));
        assert_eq!(revision_voice_section(ReviewVoice::Robotic), "");
    }

    #[test]
    fn smoke_test_is_skipped_only_when_nothing_runnable_changed() {
        let paths = |list: &[&str]| list.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        for skip in [
            paths(&["spec/models/user_spec.rb", "tests/evalraw.test.js"]),
            paths(&["README.md", "docs/setup/install.txt", "CHANGELOG"]),
            paths(&[
                ".github/workflows/ci.yml",
                ".gitlab-ci.yml",
                "src/lib_test.go",
            ]),
        ] {
            assert!(smoke_test_skip_reason(&skip).is_some(), "{skip:?}");
        }
        for run in [
            paths(&["README.md", "src/parser.rs"]),
            // A dependency bump can change behavior.
            paths(&["package-lock.json"]),
            paths(&["requirements.txt"]),
            paths(&["assets/logo.png"]),
            Vec::new(),
        ] {
            assert_eq!(smoke_test_skip_reason(&run), None, "{run:?}");
        }
    }

    #[test]
    fn smoke_test_cache_reuses_a_finished_run_for_the_same_commits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("review").join("smoke_tests.json");
        assert_eq!(load_cached_smoke_test(&file, "a", "b"), None);

        store_smoke_test(&file, "a", "b", &smoke()).unwrap();
        assert_eq!(load_cached_smoke_test(&file, "a", "b"), Some(smoke()));
        // Another commit on either side is a different smoke test.
        assert_eq!(load_cached_smoke_test(&file, "a", "c"), None);
        assert_eq!(load_cached_smoke_test(&file, "z", "b"), None);

        // Re-storing the same pair replaces it instead of piling up.
        let newer = ReviewSmokeTest {
            outcome: Some(SmokeOutcome::Regression),
            ..smoke()
        };
        store_smoke_test(&file, "a", "b", &newer).unwrap();
        assert_eq!(load_cached_smoke_test(&file, "a", "b"), Some(newer));
        let json = std::fs::read_to_string(&file).unwrap();
        assert_eq!(json.matches("\"beforeSha\"").count(), 1);
        assert!(json.contains("\"outcome\": \"regression\""), "{json}");
    }

    #[test]
    fn inconclusive_smoke_tests_are_not_cached_and_the_cache_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("smoke_tests.json");
        let blocked = ReviewSmokeTest {
            outcome: Some(SmokeOutcome::Inconclusive),
            ..smoke()
        };
        store_smoke_test(&file, "a", "b", &blocked).unwrap();
        assert_eq!(load_cached_smoke_test(&file, "a", "b"), None);

        for i in 0..SMOKE_TEST_CACHE_LIMIT + 3 {
            store_smoke_test(&file, "base", &format!("head{i}"), &smoke()).unwrap();
        }
        assert_eq!(load_cached_smoke_test(&file, "base", "head0"), None);
        assert!(load_cached_smoke_test(&file, "base", "head3").is_some());
        let last = format!("head{}", SMOKE_TEST_CACHE_LIMIT + 2);
        assert!(load_cached_smoke_test(&file, "base", &last).is_some());

        std::fs::write(&file, "not json").unwrap();
        assert_eq!(load_cached_smoke_test(&file, "base", &last), None);
    }

    #[test]
    fn smoke_facts_inline_small_diffs_and_list_oversized_ones() {
        let small = "diff --git a/build.sh b/build.sh\n--- a/build.sh\n+++ b/build.sh\n@@ -1 +1 @@\n-old\n+new\n";
        let generated = format!(
            "diff --git a/wasm/bundle.js b/wasm/bundle.js\n{}",
            "+x\n".repeat(SMOKE_FACTS_FILE_DIFF_LIMIT)
        );
        let facts = build_smoke_test_facts(
            " build.sh | 2 +-",
            &format!("{small}{generated}"),
            "-2b9c1e0 equations-parser (v1.0)",
            &[
                ("node".to_string(), "v22.4.0".to_string()),
                ("npm".to_string(), "not installed".to_string()),
            ],
            &[(
                "package.json".to_string(),
                "{\"name\": \"app\"}".to_string(),
            )],
        );
        assert!(facts.starts_with("#### Diff stat\n\n~~~text\n build.sh | 2 +-\n~~~"));
        assert!(facts.contains("~~~diff\ndiff --git a/build.sh b/build.sh"));
        assert!(facts.contains("-old\n+new"));
        assert!(
            !facts.contains("+x\n+x"),
            "the generated bundle is not inlined"
        );
        assert!(facts.contains(&format!(
            "- `wasm/bundle.js` ({} diff lines)",
            SMOKE_FACTS_FILE_DIFF_LIMIT + 1
        )));
        assert!(facts.contains(
            "-2b9c1e0 equations-parser (v1.0)\n~~~ (a leading `-` means not initialized)"
        ));
        assert!(facts.contains("- `node`: v22.4.0\n- `npm`: not installed"));
        assert!(facts.contains("#### `package.json`\n\n~~~\n{\"name\": \"app\"}\n~~~"));

        let empty = build_smoke_test_facts("", "", "", &[], &[]);
        assert!(empty.contains("#### Submodules\n\nnone"));
        assert!(empty.contains("No root manifest names a runtime."));
    }
}
