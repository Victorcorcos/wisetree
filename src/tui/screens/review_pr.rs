//! "Review Pull Request" screen. Scans the PR's changed files with one
//! captured AI call per file — a small pool of files in parallel, test files
//! with a dedicated test-quality prompt, plus one whole-diff pass that alone
//! owns test-coverage findings — then walks the findings one at a time and
//! posts the approved ones as PR comments. State machine:
//!
//! - `Confirm`   : explanation panel + `ConfirmationModal` (Yes/No, **No**
//!   default). Enter on Yes returns `ReviewAction::Confirmed`.
//! - `Working`   : a quiet spinner + step toast for the captured/deterministic
//!   phases the `App` drives (syncing + fetching the diff, posting a comment,
//!   revising a finding, submitting the summary). The multi-pass scan instead
//!   renders a live progress dashboard: a `Scan → Audit → Verify` stepper, a
//!   per-stage progress bar, an "Under review" panel naming the files/passes
//!   in flight, and a running per-severity findings tally.
//! - `Decision`  : one finding at a time — category/severity badge, the exact
//!   comment body that would be posted, then native **Post / Edit / Other /
//!   Skip** buttons.
//! - `EditFinding`: deterministic, AI-free editing of the current finding —
//!   severity cycle, title/explanation text fields, suggestion keep/remove —
//!   with a live preview of the comment. Entirely local to this screen (the
//!   `App` never hears about it); `S` saves back into the finding, Esc
//!   cancels. This absorbs the mechanical revisions ("lower the severity",
//!   "reword the title") that would otherwise pay for an "Other" AI call.
//! - `OtherInput`: freeform feedback box (the "Other" path); submitting
//!   returns `ReviewAction::Revise(feedback)` so the `App` re-scans that one
//!   finding.
//! - `Summary`   : the deterministic review-summary markdown built from the
//!   posted findings, with **Request changes / Comment / Skip** buttons.
//! - `Done`      : a results table (one row per finding) mirroring the Fix
//!   Done page.
//!
//! All async + git/gh/AI work is owned by `App`; this screen is a presentation
//! state machine that records per-finding outcomes for the final table. The
//! AI never posts anything — every `gh` call happens after an explicit user
//! choice here.

use std::cell::Cell;
use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::schema::AiReviewConfig;
use crate::messages::colors;
use crate::services::dashboard::{
    review_coverage_groups, review_file_groups, split_duplicate_findings,
    split_run_duplicate_findings, ReviewContext, ReviewFile, ReviewFileGroup, ReviewFinding,
    ReviewGroupProfile, ReviewScanMode, ReviewSeverity, ReviewSkippedFile, ReviewVerification,
};
use crate::services::review_telemetry::{review_telemetry_label, ReviewScanTelemetry};
use crate::tui::screens::dashboard::ReviewPullRequestRequest;
use crate::tui::screens::update_pr::{button_paragraph, contains_position};
use crate::tui::widgets::spinner::spinner_frame;
use crate::tui::widgets::{
    labeled_line, render_summary_table, AiRoleRow, ConfirmationChoice, ConfirmationModal,
    ConfirmationOutcome, InputOutcome, InputPrompt, PrConfirmView, Status, StatusIndicator,
    SummaryRow,
};

/// First synthetic `file_index` for coverage-group scans. Further groups use
/// descending values, keeping them disjoint from ordinary file indices.
pub const COVERAGE_SCAN_INDEX: usize = usize::MAX;
const FILE_GROUP_SCAN_INDEX: usize = usize::MAX / 2;

fn coverage_scan_index(group_index: usize) -> usize {
    COVERAGE_SCAN_INDEX - group_index
}

/// Label for the coverage scan in the in-flight panel and report rows.
const COVERAGE_SCAN_LABEL: &str = "coverage";
const MERGED_SCAN_LABEL: &str = "merged review";

/// The three passes the Review pipeline runs, in order. Drives the progress
/// stepper and the per-stage bar on the Working view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipelineStage {
    Scan,
    Audit,
    Verify,
}

/// One stepper cell's state, mirroring the post-create command list icons
/// (`✓` done, spinner active, `○` pending, `–` skipped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageState {
    Done,
    Active,
    Pending,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStep {
    Confirm,
    /// Deterministic / captured phase (sync, scan, post, revise, submit) — a
    /// quiet spinner with a step message; never an embedded PTY (the review
    /// AI only reads, so there is nothing to watch live).
    Working,
    Decision,
    /// Deterministic edit form for the current finding — no AI, no tokens.
    EditFinding,
    OtherInput,
    /// The deterministic review summary + Request changes / Comment / Skip.
    Summary,
    Done,
}

/// The four native decision buttons for one finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecisionButton {
    Post,
    Edit,
    Other,
    Skip,
}

/// The editable rows of the `EditFinding` form, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditRow {
    Severity,
    Title,
    Explanation,
    Suggestion,
}

const EDIT_ROWS: [EditRow; 4] = [
    EditRow::Severity,
    EditRow::Title,
    EditRow::Explanation,
    EditRow::Suggestion,
];

/// Working state of the `EditFinding` form: a draft copy of the finding the
/// user mutates field by field. `S` commits the draft back into the
/// walkthrough, Esc throws it away — the original finding is never touched
/// until save.
struct EditState {
    draft: ReviewFinding,
    /// A removed suggestion is parked here so the toggle can restore it.
    removed_suggestion: Option<String>,
    row: EditRow,
    /// Open text editor for Title (single-line) or Explanation (multiline).
    input: Option<InputPrompt>,
    row_rects: Cell<[Rect; 4]>,
}

impl EditState {
    fn new(finding: ReviewFinding) -> Self {
        Self {
            draft: finding,
            removed_suggestion: None,
            row: EditRow::Severity,
            input: None,
            row_rects: Cell::new([Rect::default(); 4]),
        }
    }

    /// Cycle the draft severity: ←/→ move through Critical…Low.
    fn cycle_severity(&mut self, forward: bool) {
        const ORDER: [ReviewSeverity; 4] = [
            ReviewSeverity::Critical,
            ReviewSeverity::High,
            ReviewSeverity::Medium,
            ReviewSeverity::Low,
        ];
        let at = ORDER
            .iter()
            .position(|s| *s == self.draft.severity)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % ORDER.len()
        } else {
            (at + ORDER.len() - 1) % ORDER.len()
        };
        self.draft.severity = ORDER[next];
    }

    /// Keep ↔ remove the suggestion block. A finding that never carried one
    /// has nothing to toggle.
    fn toggle_suggestion(&mut self) {
        if self.draft.suggestion.is_some() {
            self.removed_suggestion = self.draft.suggestion.take();
        } else if self.removed_suggestion.is_some() {
            self.draft.suggestion = self.removed_suggestion.take();
        }
    }
}

/// The three summary buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryButton {
    RequestChanges,
    Comment,
    Skip,
}

/// Outcome recorded for one finding, turned into a summary-table row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewRowOutcome {
    /// Comment posted on the PR.
    Posted,
    /// The user chose Skip — nothing was posted.
    Skipped,
    /// Posting (or revising) broke. Message is included.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewAction {
    Continue,
    /// Esc / No on Confirm — abort and return to the dashboard.
    Cancelled,
    /// Confirm panel accepted — start the review pipeline.
    Confirmed,
    /// Decision: post this finding's comment on the PR.
    Post,
    /// Decision: open the freeform "Other" feedback box.
    Other,
    /// Decision: skip this finding, move on.
    Skip,
    /// OtherInput submitted — revise the current finding with this feedback.
    Revise(String),
    /// Summary: submit via `gh pr review` (blocking when `request_changes`).
    SubmitSummary {
        request_changes: bool,
    },
    /// Summary: post nothing, go to the final report.
    SkipSummary,
    /// Done page: a key was pressed; caller returns to the dashboard.
    Done,
}

pub struct ReviewPullRequestScreen {
    request: ReviewPullRequestRequest,
    /// Resolved `ai.review` profiles, shown on the confirm panel's AI table.
    ai: AiReviewConfig,
    confirm: Option<ConfirmationModal>,
    phase_message: String,
    /// True only during the per-file scan phase, so the Working view shows
    /// the file under review below the spinner. Cleared by every other
    /// Working transition.
    scanning: bool,
    /// Repository slug + PR head sha resolved during preparation; used by
    /// `App` for the comment-posting API calls.
    owner: String,
    repo: String,
    head_sha: String,
    /// The changed files to scan, in diff order. Empty until preparation.
    /// Kept whole so an "Other" revision can re-render the file's prompt.
    files: Vec<ReviewFile>,
    context: ReviewContext,
    /// Small diffs combine application review and coverage in the sentinel
    /// call; large diffs retain per-file application scans plus coverage.
    scan_mode: ReviewScanMode,
    /// Budgeted groups in dispatch order: tester groups first, then split-mode
    /// application groups. `next_scan` indexes this list.
    scan_groups: Vec<(usize, ReviewFileGroup)>,
    next_scan: usize,
    tester_scans_total: usize,
    tester_scans_done: usize,
    tester_findings: Vec<ReviewFinding>,
    /// Deterministic coverage groups. Application-file sets are disjoint;
    /// changed tests are repeated as evidence. Empty for tests-only diffs.
    coverage_groups: Vec<Vec<ReviewFile>>,
    next_coverage_group: usize,
    /// Paths currently being scanned in parallel — the Working panel's
    /// content while the scan phase runs.
    in_flight: Vec<String>,
    /// Which pipeline pass the Working view is currently showing (scan / audit
    /// / verify), so the stepper and progress bar stay honest across phases.
    stage: PipelineStage,
    /// Whether the cross-group omission audit is expected to run this review,
    /// captured when scanning begins so the stepper can show it as a real
    /// upcoming stage (or as skipped) rather than guessing.
    plan_audit: bool,
    /// Total findings queued for high-risk verification, so the Verify bar can
    /// show progress as `verification_outstanding` drains.
    verification_total: usize,
    /// Files whose scan reached a terminal state (parsed or failed twice).
    scans_done: usize,
    /// Findings aggregated across every scanned file, sorted by severity
    /// once scanning completes.
    findings: Vec<ReviewFinding>,
    skipped_files: Vec<ReviewSkippedFile>,
    gap_audit_started: bool,
    audit_finding_titles: BTreeSet<String>,
    verification_outstanding: BTreeSet<usize>,
    verification_results: Vec<Option<ReviewFinding>>,
    /// Index of the finding currently on the Decision step.
    current: usize,
    /// Findings actually posted, in posting order — the summary's input.
    posted: Vec<ReviewFinding>,
    /// The deterministic summary markdown, built when the walkthrough ends.
    summary_body: String,
    decision_button: DecisionButton,
    decision_button_rects: Cell<[Rect; 4]>,
    /// Live state of the deterministic edit form; `Some` only on
    /// `EditFinding`.
    edit: Option<EditState>,
    summary_button: SummaryButton,
    summary_button_rects: Cell<[Rect; 3]>,
    /// Scroll offset for the (potentially long) comment preview / summary.
    decision_scroll: u16,
    /// Max scroll offset from the last render, so scrolling can't overshoot
    /// the bottom and leave the reverse direction feeling dead.
    decision_max_scroll: Cell<u16>,
    other_input: Option<InputPrompt>,
    // ── results ─────────────────────────────────────────────────────────
    summary_rows: Vec<SummaryRow>,
    scan_telemetry: Vec<ReviewScanTelemetry>,
    telemetry_reported: bool,
    error: Option<String>,
    step: ReviewStep,
    pub tick: usize,
}

impl ReviewPullRequestScreen {
    pub fn new(request: ReviewPullRequestRequest, ai: AiReviewConfig) -> Self {
        Self {
            confirm: Some(build_confirm(&request)),
            request,
            ai,
            phase_message: String::new(),
            scanning: false,
            owner: String::new(),
            repo: String::new(),
            head_sha: String::new(),
            files: Vec::new(),
            context: ReviewContext::default(),
            scan_mode: ReviewScanMode::Split,
            scan_groups: Vec::new(),
            next_scan: 0,
            tester_scans_total: 0,
            tester_scans_done: 0,
            tester_findings: Vec::new(),
            coverage_groups: Vec::new(),
            next_coverage_group: 0,
            in_flight: Vec::new(),
            stage: PipelineStage::Scan,
            plan_audit: false,
            verification_total: 0,
            scans_done: 0,
            findings: Vec::new(),
            skipped_files: Vec::new(),
            gap_audit_started: false,
            audit_finding_titles: BTreeSet::new(),
            verification_outstanding: BTreeSet::new(),
            verification_results: Vec::new(),
            current: 0,
            posted: Vec::new(),
            summary_body: String::new(),
            decision_button: DecisionButton::Post,
            decision_button_rects: Cell::new([Rect::default(); 4]),
            edit: None,
            summary_button: SummaryButton::Comment,
            summary_button_rects: Cell::new([Rect::default(); 3]),
            decision_scroll: 0,
            decision_max_scroll: Cell::new(0),
            other_input: None,
            summary_rows: Vec::new(),
            scan_telemetry: Vec::new(),
            telemetry_reported: false,
            error: None,
            step: ReviewStep::Confirm,
            tick: 0,
        }
    }

    // ── accessors used by App ───────────────────────────────────────────

    pub fn request(&self) -> &ReviewPullRequestRequest {
        &self.request
    }
    pub fn step(&self) -> ReviewStep {
        self.step
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn repo(&self) -> &str {
        &self.repo
    }
    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }
    pub fn files_len(&self) -> usize {
        self.files.len()
    }
    pub fn file_at(&self, index: usize) -> Option<ReviewFile> {
        self.files.get(index).cloned()
    }
    /// True while per-file scans may still deliver results — the App drops
    /// any scan event arriving outside this window.
    pub fn scan_phase_active(&self) -> bool {
        self.scanning
    }
    pub fn findings_len(&self) -> usize {
        self.findings.len()
    }
    pub fn current_index(&self) -> usize {
        self.current
    }
    pub fn current_finding(&self) -> Option<ReviewFinding> {
        self.findings.get(self.current).cloned()
    }
    /// The scanned file a finding belongs to — an "Other" revision derives
    /// its focused hunk and optional inlined content from this snapshot.
    pub fn file_for(&self, finding: &ReviewFinding) -> Option<ReviewFile> {
        self.files.iter().find(|f| f.path == finding.file).cloned()
    }
    pub fn posted_findings(&self) -> &[ReviewFinding] {
        &self.posted
    }
    pub fn summary_body(&self) -> &str {
        &self.summary_body
    }
    /// The expanded (full-height) steps want the whole bottom region; the
    /// compact ones (Working / Done) render in a sized panel.
    pub fn wants_full_panel(&self) -> bool {
        matches!(
            self.step,
            ReviewStep::Confirm
                | ReviewStep::Decision
                | ReviewStep::EditFinding
                | ReviewStep::OtherInput
                | ReviewStep::Summary
        )
    }

    // ── App-driven transitions ──────────────────────────────────────────

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    /// Confirm → Working: the App kicks off `prepare_review`.
    pub fn start_preparing(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = "Syncing the branch and fetching the PR diff...".to_string();
        self.scanning = false;
        self.confirm = None;
    }

    /// Store the prepared files + repo context and reset the scan pool.
    pub fn set_files(&mut self, files: Vec<ReviewFile>, owner: String, repo: String, sha: String) {
        self.files = files;
        self.owner = owner;
        self.repo = repo;
        self.head_sha = sha;
        self.rebuild_scan_order();
        self.next_scan = 0;
        self.tester_scans_total = self
            .scan_groups
            .iter()
            .filter(|(_, group)| group.profile == ReviewGroupProfile::Tester)
            .count();
        self.tester_scans_done = 0;
        self.tester_findings.clear();
        self.rebuild_coverage_groups();
        self.in_flight.clear();
        self.scans_done = 0;
        self.findings.clear();
        self.posted.clear();
        self.scan_telemetry.clear();
        self.telemetry_reported = false;
    }

    pub fn set_scan_mode(&mut self, scan_mode: ReviewScanMode) {
        self.scan_mode = scan_mode;
        self.rebuild_scan_order();
        self.rebuild_coverage_groups();
    }

    pub fn set_review_context(&mut self, context: ReviewContext) {
        self.context = context;
    }

    pub fn review_context(&self) -> ReviewContext {
        self.context.clone()
    }

    pub fn scan_mode(&self) -> ReviewScanMode {
        self.scan_mode
    }

    /// Working step for the parallel scan phase: the spinner message tracks
    /// completed files and the panel below lists the files being scanned.
    pub fn begin_scan_phase(&mut self) {
        self.step = ReviewStep::Working;
        self.scanning = true;
        self.stage = PipelineStage::Scan;
        self.plan_audit = self.should_run_gap_audit();
        self.refresh_scan_message();
    }

    fn refresh_scan_message(&mut self) {
        let combined = if !self.coverage_groups.is_empty() {
            match self.scan_mode {
                ReviewScanMode::Merged => " + merged review",
                ReviewScanMode::Split => " + test coverage",
            }
        } else {
            ""
        };
        let files = self.files.len();
        self.phase_message = format!(
            "Reviewing {files} changed file{}{combined}",
            if files == 1 { "" } else { "s" },
        );
    }

    /// Hand out the next focus-budget group, tracking it as in-flight.
    pub(crate) fn take_next_scan_file(&mut self) -> Option<(usize, ReviewFileGroup)> {
        let (index, group) = self.scan_groups.get(self.next_scan)?.clone();
        self.next_scan += 1;
        self.in_flight.push(self.scan_group_label(&group));
        Some((index, group))
    }

    /// Hand out the next deterministic coverage group after tester scans
    /// settle. The synthetic index keeps retry/failure accounting scoped to
    /// the group that produced the event.
    pub fn take_coverage_scan(&mut self) -> Option<(usize, Vec<ReviewFile>)> {
        if self.tester_scans_done < self.tester_scans_total {
            return None;
        }
        let group_index = self.next_coverage_group;
        let files = self.coverage_groups.get(group_index)?.clone();
        self.next_coverage_group += 1;
        let scan_index = coverage_scan_index(group_index);
        self.in_flight.push(self.coverage_scan_label(group_index));
        Some((scan_index, files))
    }

    /// Recover a dispatched coverage group for a reformat/full retry.
    pub fn coverage_group(&self, scan_index: usize) -> Option<Vec<ReviewFile>> {
        let group_index = COVERAGE_SCAN_INDEX.checked_sub(scan_index)?;
        self.coverage_groups.get(group_index).cloned()
    }

    /// One scan reached a terminal state (result recorded or failed twice):
    /// update the progress message and the in-flight panel.
    pub fn note_scan_done(&mut self, file_index: usize) {
        self.scans_done += 1;
        if self.coverage_group_index(file_index).is_none()
            && self
                .scan_group(file_index)
                .is_some_and(|group| group.profile == ReviewGroupProfile::Tester)
        {
            self.tester_scans_done += 1;
        }
        let label = if let Some(group_index) = self.coverage_group_index(file_index) {
            Some(self.coverage_scan_label(group_index))
        } else {
            self.scan_group(file_index)
                .map(|group| self.scan_group_label(&group))
        };
        if let Some(label) = label {
            if let Some(pos) = self.in_flight.iter().position(|p| p == &label) {
                self.in_flight.remove(pos);
            }
        }
        self.refresh_scan_message();
    }

    /// True while some scan (a file's or the coverage pass) hasn't reached
    /// its terminal state yet.
    pub fn scans_pending(&self) -> bool {
        self.scans_done < self.scan_units_total()
    }

    fn scan_units_total(&self) -> usize {
        self.scan_groups.len() + self.coverage_groups.len()
    }

    fn rebuild_scan_order(&mut self) {
        self.scan_groups = review_file_groups(&self.files, self.scan_mode)
            .into_iter()
            .enumerate()
            .map(|(group_index, group)| {
                let scan_index = if group.files.len() == 1 {
                    self.files
                        .iter()
                        .position(|file| file.path == group.files[0].path)
                        .unwrap_or(FILE_GROUP_SCAN_INDEX - group_index)
                } else {
                    FILE_GROUP_SCAN_INDEX - group_index
                };
                (scan_index, group)
            })
            .collect();
    }

    fn rebuild_coverage_groups(&mut self) {
        self.coverage_groups = review_coverage_groups(&self.files, self.scan_mode);
        self.next_coverage_group = 0;
    }

    pub fn record_tester_findings(&mut self, file_index: usize, findings: &[ReviewFinding]) {
        if self
            .scan_group(file_index)
            .is_some_and(|group| group.profile == ReviewGroupProfile::Tester)
        {
            self.tester_findings.extend_from_slice(findings);
        }
    }

    pub fn tester_findings(&self) -> Vec<ReviewFinding> {
        self.tester_findings.clone()
    }

    fn coverage_group_index(&self, scan_index: usize) -> Option<usize> {
        let group_index = COVERAGE_SCAN_INDEX.checked_sub(scan_index)?;
        (group_index < self.coverage_groups.len()).then_some(group_index)
    }

    fn coverage_scan_label(&self, group_index: usize) -> String {
        if self.scan_mode == ReviewScanMode::Merged {
            MERGED_SCAN_LABEL.to_string()
        } else {
            format!(
                "{COVERAGE_SCAN_LABEL} group {} of {}",
                group_index + 1,
                self.coverage_groups.len()
            )
        }
    }

    pub(crate) fn scan_group(&self, scan_index: usize) -> Option<ReviewFileGroup> {
        self.scan_groups
            .iter()
            .find(|(index, _)| *index == scan_index)
            .map(|(_, group)| group.clone())
    }

    fn scan_group_label(&self, group: &ReviewFileGroup) -> String {
        let prefix = match group.profile {
            ReviewGroupProfile::Application => "app",
            ReviewGroupProfile::Tester => "tests",
        };
        format!(
            "{prefix}: {}",
            group
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    /// Deterministic dedup of one scan's findings against the wisetree
    /// comments already on the PR. A per-file scan checks its own file's
    /// keys; the coverage scan's findings span files, so each is checked
    /// against the keys of the file it targets.
    pub fn split_existing_duplicates(
        &self,
        file_index: usize,
        findings: Vec<ReviewFinding>,
    ) -> (Vec<ReviewFinding>, Vec<ReviewFinding>) {
        if self.coverage_group_index(file_index).is_none() {
            let Some(group) = self.scan_group(file_index) else {
                return (findings, Vec::new());
            };
            if group.files.len() == 1 {
                return split_duplicate_findings(findings, &group.files[0].existing_keys);
            }
        }
        let mut fresh = Vec::new();
        let mut duplicates = Vec::new();
        for finding in findings {
            let keys = self
                .files
                .iter()
                .find(|f| f.path == finding.file)
                .map(|f| f.existing_keys.as_slice())
                .unwrap_or(&[]);
            let (f, d) = split_duplicate_findings(vec![finding], keys);
            fresh.extend(f);
            duplicates.extend(d);
        }
        (fresh, duplicates)
    }

    /// Files the deterministic filter excluded before any AI call: one
    /// muted row each on the final report, reason in the status label —
    /// the user sees why a changed file never produced findings.
    pub fn record_skipped_files(&mut self, skipped: &[ReviewSkippedFile]) {
        self.skipped_files.extend_from_slice(skipped);
        for file in skipped {
            self.summary_rows.push(SummaryRow::with_status(
                format!("skip {}", file.path),
                format!("Skipped ({})", file.reason),
                colors::MUTED,
                None,
            ));
        }
    }

    /// Findings the deterministic dedup dropped because the PR already
    /// carries them as a wisetree comment: one muted row each on the final
    /// report — the walkthrough shrinks, but never silently.
    pub fn record_duplicate_findings(&mut self, duplicates: &[ReviewFinding]) {
        for finding in duplicates {
            self.summary_rows.push(SummaryRow::with_status(
                format!("dedup {}", finding.descriptor()),
                "Already posted",
                colors::MUTED,
                None,
            ));
        }
    }

    /// Findings collapsed as same-run duplicates (a second finding proposing
    /// the same fix, or on the same line, as one already kept): one muted row
    /// each, so the walkthrough shrinks visibly rather than silently.
    fn record_run_duplicate_findings(&mut self, duplicates: &[ReviewFinding]) {
        for finding in duplicates {
            self.summary_rows.push(SummaryRow::with_status(
                format!("dedup {}", finding.descriptor()),
                "Duplicate",
                colors::MUTED,
                None,
            ));
        }
    }

    /// Fold one file's findings into the aggregate.
    pub fn record_scan_result(&mut self, findings: Vec<ReviewFinding>) {
        self.findings.extend(findings);
    }

    pub fn record_scan_telemetry(&mut self, telemetry: ReviewScanTelemetry) {
        self.scan_telemetry.push(telemetry);
    }

    #[cfg(test)]
    pub fn scan_telemetry_len(&self) -> usize {
        self.scan_telemetry.len()
    }

    /// A scan that failed twice gets its own Failed row and the pool moves
    /// on — one bad file (or the coverage pass) never aborts the whole
    /// review.
    pub fn record_scan_failure(&mut self, file_index: usize, message: String) {
        let path = if let Some(group_index) = self.coverage_group_index(file_index) {
            self.coverage_scan_label(group_index)
        } else if let Some(group) = self.scan_group(file_index) {
            self.scan_group_label(&group)
        } else {
            String::new()
        };
        self.summary_rows.push(SummaryRow::with_status(
            format!("scan {path}"),
            "Failed",
            colors::ERROR,
            Some(message),
        ));
    }

    pub fn should_run_gap_audit(&self) -> bool {
        !self.gap_audit_started
            && self.scan_mode == ReviewScanMode::Split
            && self.coverage_groups.len() > 1
            && self
                .files
                .iter()
                .any(|file| !crate::services::dashboard::review_file_is_test(file))
    }

    pub fn begin_gap_audit(&mut self) {
        self.gap_audit_started = true;
        self.scanning = true;
        self.stage = PipelineStage::Audit;
        self.phase_message =
            "Cross-checking every group for issues a single pass might miss".to_string();
    }

    pub fn gap_audit_inputs(
        &self,
    ) -> (
        String,
        Vec<ReviewFile>,
        ReviewContext,
        String,
        Vec<ReviewSkippedFile>,
        Vec<ReviewFinding>,
    ) {
        let edges = self
            .scan_groups
            .iter()
            .filter_map(|(_, group)| {
                (!group.relationship_summary.is_empty())
                    .then_some(group.relationship_summary.as_str())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join("\n");
        (
            self.request.worktree_path.clone(),
            self.files.clone(),
            self.context.clone(),
            edges,
            self.skipped_files.clone(),
            self.findings.clone(),
        )
    }

    pub fn record_gap_audit_result(&mut self, result: Result<Vec<ReviewFinding>, String>) {
        self.scanning = false;
        match result {
            Ok(findings) => {
                let existing = self
                    .findings
                    .iter()
                    .map(|finding| {
                        (
                            finding.file.clone(),
                            finding.line,
                            finding.title.to_ascii_lowercase(),
                        )
                    })
                    .collect::<BTreeSet<_>>();
                for finding in findings {
                    let key = (
                        finding.file.clone(),
                        finding.line,
                        finding.title.to_ascii_lowercase(),
                    );
                    if existing.contains(&key) {
                        self.record_run_duplicate_findings(std::slice::from_ref(&finding));
                        continue;
                    }
                    self.audit_finding_titles
                        .insert(finding.title.to_ascii_lowercase());
                    self.findings.push(finding);
                }
            }
            Err(message) => self.summary_rows.push(SummaryRow::with_status(
                "global omission audit",
                "Failed — primary findings kept",
                colors::WARNING,
                Some(message),
            )),
        }
    }

    /// Scanning finished: sort the aggregate by severity (Critical first)
    /// and by diff order within a severity, so the walkthrough order is
    /// deterministic even though parallel scans finish in arbitrary order.
    /// Returns `true` when at least one finding awaits the walkthrough.
    pub fn finish_scanning(&mut self) -> bool {
        self.scanning = false;
        self.in_flight.clear();
        let files = &self.files;
        self.findings.sort_by_key(|f| {
            let file_order = files
                .iter()
                .position(|x| x.path == f.file)
                .unwrap_or(usize::MAX);
            (f.severity.rank(), file_order)
        });
        // Collapse same-run duplicates the parallel per-file scans can emit
        // with different wording. Sorted above, so the first occurrence — the
        // highest-severity one — is kept and the rest become muted rows.
        let (kept, duplicates) = split_run_duplicate_findings(std::mem::take(&mut self.findings));
        self.findings = kept;
        self.record_run_duplicate_findings(&duplicates);
        self.current = 0;
        !self.findings.is_empty()
    }

    pub fn begin_verification(&mut self) -> Vec<(usize, ReviewFile, ReviewFinding, bool)> {
        self.verification_results = self.findings.iter().cloned().map(Some).collect();
        let mut candidates = Vec::new();
        for (index, finding) in self.findings.iter().enumerate() {
            if !self.finding_requires_verification(finding) {
                continue;
            }
            if let Some(file) = self.files.iter().find(|file| file.path == finding.file) {
                self.verification_outstanding.insert(index);
                candidates.push((
                    index,
                    file.clone(),
                    finding.clone(),
                    self.finding_requires_strong_verification(finding),
                ));
            }
        }
        if !candidates.is_empty() {
            self.scanning = true;
            self.stage = PipelineStage::Verify;
            self.verification_total = candidates.len();
            self.phase_message = format!(
                "Double-checking {} high-risk finding{} before you review",
                candidates.len(),
                if candidates.len() == 1 { "" } else { "s" },
            );
        }
        candidates
    }

    fn finding_requires_verification(&self, finding: &ReviewFinding) -> bool {
        finding.severity.rank() <= ReviewSeverity::High.rank()
            || finding.category.eq_ignore_ascii_case("security")
            || finding.line.is_none()
            || finding.suggestion.is_some()
            || self
                .audit_finding_titles
                .contains(&finding.title.to_ascii_lowercase())
            || self.scan_groups.iter().any(|(_, group)| {
                !group.relationship_summary.is_empty()
                    && group
                        .relationship_summary
                        .contains(&format!("`{}`", finding.file))
            })
    }

    fn finding_requires_strong_verification(&self, finding: &ReviewFinding) -> bool {
        matches!(
            finding.severity,
            ReviewSeverity::Critical | ReviewSeverity::High
        ) || finding.category.eq_ignore_ascii_case("security")
            || self
                .audit_finding_titles
                .contains(&finding.title.to_ascii_lowercase())
            || self.scan_groups.iter().any(|(_, group)| {
                !group.relationship_summary.is_empty()
                    && group
                        .relationship_summary
                        .contains(&format!("`{}`", finding.file))
            })
    }

    pub fn record_verification(
        &mut self,
        index: usize,
        result: Result<ReviewVerification, String>,
    ) {
        if !self.verification_outstanding.remove(&index) {
            return;
        }
        match result {
            Ok(ReviewVerification::Confirmed { .. }) => {}
            Ok(ReviewVerification::RejectedFalsePositive { reason }) => {
                self.verification_results[index] = None;
                self.summary_rows.push(SummaryRow::with_note(
                    format!("verify {}", self.findings[index].descriptor()),
                    "Rejected false positive",
                    colors::MUTED,
                    (!reason.is_empty()).then_some(reason),
                ));
            }
            Ok(ReviewVerification::Revise { reason, finding }) => {
                self.verification_results[index] = Some(finding);
                self.summary_rows.push(SummaryRow::with_note(
                    format!("verify {}", self.findings[index].descriptor()),
                    "Revised",
                    colors::EMPHASIS,
                    (!reason.is_empty()).then_some(reason),
                ));
            }
            Err(message) => {
                self.verification_results[index] = None;
                self.summary_rows.push(SummaryRow::with_status(
                    format!("verify {}", self.findings[index].descriptor()),
                    "Unverified — withheld",
                    colors::WARNING,
                    Some(message),
                ));
            }
        }
        self.phase_message = "Double-checking high-risk findings".to_string();
    }

    pub fn verification_pending(&self) -> bool {
        !self.verification_outstanding.is_empty()
    }

    pub fn finish_verification(&mut self) -> bool {
        self.scanning = false;
        self.findings = std::mem::take(&mut self.verification_results)
            .into_iter()
            .flatten()
            .collect();
        self.current = 0;
        !self.findings.is_empty()
    }

    /// Present the current finding with the Post / Edit / Other / Skip
    /// buttons.
    pub fn enter_decision(&mut self) {
        self.decision_button = DecisionButton::Post;
        self.decision_scroll = 0;
        self.other_input = None;
        self.edit = None;
        self.step = ReviewStep::Decision;
    }

    /// Open the deterministic edit form on a draft copy of the current
    /// finding. Local to the screen — no `App` involvement, no AI call.
    fn show_edit(&mut self) {
        let Some(finding) = self.findings.get(self.current) else {
            return;
        };
        self.edit = Some(EditState::new(finding.clone()));
        self.step = ReviewStep::EditFinding;
    }

    /// Commit the edit draft into the walkthrough and return to Decision.
    fn save_edit(&mut self) {
        if let Some(edit) = self.edit.take() {
            if let Some(slot) = self.findings.get_mut(self.current) {
                *slot = edit.draft;
            }
        }
        self.enter_decision();
    }

    /// Replace the current finding with its revision and re-enter Decision.
    pub fn show_revised(&mut self, finding: ReviewFinding) {
        if let Some(slot) = self.findings.get_mut(self.current) {
            *slot = finding;
        }
        self.enter_decision();
    }

    /// Re-enter Decision with the finding already in hand. Used when an
    /// "Other" revision fails (the model disobeyed or the call errored): the
    /// user keeps their place and the previous comment instead of being
    /// dropped out of the loop.
    pub fn reshow_decision(&mut self) {
        self.enter_decision();
    }

    /// Open the freeform "Other" feedback box.
    pub fn show_other_input(&mut self) {
        self.other_input = Some(
            InputPrompt::new("Tell the AI what to change about this comment:")
                .with_placeholder("e.g. soften the tone; target the helper instead"),
        );
        self.step = ReviewStep::OtherInput;
    }

    pub fn start_posting(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = "Posting the comment on the pull request...".to_string();
        self.scanning = false;
    }

    pub fn start_revising(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = "Revising the comment with your feedback...".to_string();
        self.scanning = false;
    }

    pub fn start_submitting_summary(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = "Submitting the review summary...".to_string();
        self.scanning = false;
    }

    /// Record a per-finding outcome as a colored summary-table row.
    pub fn record_outcome(&mut self, outcome: ReviewRowOutcome) {
        let n = self.current + 1;
        let descriptor = self
            .findings
            .get(self.current)
            .map(|f| f.descriptor())
            .unwrap_or_default();
        let command = format!("#{n} {descriptor}");
        let row = match outcome {
            ReviewRowOutcome::Posted => {
                if let Some(finding) = self.findings.get(self.current) {
                    self.posted.push(finding.clone());
                }
                SummaryRow::with_status(command, "Posted", colors::SUCCESS, None)
            }
            ReviewRowOutcome::Skipped => {
                SummaryRow::with_status(command, "Skipped", colors::WARNING, None)
            }
            ReviewRowOutcome::Failed(msg) => {
                SummaryRow::with_status(command, "Failed", colors::ERROR, Some(msg))
            }
        };
        self.summary_rows.push(row);
    }

    /// Advance to the next finding. Returns `true` when one remains.
    pub fn advance_finding(&mut self) -> bool {
        self.current += 1;
        self.current < self.findings.len()
    }

    /// Walkthrough finished with posted comments: show the deterministic
    /// summary and the Request changes / Comment / Skip choice.
    pub fn enter_summary(&mut self, body: String) {
        self.summary_body = body;
        self.summary_button = SummaryButton::Comment;
        self.decision_scroll = 0;
        self.step = ReviewStep::Summary;
    }

    /// Record how the summary submission went as its own table row.
    pub fn record_summary_outcome(&mut self, request_changes: bool, result: Result<(), String>) {
        let command = if request_changes {
            "review summary (request changes)"
        } else {
            "review summary (comment)"
        };
        let row = match result {
            Ok(()) => SummaryRow::with_status(command, "Submitted", colors::INFO, None),
            Err(err) => SummaryRow::with_status(command, "Failed", colors::ERROR, Some(err)),
        };
        self.summary_rows.push(row);
    }

    pub fn enter_done(&mut self) {
        if !self.telemetry_reported && !self.scan_telemetry.is_empty() {
            let label = review_telemetry_label(&self.scan_telemetry);
            self.summary_rows.push(SummaryRow::with_status(
                "AI scan usage",
                label,
                colors::MUTED,
                None,
            ));
            persist_scan_telemetry(&self.scan_telemetry);
            self.telemetry_reported = true;
        }
        self.step = ReviewStep::Done;
    }

    // ── input ───────────────────────────────────────────────────────────

    /// Scroll the comment/summary panel toward the top (smaller offset).
    fn scroll_panel_up(&mut self, lines: u16) {
        self.decision_scroll = self.decision_scroll.saturating_sub(lines);
    }

    /// Scroll toward the bottom, clamped to the last render's max offset so
    /// the offset can't run past the end and make scrolling back feel dead.
    fn scroll_panel_down(&mut self, lines: u16) {
        self.decision_scroll = self
            .decision_scroll
            .saturating_add(lines)
            .min(self.decision_max_scroll.get());
    }

    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        match self.step {
            ReviewStep::Decision | ReviewStep::Summary => {
                self.scroll_panel_up(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        match self.step {
            ReviewStep::Decision | ReviewStep::Summary => {
                self.scroll_panel_down(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ReviewAction {
        if self.error.is_some() {
            return ReviewAction::Cancelled;
        }
        match self.step {
            ReviewStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return ReviewAction::Cancelled;
                };
                match dialog.handle_key(key) {
                    ConfirmationOutcome::Confirmed => ReviewAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        ReviewAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => ReviewAction::Continue,
                }
            }
            ReviewStep::Working => match key.code {
                KeyCode::Esc => ReviewAction::Cancelled,
                _ => ReviewAction::Continue,
            },
            ReviewStep::Decision => self.handle_decision_key(key),
            ReviewStep::EditFinding => self.handle_edit_key(key),
            ReviewStep::OtherInput => self.handle_other_key(key),
            ReviewStep::Summary => self.handle_summary_key(key),
            ReviewStep::Done => ReviewAction::Done,
        }
    }

    fn handle_decision_key(&mut self, key: KeyEvent) -> ReviewAction {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.decision_button = prev_decision_button(self.decision_button);
                ReviewAction::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.decision_button = next_decision_button(self.decision_button);
                ReviewAction::Continue
            }
            KeyCode::Up => {
                self.scroll_panel_up(1);
                ReviewAction::Continue
            }
            KeyCode::Down => {
                self.scroll_panel_down(1);
                ReviewAction::Continue
            }
            KeyCode::Enter => match self.decision_button {
                DecisionButton::Post => ReviewAction::Post,
                DecisionButton::Edit => {
                    self.show_edit();
                    ReviewAction::Continue
                }
                DecisionButton::Other => ReviewAction::Other,
                DecisionButton::Skip => ReviewAction::Skip,
            },
            // Esc skips this finding (keeps the loop going) rather than
            // aborting the whole run, which would drop the posted comments'
            // summary on the floor.
            KeyCode::Esc => ReviewAction::Skip,
            _ => ReviewAction::Continue,
        }
    }

    /// Bugkill-select-style navigation: ↑/↓ move across the field rows,
    /// ←/→ (or Enter) change the focused field, `S` saves, Esc cancels.
    /// While a text editor is open it owns the keyboard.
    fn handle_edit_key(&mut self, key: KeyEvent) -> ReviewAction {
        let Some(edit) = self.edit.as_mut() else {
            self.step = ReviewStep::Decision;
            return ReviewAction::Continue;
        };
        if let Some(input) = edit.input.as_mut() {
            match input.handle_key(key) {
                InputOutcome::Submitted(text) => {
                    let text = text.trim().to_string();
                    match edit.row {
                        EditRow::Title => edit.draft.title = text,
                        EditRow::Explanation => edit.draft.explanation = text,
                        _ => {}
                    }
                    edit.input = None;
                }
                InputOutcome::Cancelled => edit.input = None,
                InputOutcome::Pending => {}
            }
            return ReviewAction::Continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                let at = EDIT_ROWS.iter().position(|r| *r == edit.row).unwrap_or(0);
                edit.row = EDIT_ROWS[(at + EDIT_ROWS.len() - 1) % EDIT_ROWS.len()];
            }
            KeyCode::Down | KeyCode::Tab => {
                let at = EDIT_ROWS.iter().position(|r| *r == edit.row).unwrap_or(0);
                edit.row = EDIT_ROWS[(at + 1) % EDIT_ROWS.len()];
            }
            KeyCode::Left => match edit.row {
                EditRow::Severity => edit.cycle_severity(false),
                EditRow::Suggestion => edit.toggle_suggestion(),
                _ => {}
            },
            KeyCode::Right => match edit.row {
                EditRow::Severity => edit.cycle_severity(true),
                EditRow::Suggestion => edit.toggle_suggestion(),
                _ => {}
            },
            KeyCode::Enter => Self::activate_edit_row(edit),
            KeyCode::Char('s') | KeyCode::Char('S') => self.save_edit(),
            KeyCode::Esc => self.enter_decision(),
            _ => {}
        }
        ReviewAction::Continue
    }

    /// Enter on a field row: cycle/toggle in place, or open the matching
    /// text editor prefilled with the draft value.
    fn activate_edit_row(edit: &mut EditState) {
        match edit.row {
            EditRow::Severity => edit.cycle_severity(true),
            EditRow::Suggestion => edit.toggle_suggestion(),
            EditRow::Title => {
                edit.input = Some(
                    InputPrompt::new("Edit the title:")
                        .with_default(edit.draft.title.clone())
                        .with_validator(|value| {
                            value
                                .trim()
                                .is_empty()
                                .then(|| "The title cannot be empty.".to_string())
                        }),
                );
            }
            EditRow::Explanation => {
                edit.input = Some(
                    InputPrompt::new("Edit the explanation (Ctrl+J for a new line):")
                        .multiline()
                        .with_default(edit.draft.explanation.clone())
                        .with_validator(|value| {
                            value
                                .trim()
                                .is_empty()
                                .then(|| "The explanation cannot be empty.".to_string())
                        }),
                );
            }
        }
    }

    fn handle_other_key(&mut self, key: KeyEvent) -> ReviewAction {
        let Some(input) = self.other_input.as_mut() else {
            self.step = ReviewStep::Decision;
            return ReviewAction::Continue;
        };
        match input.handle_key(key) {
            InputOutcome::Submitted(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    return ReviewAction::Continue;
                }
                self.other_input = None;
                ReviewAction::Revise(trimmed)
            }
            // Cancel returns to the Decision view with the same finding.
            InputOutcome::Cancelled => {
                self.other_input = None;
                self.step = ReviewStep::Decision;
                ReviewAction::Continue
            }
            InputOutcome::Pending => ReviewAction::Continue,
        }
    }

    fn handle_summary_key(&mut self, key: KeyEvent) -> ReviewAction {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.summary_button = prev_summary_button(self.summary_button);
                ReviewAction::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.summary_button = next_summary_button(self.summary_button);
                ReviewAction::Continue
            }
            KeyCode::Up => {
                self.scroll_panel_up(1);
                ReviewAction::Continue
            }
            KeyCode::Down => {
                self.scroll_panel_down(1);
                ReviewAction::Continue
            }
            KeyCode::Enter => match self.summary_button {
                SummaryButton::RequestChanges => ReviewAction::SubmitSummary {
                    request_changes: true,
                },
                SummaryButton::Comment => ReviewAction::SubmitSummary {
                    request_changes: false,
                },
                SummaryButton::Skip => ReviewAction::SkipSummary,
            },
            KeyCode::Esc => ReviewAction::SkipSummary,
            _ => ReviewAction::Continue,
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> ReviewAction {
        if self.error.is_some() {
            return ReviewAction::Continue;
        }
        match self.step {
            ReviewStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return ReviewAction::Cancelled;
                };
                match dialog.handle_mouse_click(position) {
                    ConfirmationOutcome::Confirmed => ReviewAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        ReviewAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => ReviewAction::Continue,
                }
            }
            ReviewStep::Decision => {
                let [post, edit, other, skip] = self.decision_button_rects.get();
                if contains_position(post, position) {
                    self.decision_button = DecisionButton::Post;
                    return ReviewAction::Post;
                }
                if contains_position(edit, position) {
                    self.decision_button = DecisionButton::Edit;
                    self.show_edit();
                    return ReviewAction::Continue;
                }
                if contains_position(other, position) {
                    self.decision_button = DecisionButton::Other;
                    return ReviewAction::Other;
                }
                if contains_position(skip, position) {
                    self.decision_button = DecisionButton::Skip;
                    return ReviewAction::Skip;
                }
                ReviewAction::Continue
            }
            // A click on a field row focuses and activates it (cycle /
            // toggle / open the text editor) — same as ↑↓ + Enter.
            ReviewStep::EditFinding => {
                if let Some(edit) = self.edit.as_mut() {
                    if edit.input.is_none() {
                        let rects = edit.row_rects.get();
                        for (i, rect) in rects.iter().enumerate() {
                            if contains_position(*rect, position) {
                                edit.row = EDIT_ROWS[i];
                                Self::activate_edit_row(edit);
                                break;
                            }
                        }
                    }
                }
                ReviewAction::Continue
            }
            ReviewStep::Summary => {
                let [request, comment, skip] = self.summary_button_rects.get();
                if contains_position(request, position) {
                    self.summary_button = SummaryButton::RequestChanges;
                    return ReviewAction::SubmitSummary {
                        request_changes: true,
                    };
                }
                if contains_position(comment, position) {
                    self.summary_button = SummaryButton::Comment;
                    return ReviewAction::SubmitSummary {
                        request_changes: false,
                    };
                }
                if contains_position(skip, position) {
                    self.summary_button = SummaryButton::Skip;
                    return ReviewAction::SkipSummary;
                }
                ReviewAction::Continue
            }
            ReviewStep::Working | ReviewStep::OtherInput | ReviewStep::Done => {
                ReviewAction::Continue
            }
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            // Scanning shows the progress dashboard (spinner + stepper + bar +
            // the "Under review" panel); every other Working phase is a quiet
            // one-line spinner.
            ReviewStep::Working => {
                if self.scanning {
                    12
                } else {
                    3
                }
            }
            ReviewStep::Done => {
                let table_rows = (self.summary_rows.len() as u16).min(14);
                let table_height = if self.summary_rows.is_empty() {
                    5
                } else {
                    table_rows + 3
                };
                (3 + table_height + 1).max(10)
            }
            // Full-panel steps are sized by the App; return a sane default.
            _ => 22,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(err) = self.error.as_deref() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Length(1)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Cannot review pull request: {err}"),
                    Style::default().fg(colors::ERROR),
                )))
                .wrap(Wrap { trim: true }),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to return to dashboard...").style(
                    Style::default()
                        .fg(colors::MUTED)
                        .add_modifier(Modifier::DIM),
                ),
                chunks[1],
            );
            return;
        }
        match self.step {
            ReviewStep::Confirm => self.render_confirm(frame, area),
            ReviewStep::Working => self.render_working(frame, area),
            ReviewStep::Decision => self.render_decision(frame, area),
            ReviewStep::EditFinding => self.render_edit(frame, area),
            ReviewStep::OtherInput => self.render_other(frame, area),
            ReviewStep::Summary => self.render_summary(frame, area),
            ReviewStep::Done => self.render_done(frame, area),
        }
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        PrConfirmView::new(format!("Review Pull Request #{}?", self.request.number))
            .title_color(colors::NAVY)
            .block(build_detail_lines(&self.request))
            .steps(&REVIEW_STEPS)
            .ai_roles(vec![
                AiRoleRow::new(
                    "strong",
                    colors::NAVY,
                    self.ai.strong.model.clone(),
                    self.ai.strong.thinking.clone(),
                ),
                AiRoleRow::new(
                    "balanced",
                    colors::NAVY,
                    self.ai.balanced.model.clone(),
                    self.ai.balanced.thinking.clone(),
                ),
                AiRoleRow::new(
                    "utility",
                    colors::NAVY,
                    self.ai.utility.model.clone(),
                    self.ai.utility.thinking.clone(),
                ),
            ])
            .modal(self.confirm.as_ref())
            .render(frame, area);
    }

    /// Working spinner. While scanning, a live progress dashboard sits below
    /// it: the pipeline stepper (Scan → Audit → Verify), a per-stage bar, the
    /// files/passes currently under review, and a running findings tally —
    /// so the user can see exactly what the AI is doing in the background.
    fn render_working(&self, frame: &mut Frame, area: Rect) {
        if !self.scanning || area.height < 7 {
            StatusIndicator::new(Status::Loading, self.phase_message.clone())
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // spinner headline
                Constraint::Length(1), // pipeline stepper
                Constraint::Length(1), // progress bar
                Constraint::Length(1), // blank
                Constraint::Min(3),    // "Under review" + findings panel
            ])
            .split(area);
        StatusIndicator::new(Status::Loading, self.phase_message.clone())
            .with_tick(self.tick)
            .render(frame, chunks[0]);
        self.render_pipeline_stepper(frame, chunks[1]);
        self.render_progress_bar(frame, chunks[2]);
        self.render_scan_panel(frame, chunks[4]);
    }

    /// The `Scan → Audit → Verify` stepper. Each cell reuses the post-create
    /// command-list icon vocabulary: `✓` done, spinner active, `○` pending,
    /// `–` skipped (an audit that this diff does not qualify for).
    fn render_pipeline_stepper(&self, frame: &mut Frame, area: Rect) {
        let mut spans = Vec::new();
        for (i, (name, state)) in self.pipeline_stages().into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  →  ".to_string(), muted_dim()));
            }
            let (icon, icon_style) = match state {
                StageState::Done => ("✓".to_string(), Style::default().fg(colors::SUCCESS)),
                StageState::Active => (
                    spinner_frame(self.tick).to_string(),
                    Style::default().fg(colors::NAVY),
                ),
                StageState::Pending => ("○".to_string(), muted_dim()),
                StageState::Skipped => ("–".to_string(), muted_dim()),
            };
            let label_style = match state {
                StageState::Active => Style::default()
                    .fg(colors::EMPHASIS)
                    .add_modifier(Modifier::BOLD),
                StageState::Done => Style::default().fg(colors::EMPHASIS),
                _ => muted_dim(),
            };
            spans.push(Span::styled(icon, icon_style));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(name.to_string(), label_style));
            if matches!(state, StageState::Skipped) {
                spans.push(Span::styled(" (n/a)".to_string(), muted_dim()));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The active stage's progress bar. Countable stages (scan / verify) show
    /// a filled block bar with `done/total · pct%`; the single-call audit pass
    /// shows an indeterminate sweep so the user still sees it is alive.
    fn render_progress_bar(&self, frame: &mut Frame, area: Rect) {
        let line = match self.stage_progress() {
            Some((done, total)) => progress_bar_line(done, total, area.width),
            None => indeterminate_bar_line(self.tick, area.width),
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_scan_panel(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::NAVY))
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Under review",
                    Style::default()
                        .fg(colors::NAVY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        // Activity fills the top; the findings tally is pinned to the bottom
        // row so it stays put as scans come and go.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        let activity = self.activity_lines(rows[0].height as usize, inner.width as usize);
        frame.render_widget(Paragraph::new(activity), rows[0]);
        frame.render_widget(Paragraph::new(self.findings_tally_line()), rows[1]);
    }

    /// The pipeline stages paired with each one's current state. `Audit` is a
    /// real stage only when the diff qualifies for the cross-group omission
    /// pass; otherwise it renders as skipped.
    fn pipeline_stages(&self) -> [(&'static str, StageState); 3] {
        let scan = match self.stage {
            PipelineStage::Scan => StageState::Active,
            _ => StageState::Done,
        };
        let audit = if !self.plan_audit {
            StageState::Skipped
        } else {
            match self.stage {
                PipelineStage::Scan => StageState::Pending,
                PipelineStage::Audit => StageState::Active,
                PipelineStage::Verify => StageState::Done,
            }
        };
        let verify = match self.stage {
            PipelineStage::Verify => StageState::Active,
            _ => StageState::Pending,
        };
        [("Scan", scan), ("Audit", audit), ("Verify", verify)]
    }

    /// `(done, total)` for the active stage's bar, or `None` for the single
    /// indeterminate audit call.
    fn stage_progress(&self) -> Option<(usize, usize)> {
        match self.stage {
            PipelineStage::Scan => Some((self.scans_done, self.scan_units_total())),
            PipelineStage::Audit => None,
            PipelineStage::Verify => Some((
                self.verification_total
                    .saturating_sub(self.verification_outstanding.len()),
                self.verification_total,
            )),
        }
    }

    /// Findings tallied per severity (Critical, High, Medium, Low).
    fn severity_counts(&self) -> [usize; 4] {
        let mut counts = [0usize; 4];
        for finding in &self.findings {
            counts[finding.severity.rank() as usize] += 1;
        }
        counts
    }

    /// The rows inside the "Under review" panel, capped at `max_rows`. During
    /// the scan pass these name the files/passes in flight; the audit and
    /// verify passes carry no per-file work, so they describe what is running.
    fn activity_lines(&self, max_rows: usize, width: usize) -> Vec<Line<'static>> {
        if max_rows == 0 {
            return Vec::new();
        }
        match self.stage {
            PipelineStage::Scan => {
                if self.in_flight.is_empty() {
                    return vec![Line::from(Span::styled(
                        "  wrapping up the last scans…".to_string(),
                        muted_dim(),
                    ))];
                }
                let mut lines = Vec::new();
                let overflow = self.in_flight.len() > max_rows;
                let visible = if overflow {
                    max_rows - 1
                } else {
                    self.in_flight.len()
                };
                for label in self.in_flight.iter().take(visible) {
                    lines.push(self.activity_row(label, width));
                }
                if overflow {
                    let more = self.in_flight.len() - visible;
                    lines.push(Line::from(Span::styled(
                        format!("  … +{more} more in parallel"),
                        muted_dim(),
                    )));
                }
                lines
            }
            PipelineStage::Audit => vec![self.activity_note(
                "re-reading every changed file together to catch cross-file gaps",
                width,
            )],
            PipelineStage::Verify => {
                let n = self.verification_outstanding.len();
                vec![self.activity_note(
                    &format!(
                        "independently re-checking {n} high-risk finding{} against the code",
                        if n == 1 { "" } else { "s" },
                    ),
                    width,
                )]
            }
        }
    }

    /// One in-flight scan row: spinner + a colored kind badge (app / tests /
    /// coverage / review) + the file paths or coverage group under review.
    fn activity_row(&self, label: &str, width: usize) -> Line<'static> {
        const BADGE_W: usize = 8;
        const PREFIX_W: usize = 2 + BADGE_W + 1; // "⠋ " + badge + " "
        let (badge, detail) = split_activity_label(label);
        let detail = truncate_to(&sanitize_row(&detail), width.saturating_sub(PREFIX_W));
        Line::from(vec![
            Span::styled(
                spinner_frame(self.tick).to_string(),
                Style::default().fg(colors::PRIMARY),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{badge:<BADGE_W$}"),
                Style::default()
                    .fg(activity_badge_color(badge))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(detail, Style::default().fg(colors::EMPHASIS)),
        ])
    }

    /// A single descriptive row for the passes that do no per-file work.
    fn activity_note(&self, text: &str, width: usize) -> Line<'static> {
        let text = truncate_to(&sanitize_row(text), width.saturating_sub(2));
        Line::from(vec![
            Span::styled(
                spinner_frame(self.tick).to_string(),
                Style::default().fg(colors::PRIMARY),
            ),
            Span::raw(" "),
            Span::styled(text, Style::default().fg(colors::EMPHASIS)),
        ])
    }

    /// The running findings tally, broken down by severity with colored counts
    /// instead of one opaque total.
    fn findings_tally_line(&self) -> Line<'static> {
        let counts = self.severity_counts();
        let total: usize = counts.iter().sum();
        let mut spans = vec![Span::styled(
            "Findings  ".to_string(),
            Style::default().fg(colors::EMPHASIS),
        )];
        if total == 0 {
            spans.push(Span::styled("none surfaced yet".to_string(), muted_dim()));
            return Line::from(spans);
        }
        let by_severity = [
            (ReviewSeverity::Critical, counts[0]),
            (ReviewSeverity::High, counts[1]),
            (ReviewSeverity::Medium, counts[2]),
            (ReviewSeverity::Low, counts[3]),
        ];
        for (severity, count) in by_severity {
            let style = if count == 0 {
                muted_dim()
            } else {
                Style::default()
                    .fg(severity_color(severity))
                    .add_modifier(Modifier::BOLD)
            };
            spans.push(Span::styled(
                format!("{} {count}  ", severity.emoji()),
                style,
            ));
        }
        spans.push(Span::styled(format!("· {total} total"), muted_dim()));
        Line::from(spans)
    }

    fn render_decision(&self, frame: &mut Frame, area: Rect) {
        let total = self.findings.len();
        let n = self.current + 1;
        let finding = self.findings.get(self.current);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(3),    // comment preview panel
                Constraint::Length(3), // buttons
                Constraint::Length(1), // shortcuts
            ])
            .split(area);

        let mut header = vec![Span::styled(
            format!("Finding #{n} of {total}"),
            Style::default()
                .fg(colors::NAVY)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some(finding) = finding {
            header.push(Span::styled("  ·  ".to_string(), muted_dim()));
            header.push(Span::styled(
                format!("[{}] [{}]", finding.category, finding.severity.label()),
                Style::default()
                    .fg(severity_color(finding.severity))
                    .add_modifier(Modifier::BOLD),
            ));
            header.push(Span::styled("  ·  ".to_string(), muted_dim()));
            header.push(Span::styled(
                sanitize_row(&finding.descriptor()),
                Style::default().fg(colors::EMPHASIS),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(header)), chunks[0]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::NAVY))
            .title(Line::from(Span::styled(
                " Proposed comment ".to_string(),
                Style::default()
                    .fg(colors::NAVY)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(chunks[1]);
        let lines = match finding {
            Some(finding) => build_comment_preview_lines(finding, inner.width as usize),
            None => vec![Line::from(Span::styled(
                "(no finding)".to_string(),
                muted_dim(),
            ))],
        };
        frame.render_widget(block, chunks[1]);
        let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
        self.decision_max_scroll.set(max_scroll);
        let scroll = self.decision_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );

        self.render_decision_buttons(frame, chunks[2]);
        render_shortcut_line(frame, chunks[3], "Skip");
    }

    fn render_decision_buttons(&self, frame: &mut Frame, area: Rect) {
        let rects = render_button_row(
            frame,
            area,
            [
                (
                    "  Post  ",
                    colors::SUCCESS,
                    matches!(self.decision_button, DecisionButton::Post),
                ),
                (
                    "  Edit  ",
                    colors::INFO,
                    matches!(self.decision_button, DecisionButton::Edit),
                ),
                (
                    "  Other  ",
                    colors::BRAND,
                    matches!(self.decision_button, DecisionButton::Other),
                ),
                (
                    "  Skip  ",
                    colors::WARNING,
                    matches!(self.decision_button, DecisionButton::Skip),
                ),
            ],
        );
        self.decision_button_rects.set(rects);
    }

    /// The deterministic edit form: Bugkill-style field rows (selected row
    /// highlighted, value column colored by meaning) over a live preview of
    /// the comment as it would be posted, closed by a colored-keys footer.
    fn render_edit(&self, frame: &mut Frame, area: Rect) {
        let Some(edit) = self.edit.as_ref() else {
            return;
        };
        let n = self.current + 1;
        let total = self.findings.len();
        let fields_height = if edit.input.is_some() {
            11
        } else {
            EDIT_ROWS.len() as u16
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),             // title
                Constraint::Length(1),             // subtitle
                Constraint::Length(1),             // blank
                Constraint::Length(fields_height), // field rows / text editor
                Constraint::Length(1),             // blank
                Constraint::Min(3),                // live preview
                Constraint::Length(1),             // shortcuts
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("Edit finding #{n} of {total}"),
                    Style::default()
                        .fg(colors::NAVY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled(
                    sanitize_row(&edit.draft.descriptor()),
                    Style::default().fg(colors::EMPHASIS),
                ),
            ])),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Deterministic edit — no AI call, no tokens. The AI-written original is \
                 restored on Cancel.",
                Style::default().fg(colors::GRAY_DARK),
            ))),
            chunks[1],
        );

        if let Some(input) = edit.input.as_ref() {
            input.render(frame, chunks[3], self.tick);
            edit.row_rects.set([Rect::default(); 4]);
        } else {
            self.render_edit_rows(frame, chunks[3], edit);
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::NAVY))
            .title(Line::from(Span::styled(
                " Preview ".to_string(),
                Style::default()
                    .fg(colors::NAVY)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(chunks[5]);
        frame.render_widget(block, chunks[5]);
        frame.render_widget(
            Paragraph::new(build_comment_preview_lines(
                &edit.draft,
                inner.width as usize,
            ))
            .wrap(Wrap { trim: false }),
            inner,
        );

        render_edit_shortcut_line(frame, chunks[6], edit.input.is_some());
    }

    fn render_edit_rows(&self, frame: &mut Frame, area: Rect, edit: &EditState) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1); 4])
            .split(area);
        let mut rects = [Rect::default(); 4];
        for (i, row) in EDIT_ROWS.iter().enumerate() {
            rects[i] = rows[i];
            let selected = edit.row == *row;
            let (label, value) = edit_row_content(edit, *row);
            let marker = if selected { "▸ " } else { "  " };
            let mut line = Line::from(vec![
                Span::styled(marker.to_string(), Style::default().fg(colors::NAVY)),
                Span::styled(
                    format!("{label:<13}"),
                    Style::default()
                        .fg(if selected {
                            colors::WHITE
                        } else {
                            colors::GRAY_LIGHT
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                value,
            ]);
            if selected {
                line = line.style(Style::default().bg(colors::BG_SELECTED));
            }
            frame.render_widget(Paragraph::new(line), rows[i]);
        }
        edit.row_rects.set(rects);
    }

    fn render_other(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // heading
                Constraint::Length(1), // blank
                Constraint::Min(3),    // input
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Revise the comment".to_string(),
                Style::default()
                    .fg(colors::NAVY)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        if let Some(input) = self.other_input.as_ref() {
            input.render(frame, chunks[2], self.tick);
        }
    }

    fn render_summary(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(3),    // summary panel
                Constraint::Length(3), // buttons
                Constraint::Length(1), // shortcuts
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("Review summary for Pull Request #{}", self.request.number),
                    Style::default()
                        .fg(colors::NAVY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled(
                    format!("{} comment(s) posted", self.posted.len()),
                    Style::default().fg(colors::EMPHASIS),
                ),
            ])),
            chunks[0],
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::NAVY))
            .title(Line::from(Span::styled(
                " Will be submitted ".to_string(),
                Style::default()
                    .fg(colors::NAVY)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(chunks[1]);
        let lines: Vec<Line<'static>> = self
            .summary_body
            .lines()
            .map(|raw| {
                Line::from(Span::styled(
                    sanitize_row(raw),
                    Style::default().fg(colors::WHITE),
                ))
            })
            .collect();
        frame.render_widget(block, chunks[1]);
        let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
        self.decision_max_scroll.set(max_scroll);
        let scroll = self.decision_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );

        self.render_summary_buttons(frame, chunks[2]);
        render_shortcut_line(frame, chunks[3], "Skip summary");
    }

    fn render_summary_buttons(&self, frame: &mut Frame, area: Rect) {
        let rects = render_button_row(
            frame,
            area,
            [
                (
                    "  Request changes  ",
                    colors::ERROR,
                    matches!(self.summary_button, SummaryButton::RequestChanges),
                ),
                (
                    "  Comment  ",
                    colors::INFO,
                    matches!(self.summary_button, SummaryButton::Comment),
                ),
                (
                    "  Skip  ",
                    colors::WARNING,
                    matches!(self.summary_button, SummaryButton::Skip),
                ),
            ],
        );
        self.summary_button_rects.set(rects);
    }

    fn render_done(&self, frame: &mut Frame, area: Rect) {
        let failures = self.summary_rows.iter().filter(|r| !r.success).count();
        let (status, headline) = if failures > 0 {
            (
                Status::Error,
                format!("Finished with {failures} failure(s) — see below."),
            )
        } else if self.findings.is_empty() {
            (
                Status::Success,
                "No issues found — the code looks good!".to_string(),
            )
        } else if self.posted.is_empty() {
            (
                Status::Success,
                "Review complete — nothing was posted.".to_string(),
            )
        } else {
            (
                Status::Success,
                format!("Posted {} review comment(s)!", self.posted.len()),
            )
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);
        StatusIndicator::new(status, headline)
            .without_spinner()
            .render(frame, chunks[0]);
        if self.summary_rows.is_empty() {
            frame.render_widget(
                Paragraph::new("No comments were posted on this pull request.").style(muted_dim()),
                chunks[1],
            );
        } else {
            render_summary_table(&self.summary_rows, frame, chunks[1]);
        }
        frame.render_widget(
            Paragraph::new("Press any key to continue").style(muted_dim()),
            chunks[2],
        );
    }
}

fn persist_scan_telemetry(scans: &[ReviewScanTelemetry]) {
    #[cfg(not(test))]
    {
        let scans = scans.to_vec();
        tokio::task::spawn_blocking(move || {
            crate::services::review_telemetry::persist_review_telemetry(&scans);
        });
    }
    #[cfg(test)]
    let _ = scans;
}

/// Render a centered row of bordered buttons, each sized to exactly its own
/// (already symmetrically padded) label so the text stays perfectly centered.
///
/// `button_paragraph` centers with `Alignment::Center`, which biases the odd
/// remainder to one side. A fixed button width therefore de-centers any label
/// whose width has the opposite parity — e.g. `"  Edit  "` (8) or
/// `"  Comment  "` (11) in a wider field leaves an odd gap and drifts left.
/// Making each inner width equal its label width keeps the remainder zero, so
/// every label centers regardless of length. Buttons are separated by
/// two-column gaps and centered as a group; the returned rects feed mouse
/// hit-testing.
fn render_button_row<const N: usize>(
    frame: &mut Frame,
    area: Rect,
    buttons: [(&str, Color, bool); N],
) -> [Rect; N] {
    let mut constraints: Vec<Constraint> = Vec::with_capacity(N * 2 + 1);
    constraints.push(Constraint::Min(0));
    for (i, (label, _, _)) in buttons.iter().enumerate() {
        if i > 0 {
            constraints.push(Constraint::Length(2));
        }
        // +2 for the left/right borders around the label.
        constraints.push(Constraint::Length(label.chars().count() as u16 + 2));
    }
    constraints.push(Constraint::Min(0));
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let mut rects = [Rect::default(); N];
    for (i, (label, color, focused)) in buttons.iter().enumerate() {
        // Layout: [Min, b0, gap, b1, gap, …] — button i sits at 1 + i*2.
        let rect = chunks[1 + i * 2];
        frame.render_widget(button_paragraph(label, *color, *focused), rect);
        rects[i] = rect;
    }
    rects
}

fn next_decision_button(b: DecisionButton) -> DecisionButton {
    match b {
        DecisionButton::Post => DecisionButton::Edit,
        DecisionButton::Edit => DecisionButton::Other,
        DecisionButton::Other => DecisionButton::Skip,
        DecisionButton::Skip => DecisionButton::Post,
    }
}

fn prev_decision_button(b: DecisionButton) -> DecisionButton {
    match b {
        DecisionButton::Post => DecisionButton::Skip,
        DecisionButton::Edit => DecisionButton::Post,
        DecisionButton::Other => DecisionButton::Edit,
        DecisionButton::Skip => DecisionButton::Other,
    }
}

fn next_summary_button(b: SummaryButton) -> SummaryButton {
    match b {
        SummaryButton::RequestChanges => SummaryButton::Comment,
        SummaryButton::Comment => SummaryButton::Skip,
        SummaryButton::Skip => SummaryButton::RequestChanges,
    }
}

fn prev_summary_button(b: SummaryButton) -> SummaryButton {
    match b {
        SummaryButton::RequestChanges => SummaryButton::Skip,
        SummaryButton::Comment => SummaryButton::RequestChanges,
        SummaryButton::Skip => SummaryButton::Comment,
    }
}

fn muted_dim() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM)
}

/// One edit-form row: its label and the styled value span.
fn edit_row_content(edit: &EditState, row: EditRow) -> (&'static str, Span<'static>) {
    match row {
        EditRow::Severity => (
            "Severity",
            Span::styled(
                format!("‹ {} ›", edit.draft.severity.label()),
                Style::default()
                    .fg(severity_color(edit.draft.severity))
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        EditRow::Title => (
            "Title",
            Span::styled(
                sanitize_row(&edit.draft.title),
                Style::default().fg(colors::WHITE),
            ),
        ),
        EditRow::Explanation => {
            let mut lines = edit.draft.explanation.lines();
            let first = lines.next().unwrap_or("").to_string();
            let more = lines.next().is_some();
            let shown = if more { format!("{first} …") } else { first };
            (
                "Explanation",
                Span::styled(
                    sanitize_row(&shown),
                    Style::default().fg(colors::GRAY_LIGHT),
                ),
            )
        }
        EditRow::Suggestion => match (&edit.draft.suggestion, &edit.removed_suggestion) {
            (Some(_), _) => (
                "Suggestion",
                Span::styled(
                    "Kept — ← → removes the one-click suggestion block".to_string(),
                    Style::default().fg(colors::SUCCESS),
                ),
            ),
            (None, Some(_)) => (
                "Suggestion",
                Span::styled(
                    "Removed — ← → restores it".to_string(),
                    Style::default().fg(colors::WARNING),
                ),
            ),
            (None, None) => (
                "Suggestion",
                Span::styled("(this finding has none)".to_string(), muted_dim()),
            ),
        },
    }
}

/// Footer for the edit form. While a text editor is open the editor owns
/// the keys, so only its own submit/cancel hints apply.
fn render_edit_shortcut_line(frame: &mut Frame, area: Rect, input_open: bool) {
    let separator = Span::styled("  ·  ".to_string(), muted_dim());
    let spans = if input_open {
        vec![
            Span::styled("↵ ".to_string(), Style::default().fg(colors::SUCCESS)),
            Span::styled("Apply".to_string(), muted_dim()),
            separator,
            Span::styled("Esc ".to_string(), Style::default().fg(colors::WARNING)),
            Span::styled("Back to the form".to_string(), muted_dim()),
        ]
    } else {
        vec![
            Span::styled("↑ ↓ ".to_string(), Style::default().fg(colors::INFO)),
            Span::styled("Field".to_string(), muted_dim()),
            separator.clone(),
            Span::styled("← → ".to_string(), Style::default().fg(colors::INFO)),
            Span::styled("Change".to_string(), muted_dim()),
            separator.clone(),
            Span::styled("↵ ".to_string(), Style::default().fg(colors::INFO)),
            Span::styled("Edit / toggle".to_string(), muted_dim()),
            separator.clone(),
            Span::styled("S ".to_string(), Style::default().fg(colors::SUCCESS)),
            Span::styled("Save".to_string(), muted_dim()),
            separator,
            Span::styled("Esc ".to_string(), Style::default().fg(colors::WARNING)),
            Span::styled("Cancel".to_string(), muted_dim()),
        ]
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The severity badge color: Critical reads as an error, Low as info.
fn severity_color(severity: ReviewSeverity) -> Color {
    match severity {
        ReviewSeverity::Critical => colors::ERROR,
        ReviewSeverity::High => colors::ACCENT,
        ReviewSeverity::Medium => colors::WARNING,
        ReviewSeverity::Low => colors::INFO,
    }
}

fn build_confirm(request: &ReviewPullRequestRequest) -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title(format!("Review Pull Request #{}?", request.number))
        .with_subtitle(format!(
            "Scan `{}`'s changed files with AI and post review comments?",
            request.branch
        ))
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color_value(colors::NAVY)
        .with_selected(ConfirmationChoice::Cancel)
}

fn build_detail_lines(request: &ReviewPullRequestRequest) -> Vec<Line<'static>> {
    vec![
        labeled_line(
            "PR",
            Span::styled(
                format!("#{} ", request.number),
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
            (!request.title.is_empty())
                .then(|| Span::styled(request.title.clone(), Style::default().fg(colors::WHITE))),
        ),
        labeled_line(
            "Branch",
            Span::styled(
                request.branch.clone(),
                Style::default()
                    .fg(colors::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            None,
        ),
        labeled_line(
            "Worktree",
            Span::styled(
                request.worktree_path.clone(),
                Style::default().fg(colors::EMPHASIS),
            ),
            None,
        ),
    ]
}

/// `Will run:` step text for the confirm panel; the shared [`PrConfirmView`]
/// owns the numbering + styling.
const REVIEW_STEPS: [&str; 7] = [
    "Sync the branch + fetch the PR diff and its existing comments",
    "Only binary or blank-only changes are skipped; risky text changes stay reviewable",
    "AI scans files in parallel; one whole-diff pass alone judges test coverage",
    "You choose Post / Edit / Other / Skip per finding (Edit is AI-free)",
    "Approved findings are posted as inline PR comments (with suggestions)",
    "A review summary is assembled from the posted comments (no AI)",
    "You choose Request changes / Comment / Skip for the summary",
];

/// Build the body of the `Proposed comment` panel: the exact comment header,
/// explanation, and — when present — the one-click suggestion rendered as
/// full-width replacement bars. `width` is the panel's inner content width.
fn build_comment_preview_lines(finding: &ReviewFinding, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            sanitize_row(&format!(
                "[{}] [{}]: ",
                finding.category,
                finding.severity.label()
            )),
            Style::default()
                .fg(severity_color(finding.severity))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            sanitize_row(&finding.title),
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if finding.line.is_none() {
        lines.push(Line::from(Span::styled(
            sanitize_row(&format!("📄 {} (general PR comment)", finding.file)),
            muted_dim(),
        )));
    }
    if !finding.explanation.trim().is_empty() {
        lines.push(Line::from(""));
        for raw in finding.explanation.lines() {
            lines.push(Line::from(Span::styled(
                sanitize_row(raw),
                Style::default().fg(colors::WHITE),
            )));
        }
    }
    if let Some(suggestion) = &finding.suggestion {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("▌ ".to_string(), Style::default().fg(colors::NAVY)),
            Span::styled(
                "Suggested replacement".to_string(),
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for raw in suggestion.lines() {
            push_suggestion_bar(&mut lines, raw, width);
        }
    }
    lines
}

/// Push one suggestion line as a full-width green bar (the same treatment the
/// Fix screen gives `+` diff lines), hard-wrapped at `width` columns with
/// trailing padding so the background reaches the panel edge.
fn push_suggestion_bar(lines: &mut Vec<Line<'static>>, raw: &str, width: usize) {
    let style = Style::default()
        .fg(colors::DIFF_ADD_FG)
        .bg(colors::DIFF_ADD_BG);
    let text = sanitize_row(raw);
    if width == 0 {
        lines.push(Line::from(Span::styled(text, style)));
        return;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0;
    loop {
        let end = (start + width).min(chars.len());
        let mut segment: String = chars[start..end].iter().collect();
        let pad = width - (end - start);
        segment.extend(std::iter::repeat(' ').take(pad));
        lines.push(Line::from(Span::styled(segment, style)));
        start = end;
        if start >= chars.len() {
            break;
        }
    }
}

/// The `← → Switch · ↑ ↓ Scroll · ↵ Choose · Esc <esc_label>` footer shared
/// by the Decision and Summary steps.
fn render_shortcut_line(frame: &mut Frame, area: Rect, esc_label: &str) {
    let separator = Span::styled("  ·  ".to_string(), muted_dim());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("← → ".to_string(), Style::default().fg(colors::INFO)),
            Span::styled("Switch".to_string(), muted_dim()),
            separator.clone(),
            Span::styled("↑ ↓ ".to_string(), Style::default().fg(colors::INFO)),
            Span::styled("Scroll".to_string(), muted_dim()),
            separator.clone(),
            Span::styled("↵ ".to_string(), Style::default().fg(colors::SUCCESS)),
            Span::styled("Choose".to_string(), muted_dim()),
            separator,
            Span::styled("Esc ".to_string(), Style::default().fg(colors::WARNING)),
            Span::styled(esc_label.to_string(), muted_dim()),
        ])),
        area,
    );
}

/// Sanitize one display row before it becomes a rendered cell: tabs expand to
/// four spaces and every other control character is dropped, so a stray `\r`
/// or escape byte in AI output can never shred the panel layout (see the
/// identical helper on the Fix screen for the war story).
fn sanitize_row(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Bar cells the progress line ever draws — kept short so a wide terminal does
/// not stretch it into a runway.
const PROGRESS_BAR_MAX: usize = 32;

/// A determinate progress bar: `████████░░░░  8/12 · 67%`.
fn progress_bar_line(done: usize, total: usize, width: u16) -> Line<'static> {
    if total == 0 {
        return Line::from("");
    }
    let done = done.min(total);
    let suffix = format!("  {done}/{total} · {}%", done * 100 / total);
    let bar_w = (width as usize)
        .saturating_sub(suffix.chars().count())
        .min(PROGRESS_BAR_MAX);
    let filled = (bar_w * done + total / 2) / total;
    let filled = filled.min(bar_w);
    Line::from(vec![
        Span::styled("█".repeat(filled), Style::default().fg(colors::NAVY)),
        Span::styled("░".repeat(bar_w - filled), muted_dim()),
        Span::styled(suffix, Style::default().fg(colors::EMPHASIS)),
    ])
}

/// An indeterminate bar: a filled window sweeps back and forth over a muted
/// track, so a single long-running call still reads as alive.
fn indeterminate_bar_line(tick: usize, width: u16) -> Line<'static> {
    let bar_w = (width as usize).min(PROGRESS_BAR_MAX);
    if bar_w == 0 {
        return Line::from("");
    }
    let window = 6.min(bar_w);
    let span = bar_w - window;
    let start = if span == 0 {
        0
    } else {
        let p = tick % (span * 2);
        if p <= span {
            p
        } else {
            span * 2 - p
        }
    };
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::styled("░".repeat(start), muted_dim()));
    }
    spans.push(Span::styled(
        "█".repeat(window),
        Style::default().fg(colors::NAVY),
    ));
    let tail = bar_w - start - window;
    if tail > 0 {
        spans.push(Span::styled("░".repeat(tail), muted_dim()));
    }
    Line::from(spans)
}

/// Split an in-flight scan label into a short kind badge and its detail. The
/// labels are produced by `scan_group_label` / `coverage_scan_label`, so the
/// prefixes are ours to rely on.
fn split_activity_label(label: &str) -> (&'static str, String) {
    if let Some(rest) = label.strip_prefix("app: ") {
        ("app", rest.to_string())
    } else if let Some(rest) = label.strip_prefix("tests: ") {
        ("tests", rest.to_string())
    } else if let Some(rest) = label.strip_prefix(&format!("{COVERAGE_SCAN_LABEL} ")) {
        ("coverage", rest.to_string())
    } else if label == MERGED_SCAN_LABEL {
        ("review", "whole diff + test coverage".to_string())
    } else {
        ("scan", label.to_string())
    }
}

fn activity_badge_color(badge: &str) -> Color {
    match badge {
        "app" => colors::CYAN,
        "tests" => colors::GREEN,
        "coverage" => colors::ACCENT,
        "review" => colors::NAVY,
        _ => colors::EMPHASIS,
    }
}

/// Truncate to `max` display columns (char count is a fair proxy for the ASCII
/// paths and ASCII labels this renders), adding an ellipsis when it clips.
fn truncate_to(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::AiModelConfig;
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::BTreeSet;

    fn request() -> ReviewPullRequestRequest {
        ReviewPullRequestRequest {
            number: 42,
            title: "Add retry logic".to_string(),
            url: "https://github.com/o/r/pull/42".to_string(),
            branch: "digit-3131-retry".to_string(),
            worktree_path: "/tmp/repo-retry".to_string(),
        }
    }

    fn test_ai() -> AiReviewConfig {
        let model = AiModelConfig {
            model: "opencode/review-scan".to_string(),
            thinking: "max".to_string(),
        };
        AiReviewConfig {
            strong: model.clone(),
            balanced: model.clone(),
            utility: model,
        }
    }

    fn file(path: &str) -> ReviewFile {
        ReviewFile {
            path: path.to_string(),
            annotated_diff:
                "@@ -1,2 +1,3 @@\n     1  fn main() {\n     2 +    let x = 1;\n     3  }"
                    .to_string(),
            full_content: None,
            commentable_lines: BTreeSet::from([1, 2, 3]),
            existing_comments: String::new(),
            existing_keys: Vec::new(),
        }
    }

    fn finding(path: &str, line: Option<u64>, severity: ReviewSeverity) -> ReviewFinding {
        ReviewFinding {
            category: "Security".to_string(),
            severity,
            file: path.to_string(),
            start_line: None,
            line,
            title: "Hardcoded API key".to_string(),
            explanation: "Secrets in source leak through the VCS history.".to_string(),
            suggestion: Some("let key = env::var(\"API_KEY\")?;".to_string()),
        }
    }

    fn finding_with(
        path: &str,
        line: Option<u64>,
        severity: ReviewSeverity,
        suggestion: Option<&str>,
    ) -> ReviewFinding {
        ReviewFinding {
            suggestion: suggestion.map(str::to_string),
            ..finding(path, line, severity)
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render_dump(screen: &mut ReviewPullRequestScreen, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| screen.render(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn starts_on_confirm_with_no_default() {
        let screen = ReviewPullRequestScreen::new(request(), test_ai());
        assert_eq!(screen.step(), ReviewStep::Confirm);
        assert_eq!(
            screen.confirm.as_ref().unwrap().selected(),
            ConfirmationChoice::Cancel
        );
    }

    #[test]
    fn confirm_renders_pr_row_steps_and_review_ai_table() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        let dump = render_dump(&mut screen, 110, 36);
        assert!(dump.contains("Review Pull Request #42?"), "{dump}");
        assert!(dump.contains("#42"), "{dump}");
        assert!(dump.contains("Add retry logic"), "{dump}");
        assert!(dump.contains("Will run:"), "{dump}");
        assert!(dump.contains("strong"), "{dump}");
        assert!(dump.contains("balanced"), "{dump}");
        assert!(dump.contains("utility"), "{dump}");
        assert!(dump.contains("opencode/review-scan"), "{dump}");
    }

    #[test]
    fn confirm_default_no_cancels_but_tab_confirms() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::Cancelled
        );
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        assert_eq!(screen.handle_key(key(KeyCode::Tab)), ReviewAction::Continue);
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::Confirmed
        );
    }

    #[test]
    fn scan_pool_tracks_files_and_aggregates_findings() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.start_preparing();
        assert_eq!(screen.step(), ReviewStep::Working);
        screen.set_files(
            vec![file("a.rs"), file("b.rs")],
            "o".into(),
            "r".into(),
            "sha1".into(),
        );
        assert_eq!(screen.files_len(), 2);
        assert_eq!(screen.head_sha(), "sha1");

        screen.begin_scan_phase();
        assert!(screen.scan_phase_active());
        assert!(screen
            .phase_message
            .contains("Reviewing 2 changed files + test coverage"));
        let (group_index, group) = screen.take_next_scan_file().unwrap();
        assert_eq!(group_index, FILE_GROUP_SCAN_INDEX);
        assert_eq!(
            group
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a.rs", "b.rs"]
        );
        assert!(screen.take_next_scan_file().is_none());
        // App files changed, so the whole-diff coverage pass fills the last
        // pool slot — exactly once.
        let (coverage_index, coverage_files) = screen.take_coverage_scan().unwrap();
        assert_eq!(coverage_index, COVERAGE_SCAN_INDEX);
        assert_eq!(coverage_files.len(), 2);
        assert!(screen.take_coverage_scan().is_none());

        // One grouped result can contain findings from both files.
        screen.record_scan_result(vec![finding("b.rs", Some(3), ReviewSeverity::Critical)]);
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::Low)]);
        screen.note_scan_done(group_index);
        // The progress bar carries the count now: 1 of 2 scan units done.
        assert!(render_dump(&mut screen, 80, 14).contains("1/2"));
        assert!(screen.scans_pending());
        screen.record_scan_result(Vec::new());
        screen.note_scan_done(COVERAGE_SCAN_INDEX);
        assert!(!screen.scans_pending());

        // Aggregation sorts Critical first, and the walkthrough starts at 0.
        assert!(screen.finish_scanning());
        assert!(!screen.scan_phase_active());
        assert_eq!(screen.findings_len(), 2);
        assert_eq!(screen.current_finding().unwrap().file, "b.rs");
    }

    #[test]
    fn merged_scan_pool_dispatches_testers_and_one_combined_scan() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_scan_mode(ReviewScanMode::Merged);
        screen.set_files(
            vec![file("src/a.rs"), file("tests/a_test.rs"), file("src/b.rs")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        screen.begin_scan_phase();
        assert!(screen.phase_message.contains("+ merged review"));
        let (index, tester) = screen.take_next_scan_file().unwrap();
        assert_eq!(index, 1);
        assert_eq!(tester.files[0].path, "tests/a_test.rs");
        assert!(screen.take_next_scan_file().is_none());
        assert!(screen.take_coverage_scan().is_none());
        screen.note_scan_done(index);
        assert!(screen.scans_pending());
        assert_eq!(screen.take_coverage_scan().unwrap().1.len(), 3);
        screen.note_scan_done(COVERAGE_SCAN_INDEX);
        assert!(!screen.scans_pending());
    }

    #[test]
    fn split_pool_prioritizes_testers_and_gates_coverage_until_they_settle() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![
                file("src/a.rs"),
                file("tests/a_test.rs"),
                file("tests/b_test.rs"),
                file("src/b.rs"),
            ],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        let indices = (0..2)
            .map(|_| screen.take_next_scan_file().unwrap().0)
            .collect::<Vec<_>>();
        assert_eq!(indices, vec![1, 2]);
        assert!(screen.take_coverage_scan().is_none());

        let weak = finding("tests/a_test.rs", Some(3), ReviewSeverity::Medium);
        let second_weak = finding("tests/b_test.rs", Some(2), ReviewSeverity::Low);
        screen.record_tester_findings(indices[0], std::slice::from_ref(&weak));
        screen.record_tester_findings(indices[1], std::slice::from_ref(&second_weak));
        screen.note_scan_done(indices[0]);
        assert_eq!(screen.tester_findings(), vec![weak, second_weak]);
        assert!(screen.take_coverage_scan().is_none());
        screen.note_scan_done(indices[1]);
        assert_eq!(screen.take_coverage_scan().unwrap().1.len(), 4);
    }

    #[test]
    fn split_pool_settles_multiple_coverage_groups_independently() {
        let mut first = file("src/first.rs");
        first.annotated_diff = "x".repeat(crate::services::dashboard::REVIEW_MERGED_FOCUS_BYTES);
        let second = file("src/tail.rs");
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![first, second], "o".into(), "r".into(), "sha".into());

        let first_file = screen.take_next_scan_file().unwrap().0;
        let second_file = screen.take_next_scan_file().unwrap().0;
        let (first_group, first_files) = screen.take_coverage_scan().unwrap();
        let (second_group, second_files) = screen.take_coverage_scan().unwrap();
        assert_eq!(first_group, COVERAGE_SCAN_INDEX);
        assert_eq!(second_group, COVERAGE_SCAN_INDEX - 1);
        assert_eq!(first_files[0].path, "src/first.rs");
        assert_eq!(second_files[0].path, "src/tail.rs");

        screen.record_scan_failure(first_group, "bad group".to_string());
        screen.note_scan_done(first_group);
        screen.note_scan_done(first_file);
        screen.note_scan_done(second_file);
        assert!(
            screen.scans_pending(),
            "tail coverage group is still active"
        );
        screen.note_scan_done(second_group);
        assert!(!screen.scans_pending());
        assert!(screen.summary_rows[0]
            .command
            .contains("coverage group 1 of 2"));
        assert!(screen.coverage_group(second_group).is_some());
    }

    #[test]
    fn merged_tests_only_diff_has_no_combined_coverage_owner() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_scan_mode(ReviewScanMode::Merged);
        screen.set_files(
            vec![file("tests/a_test.rs")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        let (index, _) = screen.take_next_scan_file().unwrap();
        assert!(screen.take_coverage_scan().is_none());
        screen.note_scan_done(index);
        assert!(!screen.scans_pending());
    }

    #[test]
    fn finish_scanning_collapses_same_run_duplicates() {
        // Two scans of the same file surface the same fix with different
        // wording/severity. finish_scanning keeps the highest-severity one and
        // records the other as a muted "Duplicate" row — not in the walkthrough.
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![finding_with(
            "a.rs",
            Some(9),
            ReviewSeverity::Low,
            Some("let k = env(\"K\");"),
        )]);
        screen.record_scan_result(vec![finding_with(
            "a.rs",
            Some(4),
            ReviewSeverity::Critical,
            Some("let k = env(\"K\");"),
        )]);
        assert!(screen.finish_scanning());
        assert_eq!(screen.findings_len(), 1);
        assert_eq!(screen.current_finding().unwrap().line, Some(4)); // Critical kept
        let dup_row = screen
            .summary_rows
            .iter()
            .find(|r| r.status.as_ref().is_some_and(|s| s.label == "Duplicate"))
            .expect("a Duplicate row should be recorded");
        assert!(dup_row.command.contains("a.rs:9"));
    }

    #[test]
    fn verification_policy_filters_high_risk_and_withholds_failures() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        let mut low_prose = finding_with("a.rs", Some(2), ReviewSeverity::Low, None);
        low_prose.category = "Code Smell".to_string();
        low_prose.title = "Minor naming issue".to_string();
        let mut high = finding_with("a.rs", Some(2), ReviewSeverity::High, None);
        high.title = "High impact bug".to_string();
        let mut suggestion = finding_with("a.rs", Some(3), ReviewSeverity::Low, Some("fixed"));
        suggestion.category = "Convention".to_string();
        suggestion.title = "Direct replacement".to_string();
        screen.record_scan_result(vec![low_prose.clone(), high, suggestion]);
        assert!(screen.finish_scanning());
        let candidates = screen.begin_verification();
        assert_eq!(
            candidates.len(),
            2,
            "low-severity prose should bypass verifier"
        );
        let routed_profiles = candidates
            .iter()
            .map(|(_, _, finding, strong)| (finding.title.as_str(), *strong))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(routed_profiles.get("High impact bug"), Some(&true));
        assert_eq!(routed_profiles.get("Direct replacement"), Some(&false));
        let rejected = candidates[0].0;
        screen.record_verification(
            rejected,
            Ok(ReviewVerification::RejectedFalsePositive {
                reason: "guard already applies".to_string(),
            }),
        );
        let failed = candidates[1].0;
        screen.record_verification(failed, Err("malformed verifier output".to_string()));
        assert!(!screen.verification_pending());
        assert!(screen.finish_verification());
        assert_eq!(screen.findings, vec![low_prose]);
        let rejected_row = screen
            .summary_rows
            .iter()
            .find(|row| {
                row.status
                    .as_ref()
                    .is_some_and(|status| status.label == "Rejected false positive")
            })
            .expect("rejection is recorded on the summary");
        assert!(
            rejected_row.success,
            "a rejected false positive is a correct verifier decision, not a failure"
        );
        let withheld_row = screen
            .summary_rows
            .iter()
            .find(|row| {
                row.status
                    .as_ref()
                    .is_some_and(|status| status.label == "Unverified — withheld")
            })
            .expect("a verifier error is recorded on the summary");
        assert!(
            !withheld_row.success,
            "a verifier that errors out is a genuine failure"
        );
    }

    #[test]
    fn verifier_revision_replaces_candidate_without_colliding_shared_anchor() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        let mut first = finding_with("a.rs", Some(2), ReviewSeverity::High, None);
        first.title = "First concern".to_string();
        let mut second = finding_with("a.rs", Some(2), ReviewSeverity::High, None);
        second.title = "Second concern".to_string();
        screen.record_scan_result(vec![first, second.clone()]);
        screen.finish_scanning();
        let candidates = screen.begin_verification();
        let mut revised = candidates[0].2.clone();
        revised.title = "Corrected first concern".to_string();
        screen.record_verification(
            candidates[0].0,
            Ok(ReviewVerification::Revise {
                reason: "anchor retained".to_string(),
                finding: revised.clone(),
            }),
        );
        screen.record_verification(
            candidates[1].0,
            Ok(ReviewVerification::Confirmed {
                reason: "independent concern".to_string(),
            }),
        );
        screen.finish_verification();
        assert_eq!(screen.findings.len(), 2);
        assert!(screen.findings.contains(&revised));
        assert!(screen.findings.contains(&second));
        let revised_row = screen
            .summary_rows
            .iter()
            .find(|row| {
                row.status
                    .as_ref()
                    .is_some_and(|status| status.label == "Revised")
            })
            .expect("a revision is recorded on the summary");
        assert!(
            revised_row.success,
            "a revised finding is a correct verifier decision, not a failure"
        );
    }

    #[test]
    fn gap_audit_runs_only_for_decomposed_application_reviews() {
        let mut large = file("src/large.rs");
        large.annotated_diff = "x".repeat(crate::services::dashboard::REVIEW_MERGED_FOCUS_BYTES);
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![large, file("src/tail.rs")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        assert!(screen.should_run_gap_audit());
        screen.begin_gap_audit();
        assert!(!screen.should_run_gap_audit());
        assert!(screen.scan_phase_active());

        let mut tests_only = ReviewPullRequestScreen::new(request(), test_ai());
        tests_only.set_files(
            vec![file("tests/a_test.rs"), file("e2e/login.cy.ts")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        assert!(!tests_only.should_run_gap_audit());
    }

    #[test]
    fn gap_audit_failure_keeps_primary_findings_and_duplicates_are_suppressed() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        let primary = finding_with("a.rs", Some(2), ReviewSeverity::Medium, None);
        screen.record_scan_result(vec![primary.clone()]);
        screen.record_gap_audit_result(Ok(vec![primary.clone()]));
        assert_eq!(screen.findings, vec![primary.clone()]);
        assert!(screen.summary_rows.iter().any(|row| row
            .status
            .as_ref()
            .is_some_and(|status| status.label == "Duplicate")));

        screen.record_gap_audit_result(Err("audit unavailable".to_string()));
        assert_eq!(screen.findings, vec![primary]);
        assert!(screen.summary_rows.iter().any(|row| {
            row.status
                .as_ref()
                .is_some_and(|status| status.label == "Failed — primary findings kept")
        }));
    }

    #[test]
    fn findings_of_equal_severity_keep_diff_order_despite_arrival_order() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![file("a.rs"), file("b.rs")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        screen.begin_scan_phase();
        screen.take_next_scan_file();
        screen.take_next_scan_file();
        // b.rs (second in the diff) finishes before a.rs.
        screen.record_scan_result(vec![finding("b.rs", Some(3), ReviewSeverity::High)]);
        screen.note_scan_done(1);
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.note_scan_done(0);
        screen.finish_scanning();
        assert_eq!(screen.current_finding().unwrap().file, "a.rs");
    }

    #[test]
    fn scanning_working_view_names_the_in_flight_files() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![file("src/lib/deep.rs"), file("src/other.rs")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        screen.begin_scan_phase();
        screen.take_next_scan_file();
        screen.take_next_scan_file();
        screen.take_coverage_scan();
        let dump = render_dump(&mut screen, 80, 12);
        assert!(
            dump.contains("Reviewing 2 changed files + test coverage"),
            "{dump}"
        );
        assert!(dump.contains("Under review"), "{dump}");
        assert!(dump.contains("src/lib/deep.rs"), "{dump}");
        assert!(dump.contains("src/other.rs"), "{dump}");
        assert!(dump.contains("coverage"), "{dump}");
        // The pipeline stepper and the severity tally replace the opaque
        // "N finding(s) so far" line.
        assert!(dump.contains("Scan"), "{dump}");
        assert!(dump.contains("Verify"), "{dump}");
        assert!(dump.contains("Findings"), "{dump}");
    }

    #[test]
    fn scanning_dashboard_breaks_findings_down_by_severity() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        screen.begin_scan_phase();
        screen.take_next_scan_file();
        screen.record_scan_result(vec![
            finding("a.rs", Some(2), ReviewSeverity::Critical),
            finding("a.rs", Some(3), ReviewSeverity::Low),
            finding("a.rs", Some(4), ReviewSeverity::Low),
        ]);
        // One Critical, two Low, three total — broken out per severity.
        assert_eq!(screen.severity_counts(), [1, 0, 0, 2]);
        let dump = render_dump(&mut screen, 80, 12);
        // The severity tally with colored circles replaces "N finding(s) so far".
        assert!(dump.contains("Findings"), "{dump}");
        assert!(dump.contains("🔴"), "{dump}");
        assert!(dump.contains("⚪"), "{dump}");
        assert!(dump.contains("· 3 total"), "{dump}");
        assert!(!dump.contains("finding(s) so far"), "{dump}");
    }

    #[test]
    fn verify_phase_shows_verify_stage_active_with_progress() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        screen.begin_scan_phase();
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::Critical)]);
        screen.finish_scanning();
        let candidates = screen.begin_verification();
        assert_eq!(candidates.len(), 1, "a Critical finding needs verifying");
        let dump = render_dump(&mut screen, 80, 12);
        assert!(dump.contains("Verify"), "{dump}");
        // The bar tracks verification progress: none confirmed yet.
        assert!(dump.contains("0/1"), "{dump}");
        // The panel describes the pass rather than listing files.
        assert!(dump.contains("re-checking"), "{dump}");
    }

    #[test]
    fn scan_failure_records_row_and_pool_continues() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![file("a.rs"), file("b.rs")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        screen.begin_scan_phase();
        let group_index = screen.take_next_scan_file().unwrap().0;
        screen.record_scan_failure(group_index, "model returned garbage".to_string());
        screen.note_scan_done(group_index);
        assert!(screen.scans_pending());
        let row = &screen.summary_rows[0];
        assert_eq!(row.status.as_ref().unwrap().label, "Failed");
        assert!(row.command.contains("a.rs"));
    }

    #[test]
    fn tests_only_diff_runs_no_coverage_pass() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![file("tests/a_test.rs")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        screen.begin_scan_phase();
        assert!(screen.phase_message.contains("Reviewing 1 changed file"));
        screen.take_next_scan_file();
        assert!(screen.take_coverage_scan().is_none());
        screen.record_scan_result(Vec::new());
        screen.note_scan_done(0);
        assert!(!screen.scans_pending());
    }

    #[test]
    fn coverage_scan_failure_row_names_the_pass() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        screen.begin_scan_phase();
        screen.take_next_scan_file();
        screen.take_coverage_scan();
        screen.record_scan_failure(COVERAGE_SCAN_INDEX, "model returned garbage".to_string());
        screen.note_scan_done(COVERAGE_SCAN_INDEX);
        assert!(screen.scans_pending(), "a.rs is still out");
        let row = &screen.summary_rows[0];
        assert_eq!(row.status.as_ref().unwrap().label, "Failed");
        assert!(row.command.contains("coverage group 1 of 1"));
    }

    #[test]
    fn merged_scan_failure_row_names_the_combined_pass() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_scan_mode(ReviewScanMode::Merged);
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        screen.begin_scan_phase();
        screen.take_coverage_scan();
        screen.record_scan_failure(COVERAGE_SCAN_INDEX, "model returned garbage".to_string());
        screen.note_scan_done(COVERAGE_SCAN_INDEX);
        assert!(!screen.scans_pending());
        let row = &screen.summary_rows[0];
        assert_eq!(row.status.as_ref().unwrap().label, "Failed");
        assert!(row.command.contains("merged review"));
    }

    #[test]
    fn split_existing_duplicates_checks_each_coverage_finding_against_its_file() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        let mut a = file("a.rs");
        a.existing_keys = vec![crate::services::dashboard::ExistingFindingKey {
            line: Some(2),
            title: "hardcoded api key".to_string(),
        }];
        screen.set_files(vec![a, file("b.rs")], "o".into(), "r".into(), "sha".into());
        let (fresh, duplicates) = screen.split_existing_duplicates(
            COVERAGE_SCAN_INDEX,
            vec![
                finding("a.rs", Some(2), ReviewSeverity::High), // already on the PR
                finding("b.rs", Some(2), ReviewSeverity::High), // b.rs has no keys
            ],
        );
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].file, "b.rs");
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].file, "a.rs");
    }

    #[test]
    fn decision_buttons_emit_each_action() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.enter_decision();
        assert_eq!(screen.step(), ReviewStep::Decision);
        // Default focus = Post; cycle Post → Edit → Other → Skip.
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), ReviewAction::Post);
        screen.handle_key(key(KeyCode::Right)); // Edit — exercised in its own tests
        screen.handle_key(key(KeyCode::Right));
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), ReviewAction::Other);
        assert_eq!(
            screen.handle_key(key(KeyCode::Right)),
            ReviewAction::Continue
        );
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), ReviewAction::Skip);
        // Esc on a finding skips it (keeps the loop going).
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), ReviewAction::Skip);
    }

    #[test]
    fn decision_buttons_center_their_labels() {
        // Each button is sized to its symmetrically padded label, so the text
        // sits with equal space on both sides — no `Alignment::Center` parity
        // drift regardless of whether the label width is odd or even.
        let mut screen = screen_on_decision();
        let dump = render_dump(&mut screen, 110, 20);
        assert!(dump.contains("│  Post  │"), "{dump}");
        assert!(dump.contains("│  Edit  │"), "{dump}");
        assert!(dump.contains("│  Other  │"), "{dump}");
        assert!(dump.contains("│  Skip  │"), "{dump}");
    }

    #[test]
    fn summary_buttons_center_their_labels() {
        let mut screen = screen_on_decision();
        screen.enter_summary("Review summary body".into());
        let dump = render_dump(&mut screen, 110, 20);
        assert!(dump.contains("│  Request changes  │"), "{dump}");
        assert!(dump.contains("│  Comment  │"), "{dump}");
        assert!(dump.contains("│  Skip  │"), "{dump}");
    }

    #[test]
    fn summary_panel_scrolls_naturally_and_clamps_to_the_bottom() {
        let mut screen = screen_on_decision();
        let body = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        screen.enter_summary(body);
        // Render short so the panel overflows and the max-scroll cache fills.
        let _ = render_dump(&mut screen, 80, 16);
        let max = screen.decision_max_scroll.get();
        assert!(max > 0, "summary should overflow a 16-row viewport");

        // Wheel down moves toward the bottom; wheel up moves back up.
        assert!(screen.handle_mouse_scroll_down(3));
        assert_eq!(screen.decision_scroll, 3);
        assert!(screen.handle_mouse_scroll_up(2));
        assert_eq!(screen.decision_scroll, 1);
        // Scrolling above the top saturates at 0, never negative.
        assert!(screen.handle_mouse_scroll_up(50));
        assert_eq!(screen.decision_scroll, 0);

        // Scrolling far past the bottom is clamped to `max`, so a single
        // wheel-up immediately reverses instead of burning dead ticks.
        for _ in 0..100 {
            screen.handle_mouse_scroll_down(3);
        }
        assert_eq!(screen.decision_scroll, max);
        screen.handle_mouse_scroll_up(1);
        assert_eq!(screen.decision_scroll, max - 1);
    }

    fn screen_on_decision() -> ReviewPullRequestScreen {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.enter_decision();
        screen
    }

    fn enter_edit(screen: &mut ReviewPullRequestScreen) {
        screen.handle_key(key(KeyCode::Right)); // focus Edit
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::Continue
        );
        assert_eq!(screen.step(), ReviewStep::EditFinding);
    }

    #[test]
    fn edit_saves_severity_title_and_suggestion_changes() {
        let mut screen = screen_on_decision();
        enter_edit(&mut screen);
        // Severity row: → cycles High → Medium.
        screen.handle_key(key(KeyCode::Right));
        // Title row: Enter opens the prefilled editor; append " v2".
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Enter));
        for c in " v2".chars() {
            screen.handle_key(key(KeyCode::Char(c)));
        }
        screen.handle_key(key(KeyCode::Enter));
        // Suggestion row: Enter toggles the block off.
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Enter));
        // Save with S — back on Decision with the draft committed.
        screen.handle_key(key(KeyCode::Char('s')));
        assert_eq!(screen.step(), ReviewStep::Decision);
        let f = screen.current_finding().unwrap();
        assert_eq!(f.severity, ReviewSeverity::Medium);
        assert_eq!(f.title, "Hardcoded API key v2");
        assert_eq!(f.suggestion, None);
    }

    #[test]
    fn edit_cancel_discards_every_draft_change() {
        let mut screen = screen_on_decision();
        enter_edit(&mut screen);
        screen.handle_key(key(KeyCode::Right)); // severity High → Medium
        screen.handle_key(key(KeyCode::Esc));
        assert_eq!(screen.step(), ReviewStep::Decision);
        let f = screen.current_finding().unwrap();
        assert_eq!(f.severity, ReviewSeverity::High);
        assert!(f.suggestion.is_some());
    }

    #[test]
    fn edit_suggestion_toggle_restores_the_removed_block() {
        let mut screen = screen_on_decision();
        let original = screen.current_finding().unwrap().suggestion;
        enter_edit(&mut screen);
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Down)); // Suggestion row
        screen.handle_key(key(KeyCode::Left)); // remove
        screen.handle_key(key(KeyCode::Left)); // restore
        screen.handle_key(key(KeyCode::Char('s')));
        assert_eq!(screen.current_finding().unwrap().suggestion, original);
    }

    #[test]
    fn edit_page_renders_fields_live_preview_and_footer() {
        let mut screen = screen_on_decision();
        enter_edit(&mut screen);
        let dump = render_dump(&mut screen, 110, 30);
        assert!(dump.contains("Edit finding #1 of 1"), "{dump}");
        assert!(dump.contains("no AI call, no tokens"), "{dump}");
        assert!(dump.contains("Severity"), "{dump}");
        assert!(dump.contains("‹ High ›"), "{dump}");
        assert!(dump.contains("Hardcoded API key"), "{dump}");
        assert!(dump.contains("Explanation"), "{dump}");
        assert!(dump.contains("Kept"), "{dump}");
        assert!(dump.contains("Preview"), "{dump}");
        assert!(dump.contains("Save"), "{dump}");
        assert!(dump.contains("Cancel"), "{dump}");
    }

    #[test]
    fn skipped_files_get_muted_success_rows_with_the_reason() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.record_skipped_files(&[ReviewSkippedFile {
            path: "Cargo.lock".to_string(),
            reason: "lockfile",
        }]);
        let row = &screen.summary_rows[0];
        assert!(row.success, "a skip is not a failure");
        assert_eq!(row.status.as_ref().unwrap().label, "Skipped (lockfile)");
        assert!(row.command.contains("Cargo.lock"));
    }

    #[test]
    fn deduped_findings_get_already_posted_rows() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.record_duplicate_findings(&[finding("a.rs", Some(2), ReviewSeverity::High)]);
        let row = &screen.summary_rows[0];
        assert!(row.success, "a dedup is not a failure");
        assert_eq!(row.status.as_ref().unwrap().label, "Already posted");
        assert!(row.command.contains("a.rs:2"));
    }

    #[test]
    fn decision_renders_comment_preview_and_buttons() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![file("src/auth.rs")],
            "o".into(),
            "r".into(),
            "s".into(),
        );
        screen.record_scan_result(vec![finding(
            "src/auth.rs",
            Some(2),
            ReviewSeverity::Critical,
        )]);
        screen.finish_scanning();
        screen.enter_decision();
        let dump = render_dump(&mut screen, 110, 26);
        assert!(dump.contains("Finding #1 of 1"), "{dump}");
        assert!(dump.contains("[Security] [Critical]"), "{dump}");
        assert!(dump.contains("src/auth.rs:2"), "{dump}");
        assert!(dump.contains("Proposed comment"), "{dump}");
        assert!(dump.contains("Hardcoded API key"), "{dump}");
        assert!(dump.contains("Secrets in source"), "{dump}");
        assert!(dump.contains("Suggested replacement"), "{dump}");
        assert!(dump.contains("env::var"), "{dump}");
        assert!(dump.contains("Post"), "{dump}");
        assert!(dump.contains("Edit"), "{dump}");
        assert!(dump.contains("Other"), "{dump}");
        assert!(dump.contains("Skip"), "{dump}");
    }

    #[test]
    fn file_level_finding_preview_names_the_file() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        let mut f = finding("a.rs", None, ReviewSeverity::Medium);
        f.suggestion = None;
        screen.record_scan_result(vec![f]);
        screen.finish_scanning();
        screen.enter_decision();
        let dump = render_dump(&mut screen, 100, 24);
        assert!(dump.contains("general PR comment"), "{dump}");
    }

    #[test]
    fn other_input_submits_as_revise() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.enter_decision();
        screen.show_other_input();
        assert_eq!(screen.step(), ReviewStep::OtherInput);
        screen.handle_key(key(KeyCode::Char('n')));
        screen.handle_key(key(KeyCode::Char('o')));
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::Revise("no".to_string())
        );
    }

    #[test]
    fn other_input_cancel_returns_to_decision() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.enter_decision();
        screen.show_other_input();
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), ReviewAction::Continue);
        assert_eq!(screen.step(), ReviewStep::Decision);
    }

    #[test]
    fn show_revised_swaps_the_current_finding_in_place() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.enter_decision();
        let mut revised = finding("a.rs", Some(3), ReviewSeverity::High);
        revised.title = "Use a secrets manager".to_string();
        screen.show_revised(revised);
        assert_eq!(screen.step(), ReviewStep::Decision);
        assert_eq!(
            screen.current_finding().unwrap().title,
            "Use a secrets manager"
        );
    }

    #[test]
    fn failed_revision_returns_to_the_existing_finding() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        let original = finding("a.rs", Some(2), ReviewSeverity::High);
        screen.record_scan_result(vec![original.clone()]);
        screen.finish_scanning();
        screen.enter_decision();
        screen.start_revising();
        screen.reshow_decision();
        assert_eq!(screen.step(), ReviewStep::Decision);
        assert_eq!(screen.current_finding(), Some(original));
    }

    #[test]
    fn record_outcome_builds_colored_rows_and_tracks_posted() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        // Three genuinely distinct findings (distinct fixes), so the same-run
        // dedup keeps all three.
        let mut critical = finding_with("a.rs", Some(2), ReviewSeverity::Critical, Some("fix a"));
        critical.title = "Critical concern".to_string();
        let mut high = finding_with("a.rs", Some(3), ReviewSeverity::High, Some("fix b"));
        high.title = "High concern".to_string();
        let mut low = finding_with("a.rs", None, ReviewSeverity::Low, Some("fix c"));
        low.title = "Low concern".to_string();
        screen.record_scan_result(vec![critical, high, low]);
        screen.finish_scanning();
        screen.record_outcome(ReviewRowOutcome::Posted);
        assert!(screen.advance_finding());
        screen.record_outcome(ReviewRowOutcome::Skipped);
        assert!(screen.advance_finding());
        screen.record_outcome(ReviewRowOutcome::Failed("boom".to_string()));
        assert!(!screen.advance_finding());

        assert_eq!(screen.summary_rows.len(), 3);
        assert_eq!(screen.posted_findings().len(), 1);
        let posted = &screen.summary_rows[0];
        assert_eq!(posted.status.as_ref().unwrap().label, "Posted");
        assert_eq!(posted.status.as_ref().unwrap().color, colors::SUCCESS);
        assert!(posted.command.starts_with("#1 a.rs:2"));
        let skipped = &screen.summary_rows[1];
        assert_eq!(skipped.status.as_ref().unwrap().label, "Skipped");
        let failed = &screen.summary_rows[2];
        assert_eq!(failed.status.as_ref().unwrap().label, "Failed");
        assert_eq!(failed.failure.as_deref(), Some("boom"));
    }

    #[test]
    fn summary_buttons_emit_each_action() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.enter_summary("## Review Summary\nbody".to_string());
        assert_eq!(screen.step(), ReviewStep::Summary);
        // Default focus = Comment (the non-blocking choice).
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::SubmitSummary {
                request_changes: false
            }
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Left)),
            ReviewAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::SubmitSummary {
                request_changes: true
            }
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Right)),
            ReviewAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Right)),
            ReviewAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::SkipSummary
        );
        // Esc also skips the summary.
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            ReviewAction::SkipSummary
        );
    }

    #[test]
    fn summary_renders_body_and_buttons() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.enter_summary("## Review Summary\n\n1. **[Security] [Critical]**".to_string());
        let dump = render_dump(&mut screen, 110, 24);
        assert!(
            dump.contains("Review summary for Pull Request #42"),
            "{dump}"
        );
        assert!(dump.contains("Will be submitted"), "{dump}");
        assert!(dump.contains("## Review Summary"), "{dump}");
        assert!(dump.contains("Request changes"), "{dump}");
        assert!(dump.contains("Comment"), "{dump}");
        assert!(dump.contains("Skip"), "{dump}");
    }

    #[test]
    fn done_renders_results_table_and_headline() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.record_outcome(ReviewRowOutcome::Posted);
        screen.record_summary_outcome(false, Ok(()));
        screen.enter_done();
        let dump = render_dump(&mut screen, 100, 16);
        assert!(dump.contains("Posted 1 review comment"), "{dump}");
        assert!(dump.contains("Submitted"), "{dump}");
        assert!(dump.contains("Press any key"), "{dump}");
    }

    #[test]
    fn done_report_includes_aggregate_scan_telemetry_once() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.record_scan_telemetry(ReviewScanTelemetry {
            scan: "app:a.rs".to_string(),
            scan_role: "application".to_string(),
            retry_role: "initial".to_string(),
            model_profile: "balanced".to_string(),
            model: "openai/gpt-5.6-terra".to_string(),
            thinking: "medium".to_string(),
            prompt_bytes: 1200,
            usage: crate::services::review_telemetry::ReviewTokenUsage {
                uncached_input: Some(40_000),
                cache_read: Some(0),
                cache_write: Some(0),
                output: Some(8_000),
                reasoning: Some(0),
                cost_usd: None,
            },
            duration_ms: 250,
            findings: 1,
        });
        screen.enter_done();
        screen.enter_done();
        let rows = screen
            .summary_rows
            .iter()
            .filter(|row| row.command == "AI scan usage")
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status.as_ref().unwrap().label,
            "~48k logical tokens across 1 call"
        );
    }

    #[test]
    fn done_with_no_findings_reports_clean_review() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.finish_scanning();
        screen.enter_done();
        let dump = render_dump(&mut screen, 100, 12);
        assert!(dump.contains("No issues found"), "{dump}");
    }

    #[test]
    fn set_error_shows_error_view() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_error("boom".to_string());
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('x'))),
            ReviewAction::Cancelled
        );
        let dump = render_dump(&mut screen, 80, 6);
        assert!(dump.contains("Cannot review pull request"), "{dump}");
    }
}
