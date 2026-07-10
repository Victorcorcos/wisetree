//! Pure, synchronous Bugkill logic: the hypothesis data model, the
//! investigate-contract parser, the `BUG_INVESTIGATION.md` renderer + resume
//! parser, the attempt change-set computation, and the ranking/quality
//! clamping. No I/O lives here — `DashboardService` owns every git/AI call
//! and `App` owns the async orchestration, so everything in this module is
//! unit-testable with plain strings.

/// How solid the evidence behind a hypothesis is. Order matters only for
/// display; the consistency clamps in [`normalize_hypotheses`] key off
/// `Confirmed` / `Speculative` explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceQuality {
    Confirmed,
    Observed,
    Inferred,
    Speculative,
}

impl EvidenceQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceQuality::Confirmed => "confirmed",
            EvidenceQuality::Observed => "observed",
            EvidenceQuality::Inferred => "inferred",
            EvidenceQuality::Speculative => "speculative",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "confirmed" => Some(EvidenceQuality::Confirmed),
            "observed" => Some(EvidenceQuality::Observed),
            "inferred" => Some(EvidenceQuality::Inferred),
            "speculative" => Some(EvidenceQuality::Speculative),
            _ => None,
        }
    }
}

/// One ranked root-cause hypothesis — a row of `BUG_INVESTIGATION.md`. The
/// harness holds the `Vec<BugHypothesis>` in memory and re-renders the whole
/// file from it after every mutation; the file is output for the human,
/// never input for the AI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugHypothesis {
    /// 1-based, assigned after sorting by ranking desc.
    pub number: usize,
    /// Problem + key evidence + affected code path.
    pub description: String,
    /// 1..=5.
    pub ranking: u8,
    pub quality: EvidenceQuality,
    /// Detailed fix plan.
    pub solution: String,
    /// `Implemented?` column: false = blank, true = 🟢.
    pub implemented: bool,
    /// `Worked?` column: None = blank, Some(true) = 🟢, Some(false) = 🔴.
    pub worked: Option<bool>,
}

impl BugHypothesis {
    /// A row is eligible for a fix attempt iff it has not been implemented
    /// and carries no verdict. A failed attempt (`implemented` +
    /// `worked == Some(false)`) is never offered again; `worked == Some(true)`
    /// on any row ends the loop.
    pub fn eligible(&self) -> bool {
        !self.implemented && self.worked.is_none()
    }
}

// ── Investigate-contract parser ─────────────────────────────────────────

const HYPOTHESIS_OPEN: &str = "==== HYPOTHESIS ====";
const HYPOTHESIS_CLOSE: &str = "==== END ====";

/// Parse the investigate AI's transcript into raw hypotheses. A block opens
/// at a line exactly `==== HYPOTHESIS ====` and closes at `==== END ====`;
/// inside, a field starts at a line beginning with one of the four keys
/// followed by `:` and continues until the next key or the closing marker.
/// Text outside blocks (opencode transcript noise) is ignored.
///
/// Returns `None` when no block is present, a block is missing a field, a
/// `RANKING` is not an integer in 1..=5, or a `QUALITY` is not one of the
/// four lowercase values — one invalid block invalidates the whole parse
/// (triggering the caller's single corrective retry).
pub fn parse_hypotheses(transcript: &str) -> Option<Vec<BugHypothesis>> {
    let mut hypotheses = Vec::new();
    let mut block: Option<Vec<&str>> = None;
    for line in transcript.lines() {
        match (
            &mut block,
            line.trim() == HYPOTHESIS_OPEN,
            line.trim() == HYPOTHESIS_CLOSE,
        ) {
            (None, true, _) => block = Some(Vec::new()),
            (None, false, _) => {}
            (Some(lines), _, true) => {
                hypotheses.push(parse_hypothesis_block(lines)?);
                block = None;
            }
            (Some(lines), _, false) => lines.push(line),
        }
    }
    // An unterminated block or an empty transcript is a parse failure, not
    // an empty success.
    if block.is_some() || hypotheses.is_empty() {
        return None;
    }
    Some(hypotheses)
}

fn parse_hypothesis_block(lines: &[&str]) -> Option<BugHypothesis> {
    const KEYS: [&str; 4] = ["DESCRIPTION:", "RANKING:", "QUALITY:", "SOLUTION:"];
    let mut fields: [Option<String>; 4] = [None, None, None, None];
    let mut current: Option<usize> = None;
    for line in lines {
        if let Some(idx) = KEYS.iter().position(|key| line.starts_with(key)) {
            fields[idx] = Some(line[KEYS[idx].len()..].trim_start().to_string());
            current = Some(idx);
        } else if let Some(idx) = current {
            let value = fields[idx].as_mut().expect("current field is set");
            value.push('\n');
            value.push_str(line);
        }
        // Lines before the first key are contract violations; tolerate them
        // as noise rather than failing the block.
    }
    let [description, ranking, quality, solution] = fields;
    let ranking: u8 = ranking?.trim().parse().ok()?;
    if !(1..=5).contains(&ranking) {
        return None;
    }
    Some(BugHypothesis {
        number: 0, // assigned by normalize_hypotheses
        description: description?.trim().to_string(),
        ranking,
        quality: EvidenceQuality::parse(&quality?)?,
        solution: solution?.trim().to_string(),
        implemented: false,
        worked: None,
    })
}

/// Maximum number of hypotheses kept after normalization; the lowest-ranked
/// overflow is dropped.
pub const MAX_HYPOTHESES: usize = 8;

/// Validation + normalization, all in Rust: clamp `ranking` to 1..=5, apply
/// the consistency clamps (`Speculative` forces ranking ≤ 2; ranking 5
/// requires `Confirmed`, else 4), stable-sort by ranking descending, assign
/// 1-based `number`s, and cap at [`MAX_HYPOTHESES`].
pub fn normalize_hypotheses(mut hypotheses: Vec<BugHypothesis>) -> Vec<BugHypothesis> {
    for h in &mut hypotheses {
        h.ranking = h.ranking.clamp(1, 5);
        if h.quality == EvidenceQuality::Speculative && h.ranking > 2 {
            h.ranking = 2;
        }
        if h.ranking == 5 && h.quality != EvidenceQuality::Confirmed {
            h.ranking = 4;
        }
    }
    hypotheses.sort_by_key(|h| std::cmp::Reverse(h.ranking));
    hypotheses.truncate(MAX_HYPOTHESES);
    for (idx, h) in hypotheses.iter_mut().enumerate() {
        h.number = idx + 1;
    }
    hypotheses
}

/// Last ~500 bytes of a transcript (char-boundary safe), surfaced on the
/// error screen when the investigation output could not be parsed twice.
pub fn transcript_tail(transcript: &str) -> String {
    const TAIL_BYTES: usize = 500;
    let trimmed = transcript.trim_end();
    if trimmed.len() <= TAIL_BYTES {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - TAIL_BYTES;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &trimmed[start..])
}

// ── Judge-contract parser ───────────────────────────────────────────────

/// The judge AI's 3-way classification of a freeform "Other" answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeResult {
    Fixed,
    NotFixed,
    Unclear,
}

/// Parsed judge verdict. `reason` is the judge's one-sentence justification,
/// shown above the Verdict buttons on `UNCLEAR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugkillVerdict {
    pub result: JudgeResult,
    pub reason: String,
}

/// Parse the judge's `==== VERDICT ====` block. A parse failure is treated
/// as `UNCLEAR` by the caller (never an error screen), so this returns
/// `None` rather than an error.
pub fn parse_judge_verdict(transcript: &str) -> Option<BugkillVerdict> {
    const OPEN: &str = "==== VERDICT ====";
    let mut in_block = false;
    let mut result: Option<JudgeResult> = None;
    let mut reason = String::new();
    for line in transcript.lines() {
        let trimmed = line.trim();
        if !in_block {
            in_block = trimmed == OPEN;
            continue;
        }
        if trimmed == HYPOTHESIS_CLOSE {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("RESULT:") {
            result = match value.trim() {
                "FIXED" => Some(JudgeResult::Fixed),
                "NOT_FIXED" => Some(JudgeResult::NotFixed),
                "UNCLEAR" => Some(JudgeResult::Unclear),
                _ => return None,
            };
        } else if let Some(value) = trimmed.strip_prefix("REASON:") {
            reason = value.trim().to_string();
        }
    }
    result.map(|result| BugkillVerdict { result, reason })
}

// ── BUG_INVESTIGATION.md renderer + resume parser ───────────────────────

const RANKED_CAUSES_HEADING: &str = "## Ranked Causes and Solutions";
const TABLE_HEADER: &str =
    "| Description | Ranking | Quality | Solution | Implemented? | Worked? |";
const TABLE_SEPARATOR: &str =
    "|-------------|---------|---------|----------|--------------|---------|";

/// Escape a value for a markdown table cell: `|` → `\|`, newline → `<br>`.
fn escape_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', "<br>")
}

/// Reverse [`escape_cell`].
fn unescape_cell(value: &str) -> String {
    value.replace("<br>", "\n").replace("\\|", "|")
}

/// Render the whole `BUG_INVESTIGATION.md` from the in-memory model. Rows in
/// `number` order; the `## Attempt Notes` section is omitted when `notes` is
/// empty. The harness rewrites the file with this after **every** mutation.
pub fn render_investigation_md(
    bug_description: &str,
    hypotheses: &[BugHypothesis],
    notes: &[String],
) -> String {
    let mut out = String::from("# Bug Investigation\n\n## Bug Description\n\n");
    out.push_str(bug_description.trim());
    out.push_str("\n\n");
    out.push_str(RANKED_CAUSES_HEADING);
    out.push_str("\n\n");
    out.push_str(TABLE_HEADER);
    out.push('\n');
    out.push_str(TABLE_SEPARATOR);
    out.push('\n');
    for h in hypotheses {
        let implemented = if h.implemented { "🟢" } else { "" };
        let worked = match h.worked {
            Some(true) => "🟢",
            Some(false) => "🔴",
            None => "",
        };
        out.push_str(&format!(
            "| **{}. {}** | {} | {} | {} | {} | {} |\n",
            h.number,
            escape_cell(&h.description),
            "⭐️".repeat(h.ranking as usize),
            h.quality.as_str(),
            escape_cell(&h.solution),
            implemented,
            worked,
        ));
    }
    if !notes.is_empty() {
        out.push_str("\n## Attempt Notes\n\n");
        for note in notes {
            out.push_str(&format!("- {note}\n"));
        }
    }
    out
}

/// The model recovered from an existing `BUG_INVESTIGATION.md` on Resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInvestigation {
    pub bug_description: String,
    pub hypotheses: Vec<BugHypothesis>,
    pub notes: Vec<String>,
}

/// Resume parser. Accepts a file iff it contains the
/// `## Ranked Causes and Solutions` heading followed by a table whose header
/// names exactly the six Bugkill columns, and every data row yields a valid
/// number, star count, quality, and status cells. Any violation →
/// unparseable (`None`), which the preflight turns into the Overwrite/Cancel
/// prompt. Round-trip property: `parse(render(model)) == model`.
pub fn parse_investigation_md(content: &str) -> Option<ParsedInvestigation> {
    let lines: Vec<&str> = content.lines().collect();

    let heading_idx = lines
        .iter()
        .position(|l| l.trim() == RANKED_CAUSES_HEADING)?;

    // Bug description: the body between `## Bug Description` and the ranked
    // causes heading.
    let description_idx = lines[..heading_idx]
        .iter()
        .position(|l| l.trim() == "## Bug Description")?;
    let bug_description = lines[description_idx + 1..heading_idx]
        .join("\n")
        .trim()
        .to_string();

    // Table header + separator must follow the heading (blank lines allowed
    // in between).
    let mut cursor = heading_idx + 1;
    while cursor < lines.len() && lines[cursor].trim().is_empty() {
        cursor += 1;
    }
    let header_cells = split_row(lines.get(cursor)?)?;
    let expected = [
        "Description",
        "Ranking",
        "Quality",
        "Solution",
        "Implemented?",
        "Worked?",
    ];
    if header_cells.len() != expected.len()
        || header_cells
            .iter()
            .zip(expected.iter())
            .any(|(got, want)| got.trim() != *want)
    {
        return None;
    }
    cursor += 1;
    // Separator row: six cells of dashes.
    let separator_cells = split_row(lines.get(cursor)?)?;
    if separator_cells.len() != 6
        || separator_cells
            .iter()
            .any(|cell| cell.trim().is_empty() || !cell.trim().chars().all(|c| c == '-'))
    {
        return None;
    }
    cursor += 1;

    let mut hypotheses = Vec::new();
    while cursor < lines.len() {
        let line = lines[cursor];
        if !line.trim_start().starts_with('|') {
            break;
        }
        hypotheses.push(parse_row(line)?);
        cursor += 1;
    }
    if hypotheses.is_empty() {
        return None;
    }

    let mut notes = Vec::new();
    if let Some(notes_idx) = lines[cursor..]
        .iter()
        .position(|l| l.trim() == "## Attempt Notes")
    {
        for line in &lines[cursor + notes_idx + 1..] {
            if let Some(note) = line.trim().strip_prefix("- ") {
                notes.push(note.to_string());
            }
        }
    }

    Some(ParsedInvestigation {
        bug_description,
        hypotheses,
        notes,
    })
}

/// Split a markdown table row into its cells, honoring the `\|` escape.
/// Returns `None` when the line is not `| … |`-delimited.
fn split_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') || trimmed.len() < 2 {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for c in inner.chars() {
        match (escaped, c) {
            (true, _) => {
                current.push('\\');
                current.push(c);
                escaped = false;
            }
            (false, '\\') => escaped = true,
            (false, '|') => cells.push(std::mem::take(&mut current)),
            (false, _) => current.push(c),
        }
    }
    if escaped {
        current.push('\\');
    }
    cells.push(current);
    Some(cells)
}

fn parse_row(line: &str) -> Option<BugHypothesis> {
    let cells = split_row(line)?;
    if cells.len() != 6 {
        return None;
    }
    let [description_cell, ranking_cell, quality_cell, solution_cell, implemented_cell, worked_cell] = [
        &cells[0], &cells[1], &cells[2], &cells[3], &cells[4], &cells[5],
    ];

    // `**N. <description>**`
    let description_cell = description_cell.trim();
    let body = description_cell.strip_prefix("**")?.strip_suffix("**")?;
    let (number_part, description) = body.split_once(". ")?;
    let number: usize = number_part.trim().parse().ok()?;
    let description = unescape_cell(description);

    // Star count 1–5; accept `⭐️`, `⭐`, or `★` (ignore variation selectors).
    let ranking = ranking_cell
        .trim()
        .chars()
        .filter(|c| matches!(c, '⭐' | '★'))
        .count();
    if !(1..=5).contains(&ranking)
        || !ranking_cell
            .trim()
            .chars()
            .all(|c| matches!(c, '⭐' | '★' | '\u{fe0f}'))
    {
        return None;
    }

    let quality = EvidenceQuality::parse(quality_cell.trim())?;
    let solution = unescape_cell(solution_cell.trim());

    let implemented = match implemented_cell.trim() {
        "" => false,
        "🟢" => true,
        _ => return None,
    };
    let worked = match worked_cell.trim() {
        "" => None,
        "🟢" => Some(true),
        "🔴" => Some(false),
        _ => return None,
    };

    Some(BugHypothesis {
        number,
        description,
        ranking: ranking as u8,
        quality,
        solution,
        implemented,
        worked,
    })
}

// ── git status parsing + attempt change-set ─────────────────────────────

/// The tracked/untracked split of a `git status --porcelain=v2` snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PorcelainStatus {
    /// Paths with tracked changes (`1 `, `2 `, or `u ` lines).
    pub tracked: Vec<String>,
    /// Untracked paths (`? ` lines).
    pub untracked: Vec<String>,
}

/// Parse `git status --porcelain=v2` output (non-NUL mode). Ignored (`! `)
/// and header (`# `) lines are skipped.
pub fn parse_porcelain_v2(output: &str) -> PorcelainStatus {
    let mut status = PorcelainStatus::default();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("? ") {
            status.untracked.push(rest.to_string());
        } else if line.starts_with("1 ") {
            if let Some(path) = line.splitn(9, ' ').nth(8) {
                status.tracked.push(path.to_string());
            }
        } else if line.starts_with("2 ") {
            // Rename/copy: `<path>\t<origPath>` — the current path comes first.
            if let Some(paths) = line.splitn(10, ' ').nth(9) {
                let path = paths.split('\t').next().unwrap_or(paths);
                status.tracked.push(path.to_string());
            }
        } else if line.starts_with("u ") {
            if let Some(path) = line.splitn(11, ' ').nth(10) {
                status.tracked.push(path.to_string());
            }
        }
    }
    status
}

/// The rendered investigation file — always excluded from snapshots,
/// change-sets, and commits (invariant I1: it is harness-owned output).
pub const INVESTIGATION_FILE: &str = "BUG_INVESTIGATION.md";

/// What one fix attempt touched, computed by diffing the post-attempt
/// `git status` against the pre-attempt untracked snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttemptChanges {
    /// Every path the attempt changed: tracked changes + attempt-created
    /// untracked files + pre-existing untracked files whose hash changed.
    /// Used by the Esc-abort cleanup and the Done page's files-changed list.
    pub all: Vec<String>,
    /// The subset staged into the attempt commit: `all` minus the modified
    /// pre-existing untracked files (excluding those is what makes a later
    /// `git revert` unable to delete the user's own file).
    pub commit_paths: Vec<String>,
    /// Pre-existing untracked files the attempt modified — recorded as
    /// Attempt Notes and never committed or rolled back.
    pub modified_preexisting_untracked: Vec<String>,
}

impl AttemptChanges {
    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }
}

/// Compute the attempt change-set: all tracked-change paths, plus untracked
/// paths absent from the pre-attempt snapshot, plus pre-attempt untracked
/// paths whose sha256 changed — always excluding `BUG_INVESTIGATION.md`.
pub fn compute_attempt_changes(
    tracked: &[String],
    untracked_after: &[(String, String)],
    pre_untracked: &[(String, String)],
) -> AttemptChanges {
    let mut changes = AttemptChanges::default();
    for path in tracked {
        if path == INVESTIGATION_FILE {
            continue;
        }
        changes.all.push(path.clone());
        changes.commit_paths.push(path.clone());
    }
    for (path, hash) in untracked_after {
        if path == INVESTIGATION_FILE {
            continue;
        }
        match pre_untracked.iter().find(|(pre, _)| pre == path) {
            None => {
                // Created by the attempt.
                changes.all.push(path.clone());
                changes.commit_paths.push(path.clone());
            }
            Some((_, pre_hash)) if pre_hash != hash => {
                // Pre-existing untracked file the attempt modified: part of
                // the change-set (cleanup, notes) but never committed.
                changes.all.push(path.clone());
                changes.modified_preexisting_untracked.push(path.clone());
            }
            Some(_) => {}
        }
    }
    changes
}

/// Commit subject for one applied attempt:
/// `bugkill: attempt #N — <solution first line, truncated to 50 chars>`.
/// The prefix is load-bearing — the unverdicted-attempt recovery finds the
/// commit again by matching `bugkill: attempt #N — ` in `git log`.
pub fn attempt_commit_subject(number: usize, solution: &str) -> String {
    let first_line = solution.lines().next().unwrap_or("").trim();
    let clipped: String = if first_line.chars().count() <= 50 {
        first_line.to_string()
    } else {
        let head: String = first_line.chars().take(50).collect();
        format!("{head}…")
    };
    format!("{}{clipped}", attempt_commit_prefix(number))
}

/// The subject prefix used to recover an attempt commit's sha from
/// `git log` after a crash between the commit and the verdict.
pub fn attempt_commit_prefix(number: usize) -> String {
    format!("bugkill: attempt #{number} — ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hypothesis(number: usize, ranking: u8, quality: EvidenceQuality) -> BugHypothesis {
        BugHypothesis {
            number,
            description: format!("cause {number}"),
            ranking,
            quality,
            solution: format!("solution {number}"),
            implemented: false,
            worked: None,
        }
    }

    // ── eligibility ─────────────────────────────────────────────────────

    #[test]
    fn eligibility_requires_unimplemented_and_unverdicted() {
        let mut h = hypothesis(1, 3, EvidenceQuality::Inferred);
        assert!(h.eligible());
        h.implemented = true;
        assert!(!h.eligible());
        h.worked = Some(false);
        assert!(!h.eligible());
        h.implemented = false;
        h.worked = Some(true);
        assert!(!h.eligible());
    }

    // ── parse_hypotheses ────────────────────────────────────────────────

    #[test]
    fn parses_multi_block_transcript_with_noise() {
        let transcript = "\
Some opencode banner noise
==== HYPOTHESIS ====
DESCRIPTION: Off-by-one in pagination
 spanning a second line
RANKING: 4
QUALITY: observed
SOLUTION: Fix the loop bound
and add a regression test
==== END ====
chatter between blocks
==== HYPOTHESIS ====
DESCRIPTION: Stale cache entry
RANKING: 2
QUALITY: speculative
SOLUTION: Invalidate on write
==== END ====
trailing noise";
        let parsed = parse_hypotheses(transcript).expect("parses");
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0].description,
            "Off-by-one in pagination\n spanning a second line"
        );
        assert_eq!(parsed[0].ranking, 4);
        assert_eq!(parsed[0].quality, EvidenceQuality::Observed);
        assert_eq!(
            parsed[0].solution,
            "Fix the loop bound\nand add a regression test"
        );
        assert_eq!(parsed[1].quality, EvidenceQuality::Speculative);
    }

    #[test]
    fn missing_field_invalidates_the_whole_parse() {
        let transcript = "\
==== HYPOTHESIS ====
DESCRIPTION: ok block
RANKING: 3
QUALITY: inferred
SOLUTION: fine
==== END ====
==== HYPOTHESIS ====
DESCRIPTION: missing solution
RANKING: 3
QUALITY: inferred
==== END ====";
        assert_eq!(parse_hypotheses(transcript), None);
    }

    #[test]
    fn bad_ranking_or_quality_invalidates() {
        let bad_ranking = "\
==== HYPOTHESIS ====
DESCRIPTION: d
RANKING: 9
QUALITY: inferred
SOLUTION: s
==== END ====";
        assert_eq!(parse_hypotheses(bad_ranking), None);
        let non_integer = bad_ranking.replace("RANKING: 9", "RANKING: high");
        assert_eq!(parse_hypotheses(&non_integer), None);
        let bad_quality = bad_ranking
            .replace("RANKING: 9", "RANKING: 3")
            .replace("QUALITY: inferred", "QUALITY: Confirmed!");
        assert_eq!(parse_hypotheses(&bad_quality), None);
    }

    #[test]
    fn empty_or_blockless_transcript_fails() {
        assert_eq!(parse_hypotheses(""), None);
        assert_eq!(parse_hypotheses("just chatter, no blocks"), None);
    }

    #[test]
    fn unterminated_block_fails() {
        let transcript = "\
==== HYPOTHESIS ====
DESCRIPTION: d
RANKING: 3
QUALITY: inferred
SOLUTION: s";
        assert_eq!(parse_hypotheses(transcript), None);
    }

    #[test]
    fn transcript_tail_clips_long_transcripts_on_char_boundaries() {
        assert_eq!(transcript_tail("short  \n"), "short");
        let long = format!("{}é", "x".repeat(600));
        let tail = transcript_tail(&long);
        assert!(tail.starts_with('…'));
        assert!(tail.ends_with('é'));
        assert!(tail.len() <= 505);
    }

    // ── normalization ───────────────────────────────────────────────────

    #[test]
    fn speculative_is_clamped_to_two_stars() {
        let normalized = normalize_hypotheses(vec![hypothesis(0, 5, EvidenceQuality::Speculative)]);
        assert_eq!(normalized[0].ranking, 2);
    }

    #[test]
    fn five_stars_require_confirmed() {
        let normalized = normalize_hypotheses(vec![
            hypothesis(0, 5, EvidenceQuality::Inferred),
            hypothesis(0, 5, EvidenceQuality::Confirmed),
        ]);
        // The inferred one drops to 4 and sorts below the confirmed 5.
        assert_eq!(normalized[0].ranking, 5);
        assert_eq!(normalized[0].quality, EvidenceQuality::Confirmed);
        assert_eq!(normalized[1].ranking, 4);
        // 1-based numbering after the sort.
        assert_eq!(normalized[0].number, 1);
        assert_eq!(normalized[1].number, 2);
    }

    #[test]
    fn sorts_desc_stably_and_caps_at_eight() {
        let mut input = Vec::new();
        for i in 0..10 {
            let mut h = hypothesis(0, (i % 5 + 1) as u8, EvidenceQuality::Observed);
            h.description = format!("h{i}");
            input.push(h);
        }
        let normalized = normalize_hypotheses(input);
        assert_eq!(normalized.len(), MAX_HYPOTHESES);
        for pair in normalized.windows(2) {
            assert!(pair[0].ranking >= pair[1].ranking);
        }
        // Stable: equal rankings keep their input order. h4/h9 arrive as
        // 5★ Observed and get clamped to 4★ (5★ requires Confirmed).
        let fours: Vec<&str> = normalized
            .iter()
            .filter(|h| h.ranking == 4)
            .map(|h| h.description.as_str())
            .collect();
        assert_eq!(fours, ["h3", "h4", "h8", "h9"]);
    }

    // ── render / parse round-trip ───────────────────────────────────────

    fn full_model() -> (String, Vec<BugHypothesis>, Vec<String>) {
        let description = "Clicking Save crashes.\nSteps: open | edit | save.".to_string();
        let hypotheses = vec![
            BugHypothesis {
                number: 1,
                description: "Null pointer in save|path\nsecond line".to_string(),
                ranking: 5,
                quality: EvidenceQuality::Confirmed,
                solution: "Guard the null | add test".to_string(),
                implemented: true,
                worked: Some(false),
            },
            BugHypothesis {
                number: 2,
                description: "Race on autosave".to_string(),
                ranking: 3,
                quality: EvidenceQuality::Observed,
                solution: "Serialize writes".to_string(),
                implemented: true,
                worked: Some(true),
            },
            BugHypothesis {
                number: 3,
                description: "Stale config".to_string(),
                ranking: 2,
                quality: EvidenceQuality::Inferred,
                solution: "Reload config".to_string(),
                implemented: true,
                worked: None,
            },
            BugHypothesis {
                number: 4,
                description: "Cosmic rays".to_string(),
                ranking: 1,
                quality: EvidenceQuality::Speculative,
                solution: "Shielding".to_string(),
                implemented: false,
                worked: None,
            },
        ];
        let notes = vec!["Row 1: pre-existing untracked file notes.txt was modified.".to_string()];
        (description, hypotheses, notes)
    }

    #[test]
    fn render_then_parse_round_trips() {
        let (description, hypotheses, notes) = full_model();
        let rendered = render_investigation_md(&description, &hypotheses, &notes);
        let parsed = parse_investigation_md(&rendered).expect("round-trip parses");
        assert_eq!(parsed.bug_description, description);
        assert_eq!(parsed.hypotheses, hypotheses);
        assert_eq!(parsed.notes, notes);
    }

    #[test]
    fn render_omits_attempt_notes_when_empty() {
        let (description, hypotheses, _) = full_model();
        let rendered = render_investigation_md(&description, &hypotheses, &[]);
        assert!(!rendered.contains("## Attempt Notes"));
        let parsed = parse_investigation_md(&rendered).expect("parses");
        assert!(parsed.notes.is_empty());
    }

    #[test]
    fn render_escapes_pipes_and_newlines_in_cells() {
        let (description, hypotheses, notes) = full_model();
        let rendered = render_investigation_md(&description, &hypotheses, &notes);
        assert!(rendered.contains("save\\|path"));
        assert!(rendered.contains("second line"));
        assert!(rendered.contains("<br>"));
    }

    #[test]
    fn parser_accepts_all_three_star_glyphs() {
        let (description, hypotheses, _) = full_model();
        let rendered = render_investigation_md(&description, &hypotheses[..1], &[]);
        for glyph in ["⭐", "★"] {
            let variant = rendered.replace("⭐️", glyph);
            let parsed = parse_investigation_md(&variant).expect("parses");
            assert_eq!(parsed.hypotheses[0].ranking, 5);
        }
    }

    #[test]
    fn parser_rejects_renamed_or_extra_columns() {
        let (description, hypotheses, _) = full_model();
        let rendered = render_investigation_md(&description, &hypotheses, &[]);
        let renamed = rendered.replace("| Worked? |", "| Success? |");
        assert_eq!(parse_investigation_md(&renamed), None);
        let extra = rendered
            .replace("| Worked? |", "| Worked? | Extra |")
            .replace("---------|\n", "---------|---------|\n");
        assert_eq!(parse_investigation_md(&extra), None);
    }

    #[test]
    fn parser_rejects_invalid_status_cells_and_bad_rows() {
        let (description, hypotheses, _) = full_model();
        let rendered = render_investigation_md(&description, &hypotheses[..1], &[]);
        let bad_status = rendered.replace("| 🔴 |", "| maybe |");
        assert_eq!(parse_investigation_md(&bad_status), None);
        let no_number = rendered.replace("**1. ", "**");
        assert_eq!(parse_investigation_md(&no_number), None);
    }

    #[test]
    fn parser_rejects_missing_heading_or_empty_table() {
        assert_eq!(parse_investigation_md("# Some other doc"), None);
        let empty_table = format!(
            "# Bug Investigation\n\n## Bug Description\n\nbug\n\n{RANKED_CAUSES_HEADING}\n\n{TABLE_HEADER}\n{TABLE_SEPARATOR}\n"
        );
        assert_eq!(parse_investigation_md(&empty_table), None);
    }

    // ── porcelain v2 parsing ────────────────────────────────────────────

    #[test]
    fn porcelain_v2_splits_tracked_and_untracked() {
        let output = "\
# branch.oid abc
1 .M N... 100644 100644 100644 abc def src/lib.rs
2 R. N... 100644 100644 100644 abc def R100 new name.rs\told name.rs
u UU N... 100644 100644 100644 100644 abc def ghi conflicted.rs
? notes with spaces.txt
! ignored.log";
        let status = parse_porcelain_v2(output);
        assert_eq!(
            status.tracked,
            ["src/lib.rs", "new name.rs", "conflicted.rs"]
        );
        assert_eq!(status.untracked, ["notes with spaces.txt"]);
    }

    // ── attempt change-set ──────────────────────────────────────────────

    #[test]
    fn change_set_combines_tracked_created_and_modified_untracked() {
        let tracked = vec!["src/lib.rs".to_string()];
        let pre = vec![
            ("notes.txt".to_string(), "hash-a".to_string()),
            ("keep.txt".to_string(), "hash-k".to_string()),
        ];
        let after = vec![
            ("notes.txt".to_string(), "hash-b".to_string()), // modified pre-existing
            ("keep.txt".to_string(), "hash-k".to_string()),  // untouched
            ("new_file.rs".to_string(), "hash-n".to_string()), // created
        ];
        let changes = compute_attempt_changes(&tracked, &after, &pre);
        assert_eq!(changes.all, ["src/lib.rs", "notes.txt", "new_file.rs"]);
        assert_eq!(changes.commit_paths, ["src/lib.rs", "new_file.rs"]);
        assert_eq!(changes.modified_preexisting_untracked, ["notes.txt"]);
        assert!(!changes.is_empty());
    }

    #[test]
    fn change_set_always_excludes_the_investigation_file() {
        let tracked = vec![INVESTIGATION_FILE.to_string()];
        let after = vec![(INVESTIGATION_FILE.to_string(), "h".to_string())];
        let changes = compute_attempt_changes(&tracked, &after, &[]);
        assert!(changes.is_empty());
        assert!(changes.commit_paths.is_empty());
    }

    // ── judge verdict ───────────────────────────────────────────────────

    #[test]
    fn judge_verdict_parses_all_results() {
        for (raw, expected) in [
            ("FIXED", JudgeResult::Fixed),
            ("NOT_FIXED", JudgeResult::NotFixed),
            ("UNCLEAR", JudgeResult::Unclear),
        ] {
            let transcript = format!(
                "noise\n==== VERDICT ====\nRESULT: {raw}\nREASON: because.\n==== END ====\n"
            );
            let verdict = parse_judge_verdict(&transcript).expect("parses");
            assert_eq!(verdict.result, expected);
            assert_eq!(verdict.reason, "because.");
        }
    }

    #[test]
    fn judge_verdict_garbage_yields_none() {
        assert_eq!(parse_judge_verdict("no block at all"), None);
        assert_eq!(
            parse_judge_verdict("==== VERDICT ====\nRESULT: KINDA\n==== END ===="),
            None
        );
    }

    // ── commit subject ──────────────────────────────────────────────────

    #[test]
    fn commit_subject_clips_the_solution_first_line() {
        let subject = attempt_commit_subject(2, "Short plan\nsecond line ignored");
        assert_eq!(subject, "bugkill: attempt #2 — Short plan");
        let long = "x".repeat(80);
        let subject = attempt_commit_subject(3, &long);
        assert!(subject.starts_with("bugkill: attempt #3 — "));
        assert!(subject.ends_with('…'));
        assert_eq!(
            subject
                .strip_prefix("bugkill: attempt #3 — ")
                .unwrap()
                .chars()
                .count(),
            51
        );
    }
}
