//! The "humane" voice of the Review command: comments and the review summary
//! written the way a teammate types them. Discovery is untouched — only the
//! wording of what gets posted changes. Everything here is deterministic (prompt assembly,
//! parsing, rendering); the AI calls live on `DashboardService`.

use crate::errors::Result;
use crate::services::dashboard::{substitute_review_prompt, ReviewFinding};
use crate::services::review_telemetry::ReviewScanTelemetry;

const HUMANIZE_PROMPT: &str = include_str!("../../prompts/reviewer_humanize.md");
const HUMANE_SUMMARY_PROMPT: &str = include_str!("../../prompts/reviewer_summary_humane.md");
const VOICE_PROMPT: &str = include_str!("../../prompts/reviewer_voice.md");

const HUMANIZE_BEGIN: &str = "===WISETREE-HUMANIZE-BEGIN===";
const HUMANIZE_END: &str = "===WISETREE-HUMANIZE-END===";
const SUMMARY_BEGIN: &str = "===WISETREE-SUMMARY-BEGIN===";
const SUMMARY_END: &str = "===WISETREE-SUMMARY-END===";

/// Findings rewritten per humanize call. Every call pays the harness's fixed
/// system-prompt overhead (~10k tokens) while each rewritten comment is only
/// ~150 output tokens, so one call covers a typical review; only a very
/// large one is split.
pub const HUMANIZE_BATCH_SIZE: usize = 25;

/// How the posted comments and the summary are worded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewVoice {
    /// Conversational comments and a short human summary with the key
    /// concerns.
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

/// The humane summary prompt: every posted finding with its posted wording.
pub fn build_humane_summary_prompt(posted: &[ReviewFinding], pr_title: &str) -> String {
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
    substitute_review_prompt(HUMANE_SUMMARY_PROMPT, &[("SUMMARY_FACTS", &facts)])
}

/// Accept the model's summary opening only from its marker block (so no
/// stray CLI output can reach the PR) and only when it is plain markdown
/// prose.
pub fn validate_humane_summary_overview(output: &str) -> Option<String> {
    let block = marker_block(output, SUMMARY_BEGIN, SUMMARY_END)?;
    let text = typed_punctuation(block.trim());
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

    #[test]
    fn humane_summary_prompt_lists_the_posted_wording() {
        let posted = [finding(Some(255), None)];
        let prompt = build_humane_summary_prompt(&posted, "Fix WASM");
        assert!(prompt.contains("Pull request title: Fix WASM"));
        assert!(prompt
            .contains("1. [High] [Test] Assertion accepts any error (tests/evalraw.test.js:255)"));
        assert!(prompt.contains("Posted comment: `.toThrow()` with no argument"));
        assert!(!prompt.contains("SUMMARY_FACTS"));
        let bare = build_humane_summary_prompt(&[], "Fix WASM");
        assert!(bare.contains("Approved findings: none"));
    }

    #[test]
    fn humane_summary_falls_back_to_a_short_opening() {
        assert_eq!(deterministic_humane_overview(0), "Looks good to me 👍");
        assert_eq!(deterministic_humane_overview(1), "Left one comment inline.");
        assert_eq!(
            deterministic_humane_overview(3),
            "Left a few comments inline."
        );
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
}
