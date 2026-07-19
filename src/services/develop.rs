//! Pure, synchronous Develop logic: the plan data model, the plan-contract
//! parser, the `PLAN.md` renderer + resume parser, and the section-progress
//! helpers. No I/O lives here — `DashboardService` owns every git/AI call and
//! `App` owns the async orchestration, so everything in this module is
//! unit-testable with plain strings.
//!
//! Token-efficiency invariant: the AI never reads or writes `PLAN.md`. The
//! plan AI emits compact delimited blocks (parsed here), the harness holds
//! the model in memory and re-renders the whole file from it after every
//! mutation, and the implement AI receives only the section(s) it must build
//! — progress tracking (checkboxes, the tracker table) is done in Rust.

/// The rendered plan file — harness-owned output at the worktree root.
pub const PLAN_FILE: &str = "PLAN.md";

/// Maximum number of sections kept after a contract parse; overflow is
/// dropped so a runaway plan cannot spawn an unbounded implement loop.
pub const MAX_PLAN_SECTIONS: usize = 16;

/// One implementation section of the plan. The `body` is the markdown block
/// rendered under the section header (Goal / Files / Acceptance criteria /
/// Edge cases), stored verbatim so render → parse round-trips exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSection {
    /// 1-based, in dependency order.
    pub number: usize,
    /// Single-line section name shown in headers and the tracker.
    pub name: String,
    /// Markdown body under the header (goal, files, criteria checkboxes…).
    pub body: String,
    /// Marked by the harness once the section's implement run finishes.
    pub done: bool,
}

/// The in-memory plan — the single source of truth `PLAN.md` is rendered
/// from. Recovered from disk on Resume via [`parse_plan_md`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopPlan {
    pub task_description: String,
    /// Fibonacci-style complexity estimate in points.
    pub complexity: u8,
    pub sections: Vec<PlanSection>,
}

impl DevelopPlan {
    /// Index of the first section not yet implemented.
    pub fn first_pending(&self) -> Option<usize> {
        self.sections.iter().position(|s| !s.done)
    }

    pub fn pending_count(&self) -> usize {
        self.sections.iter().filter(|s| !s.done).count()
    }

    /// Flip one section to done, checking off its `- [ ]` boxes so the
    /// rendered file reads as completed without any AI involvement.
    pub fn mark_done(&mut self, index: usize) {
        if let Some(section) = self.sections.get_mut(index) {
            section.done = true;
            section.body = section.body.replace("- [ ]", "- [x]");
        }
    }
}

// ── Plan-contract parser ────────────────────────────────────────────────

const TASK_OPEN: &str = "==== TASK ====";
const SECTION_OPEN: &str = "==== SECTION ====";
const BLOCK_CLOSE: &str = "==== END ====";

/// Parse the plan AI's transcript into a [`DevelopPlan`]. One `TASK` block
/// (description + complexity) must precede one `SECTION` block per section;
/// text outside blocks (opencode transcript noise) is ignored. Returns
/// `None` when the task block or any section field is missing or invalid —
/// one invalid block invalidates the whole parse (triggering the caller's
/// single corrective retry).
pub fn parse_plan_transcript(transcript: &str) -> Option<DevelopPlan> {
    let mut task: Option<(String, u8)> = None;
    let mut sections: Vec<PlanSection> = Vec::new();
    let mut block: Option<(bool, Vec<&str>)> = None; // (is_task, lines)
    for line in transcript.lines() {
        let trimmed = line.trim();
        match &mut block {
            None => {
                if trimmed == TASK_OPEN {
                    block = Some((true, Vec::new()));
                } else if trimmed == SECTION_OPEN {
                    block = Some((false, Vec::new()));
                }
            }
            Some((is_task, lines)) => {
                if trimmed == BLOCK_CLOSE {
                    if *is_task {
                        task = Some(parse_task_block(lines)?);
                    } else {
                        let mut section = parse_section_block(lines)?;
                        section.number = sections.len() + 1;
                        sections.push(section);
                    }
                    block = None;
                } else {
                    lines.push(line);
                }
            }
        }
    }
    // An unterminated block, a missing task block, or zero sections is a
    // parse failure, not an empty success.
    if block.is_some() || sections.is_empty() {
        return None;
    }
    let (task_description, complexity) = task?;
    sections.truncate(MAX_PLAN_SECTIONS);
    for (idx, section) in sections.iter_mut().enumerate() {
        section.number = idx + 1;
    }
    Some(DevelopPlan {
        task_description,
        complexity,
        sections,
    })
}

/// Collect `KEY:`-prefixed multiline fields from a block's lines. Lines
/// before the first key are tolerated as noise.
fn parse_fields<const N: usize>(lines: &[&str], keys: [&str; N]) -> [Option<String>; N] {
    let mut fields: [Option<String>; N] = std::array::from_fn(|_| None);
    let mut current: Option<usize> = None;
    for line in lines {
        if let Some(idx) = keys.iter().position(|key| line.starts_with(key)) {
            fields[idx] = Some(line[keys[idx].len()..].trim_start().to_string());
            current = Some(idx);
        } else if let Some(idx) = current {
            let value = fields[idx].as_mut().expect("current field is set");
            value.push('\n');
            value.push_str(line);
        }
    }
    fields
}

fn parse_task_block(lines: &[&str]) -> Option<(String, u8)> {
    let [description, complexity] = parse_fields(lines, ["DESCRIPTION:", "COMPLEXITY:"]);
    let complexity: u8 = complexity?.trim().parse().ok()?;
    if !(1..=99).contains(&complexity) {
        return None;
    }
    Some((description?.trim().to_string(), complexity))
}

fn parse_section_block(lines: &[&str]) -> Option<PlanSection> {
    let [name, goal, files, criteria, edge_cases] = parse_fields(
        lines,
        ["NAME:", "GOAL:", "FILES:", "CRITERIA:", "EDGE_CASES:"],
    );
    let name = name?.lines().next().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return None;
    }
    let goal = goal?.trim().to_string();
    let criteria = criteria?;
    let mut body = format!("**Goal**: {goal}\n");
    if let Some(files) = files.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        body.push_str(&format!("**Files**: {files}\n"));
    }
    body.push_str("**Acceptance criteria**:\n");
    let criteria_boxes = checkbox_lines(&criteria);
    if criteria_boxes.is_empty() {
        return None;
    }
    body.push_str(&criteria_boxes);
    if let Some(edge_cases) = edge_cases
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        let boxes = checkbox_lines(edge_cases);
        if !boxes.is_empty() {
            body.push_str("**Edge cases**:\n");
            body.push_str(&boxes);
        }
    }
    Some(PlanSection {
        number: 0, // assigned by the caller
        name,
        body: body.trim_end().to_string(),
        done: false,
    })
}

/// Turn a field's `- item` lines into `- [ ] item` checkbox lines (one per
/// non-empty line; a missing `- ` prefix is added).
fn checkbox_lines(field: &str) -> String {
    let mut out = String::new();
    for line in field.lines() {
        let item = line.trim().trim_start_matches("- ").trim();
        if item.is_empty() {
            continue;
        }
        out.push_str(&format!("- [ ] {item}\n"));
    }
    out
}

// ── PLAN.md renderer + resume parser ────────────────────────────────────

const TASK_HEADING: &str = "## Task Description";
const SECTIONS_HEADING: &str = "## Implementation Sections";
const TRACKER_HEADING: &str = "## Progress Tracker";
const DONE_SUFFIX: &str = " ✅";

/// Render the whole `PLAN.md` from the in-memory model. The harness rewrites
/// the file with this after **every** mutation — the file is output for the
/// human, never input for the AI.
pub fn render_plan_md(plan: &DevelopPlan) -> String {
    let mut out = format!(
        "# Development Plan\n\n{TASK_HEADING}\n\n{}\n\n**Complexity**: {} points\n\n---\n\n\
         {SECTIONS_HEADING}\n",
        plan.task_description.trim(),
        plan.complexity
    );
    for section in &plan.sections {
        let done = if section.done { DONE_SUFFIX } else { "" };
        out.push_str(&format!(
            "\n#### Section {} — {}{done}\n{}\n\n---\n",
            section.number, section.name, section.body
        ));
    }
    out.push_str(&format!(
        "\n{TRACKER_HEADING}\n\n| Section | Name | Status |\n|---------|------|--------|\n"
    ));
    for section in &plan.sections {
        let status = if section.done {
            "✅ Done"
        } else {
            "⬚ Pending"
        };
        out.push_str(&format!(
            "| {} | {} | {status} |\n",
            section.number, section.name
        ));
    }
    out
}

/// Resume parser. Accepts a file iff it contains the Task Description
/// heading with a `**Complexity**: N points` line, the Implementation
/// Sections heading with at least one `#### Section N — Name` header, and
/// the Progress Tracker heading. Section done-state comes from the header's
/// trailing ✅. Any violation → unparseable (`None`), which the preflight
/// turns into the Overwrite/Cancel prompt.
/// Round-trip property: `parse(render(plan)) == plan`.
pub fn parse_plan_md(content: &str) -> Option<DevelopPlan> {
    let lines: Vec<&str> = content.lines().collect();
    let task_idx = lines.iter().position(|l| l.trim() == TASK_HEADING)?;
    let sections_idx = lines.iter().position(|l| l.trim() == SECTIONS_HEADING)?;
    let tracker_idx = lines.iter().position(|l| l.trim() == TRACKER_HEADING)?;
    if !(task_idx < sections_idx && sections_idx < tracker_idx) {
        return None;
    }

    // Task description: everything between the heading and the complexity
    // line; the complexity line must sit before the sections heading.
    let complexity_idx = lines[task_idx..sections_idx]
        .iter()
        .position(|l| l.trim().starts_with("**Complexity**:"))?
        + task_idx;
    let task_description = lines[task_idx + 1..complexity_idx]
        .join("\n")
        .trim()
        .trim_end_matches("---")
        .trim()
        .to_string();
    let complexity: u8 = lines[complexity_idx]
        .trim()
        .strip_prefix("**Complexity**:")?
        .trim()
        .strip_suffix("points")?
        .trim()
        .parse()
        .ok()?;

    let mut sections: Vec<PlanSection> = Vec::new();
    let mut cursor = sections_idx + 1;
    while cursor < tracker_idx {
        let line = lines[cursor].trim();
        if let Some(rest) = line.strip_prefix("#### Section ") {
            let (number_part, name_part) = rest.split_once(" — ")?;
            let number: usize = number_part.trim().parse().ok()?;
            let (name, done) = match name_part.strip_suffix(DONE_SUFFIX) {
                Some(name) => (name, true),
                None => (name_part, false),
            };
            // Body: lines until the `---` separator (or the tracker heading).
            let mut end = cursor + 1;
            while end < tracker_idx && lines[end].trim() != "---" {
                end += 1;
            }
            let body = lines[cursor + 1..end].join("\n").trim().to_string();
            sections.push(PlanSection {
                number,
                name: name.trim().to_string(),
                body,
                done,
            });
            cursor = end + 1;
        } else {
            cursor += 1;
        }
    }
    if sections.is_empty() {
        return None;
    }
    Some(DevelopPlan {
        task_description,
        complexity,
        sections,
    })
}

// ── Prompt-side renderers ───────────────────────────────────────────────

/// Compact contract-format rendering of the current plan, fed back to the
/// plan AI on a revision (so it edits the existing plan instead of being
/// re-told everything through prose). Each section body is converted back
/// to the GOAL/FILES/CRITERIA/EDGE_CASES fields, so the payload satisfies
/// the exact contract the plan AI is asked to emit.
pub fn render_plan_contract(plan: &DevelopPlan) -> String {
    let mut out = format!(
        "{TASK_OPEN}\nDESCRIPTION: {}\nCOMPLEXITY: {}\n{BLOCK_CLOSE}\n",
        plan.task_description.trim(),
        plan.complexity
    );
    for section in &plan.sections {
        let (goal, files, criteria, edge_cases) = split_body(&section.body);
        out.push_str(&format!(
            "{SECTION_OPEN}\nNAME: {}\nGOAL: {goal}\n",
            section.name
        ));
        if let Some(files) = files {
            out.push_str(&format!("FILES: {files}\n"));
        }
        out.push_str(&format!("CRITERIA: {}\n", dashed_list(&criteria)));
        if !edge_cases.is_empty() {
            out.push_str(&format!("EDGE_CASES: {}\n", dashed_list(&edge_cases)));
        }
        out.push_str(BLOCK_CLOSE);
        out.push('\n');
    }
    out
}

/// Join list items as the contract's `- item` lines (the first item sits on
/// the field's own line).
fn dashed_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split a rendered section body back into its
/// (goal, files, criteria, edge cases) fields — the exact inverse of
/// [`parse_section_block`]'s body construction. Checkbox state (`[ ]`/`[x]`)
/// is dropped: progress lives in `done`, not in the revision payload.
fn split_body(body: &str) -> (String, Option<String>, Vec<String>, Vec<String>) {
    #[derive(PartialEq)]
    enum Part {
        Goal,
        Files,
        Criteria,
        EdgeCases,
    }
    let mut goal = String::new();
    let mut files: Option<String> = None;
    let mut criteria: Vec<String> = Vec::new();
    let mut edge_cases: Vec<String> = Vec::new();
    let mut part: Option<Part> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("**Goal**:") {
            goal = rest.trim().to_string();
            part = Some(Part::Goal);
        } else if let Some(rest) = line.strip_prefix("**Files**:") {
            files = Some(rest.trim().to_string());
            part = Some(Part::Files);
        } else if line.trim() == "**Acceptance criteria**:" {
            part = Some(Part::Criteria);
        } else if line.trim() == "**Edge cases**:" {
            part = Some(Part::EdgeCases);
        } else {
            match part {
                Some(Part::Goal) => {
                    goal.push('\n');
                    goal.push_str(line);
                }
                Some(Part::Files) => {
                    if let Some(files) = files.as_mut() {
                        files.push('\n');
                        files.push_str(line);
                    }
                }
                Some(Part::Criteria) => criteria.push(strip_checkbox(line)),
                Some(Part::EdgeCases) => edge_cases.push(strip_checkbox(line)),
                None => {}
            }
        }
    }
    (goal, files, criteria, edge_cases)
}

fn strip_checkbox(line: &str) -> String {
    line.trim()
        .trim_start_matches("- [ ]")
        .trim_start_matches("- [x]")
        .trim()
        .to_string()
}

/// The section block(s) embedded in one implement prompt: only the sections
/// the run must build, nothing else (token-efficiency invariant).
pub fn render_sections_for_prompt(sections: &[&PlanSection]) -> String {
    let mut out = String::new();
    for section in sections {
        out.push_str(&format!(
            "### Section {} — {}\n{}\n\n",
            section.number, section.name, section.body
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(number: usize, name: &str, done: bool) -> PlanSection {
        PlanSection {
            number,
            name: name.to_string(),
            body: format!(
                "**Goal**: goal for {name}\n**Files**: src/{name}.rs\n\
                 **Acceptance criteria**:\n- [ ] criterion a\n- [ ] criterion b\n\
                 **Edge cases**:\n- [ ] empty input"
            ),
            done,
        }
    }

    fn plan() -> DevelopPlan {
        DevelopPlan {
            task_description: "Add CSV export.\nWith a --csv flag.".to_string(),
            complexity: 5,
            sections: vec![
                section(1, "Data model", true),
                section(2, "Exporter", false),
                section(3, "CLI flag", false),
            ],
        }
    }

    // ── contract parser ─────────────────────────────────────────────────

    #[test]
    fn parses_task_and_sections_with_noise() {
        let transcript = "\
opencode banner noise
==== TASK ====
DESCRIPTION: Add CSV export
 across two lines
COMPLEXITY: 5
==== END ====
chatter
==== SECTION ====
NAME: Data model
GOAL: Introduce the record type
FILES: src/model.rs
CRITERIA: - record struct exists
- serializes to csv row
EDGE_CASES: - empty record list
==== END ====
==== SECTION ====
NAME: CLI flag
GOAL: Wire --csv
FILES: src/cli.rs
CRITERIA: flag parses
EDGE_CASES:
==== END ====
trailing noise";
        let plan = parse_plan_transcript(transcript).expect("parses");
        assert_eq!(plan.task_description, "Add CSV export\n across two lines");
        assert_eq!(plan.complexity, 5);
        assert_eq!(plan.sections.len(), 2);
        assert_eq!(plan.sections[0].number, 1);
        assert_eq!(plan.sections[0].name, "Data model");
        assert!(plan.sections[0]
            .body
            .contains("**Goal**: Introduce the record type"));
        assert!(plan.sections[0].body.contains("**Files**: src/model.rs"));
        assert!(plan.sections[0].body.contains("- [ ] record struct exists"));
        assert!(plan.sections[0]
            .body
            .contains("- [ ] serializes to csv row"));
        assert!(plan.sections[0].body.contains("**Edge cases**:"));
        assert!(plan.sections[0].body.contains("- [ ] empty record list"));
        // Bare criteria lines gain the checkbox prefix; empty edge cases are
        // omitted entirely.
        assert!(plan.sections[1].body.contains("- [ ] flag parses"));
        assert!(!plan.sections[1].body.contains("**Edge cases**"));
        assert_eq!(plan.sections[1].number, 2);
        assert!(plan.first_pending() == Some(0));
    }

    #[test]
    fn missing_task_block_or_sections_fails() {
        let no_task = "\
==== SECTION ====
NAME: a
GOAL: g
CRITERIA: - c
==== END ====";
        assert_eq!(parse_plan_transcript(no_task), None);
        let no_sections = "\
==== TASK ====
DESCRIPTION: d
COMPLEXITY: 3
==== END ====";
        assert_eq!(parse_plan_transcript(no_sections), None);
        assert_eq!(parse_plan_transcript(""), None);
    }

    #[test]
    fn invalid_complexity_name_or_criteria_fails() {
        let template = "\
==== TASK ====
DESCRIPTION: d
COMPLEXITY: 5
==== END ====
==== SECTION ====
NAME: a
GOAL: g
CRITERIA: - c
==== END ====";
        assert!(parse_plan_transcript(template).is_some());
        let bad_complexity = template.replace("COMPLEXITY: 5", "COMPLEXITY: huge");
        assert_eq!(parse_plan_transcript(&bad_complexity), None);
        let zero_complexity = template.replace("COMPLEXITY: 5", "COMPLEXITY: 0");
        assert_eq!(parse_plan_transcript(&zero_complexity), None);
        let empty_name = template.replace("NAME: a", "NAME:");
        assert_eq!(parse_plan_transcript(&empty_name), None);
        let empty_criteria = template.replace("CRITERIA: - c", "CRITERIA:");
        assert_eq!(parse_plan_transcript(&empty_criteria), None);
    }

    #[test]
    fn unterminated_block_fails() {
        let transcript = "\
==== TASK ====
DESCRIPTION: d
COMPLEXITY: 3
==== END ====
==== SECTION ====
NAME: a
GOAL: g
CRITERIA: - c";
        assert_eq!(parse_plan_transcript(transcript), None);
    }

    #[test]
    fn overflow_sections_are_truncated_and_renumbered() {
        let mut transcript =
            String::from("==== TASK ====\nDESCRIPTION: d\nCOMPLEXITY: 8\n==== END ====\n");
        for i in 0..20 {
            transcript.push_str(&format!(
                "==== SECTION ====\nNAME: s{i}\nGOAL: g\nCRITERIA: - c\n==== END ====\n"
            ));
        }
        let plan = parse_plan_transcript(&transcript).expect("parses");
        assert_eq!(plan.sections.len(), MAX_PLAN_SECTIONS);
        assert_eq!(plan.sections.last().unwrap().number, MAX_PLAN_SECTIONS);
    }

    // ── render / parse round-trip ───────────────────────────────────────

    #[test]
    fn render_then_parse_round_trips() {
        let plan = plan();
        let rendered = render_plan_md(&plan);
        let parsed = parse_plan_md(&rendered).expect("round-trip parses");
        assert_eq!(parsed, plan);
    }

    #[test]
    fn render_marks_done_sections_in_header_and_tracker() {
        let rendered = render_plan_md(&plan());
        assert!(
            rendered.contains("#### Section 1 — Data model ✅"),
            "{rendered}"
        );
        assert!(
            rendered.contains("#### Section 2 — Exporter\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("| 1 | Data model | ✅ Done |"),
            "{rendered}"
        );
        assert!(
            rendered.contains("| 2 | Exporter | ⬚ Pending |"),
            "{rendered}"
        );
    }

    #[test]
    fn mark_done_checks_the_boxes_and_advances_first_pending() {
        let mut plan = plan();
        assert_eq!(plan.first_pending(), Some(1));
        assert_eq!(plan.pending_count(), 2);
        plan.mark_done(1);
        assert!(plan.sections[1].done);
        assert!(plan.sections[1].body.contains("- [x] criterion a"));
        assert!(!plan.sections[1].body.contains("- [ ]"));
        assert_eq!(plan.first_pending(), Some(2));
        plan.mark_done(2);
        assert_eq!(plan.first_pending(), None);
        assert_eq!(plan.pending_count(), 0);
    }

    #[test]
    fn parser_rejects_foreign_documents() {
        assert_eq!(parse_plan_md("# Some other doc"), None);
        assert_eq!(
            parse_plan_md("## Task Description\n\nx\n\n**Complexity**: 3 points"),
            None
        );
        // Headings in the wrong order.
        let scrambled = "\
## Progress Tracker
## Task Description
x
**Complexity**: 3 points
## Implementation Sections
#### Section 1 — a
body";
        assert_eq!(parse_plan_md(scrambled), None);
    }

    #[test]
    fn parser_reads_done_state_from_the_header_suffix() {
        let mut plan = plan();
        plan.mark_done(1);
        plan.mark_done(2);
        let parsed = parse_plan_md(&render_plan_md(&plan)).expect("parses");
        assert_eq!(
            parsed.sections.iter().map(|s| s.done).collect::<Vec<_>>(),
            vec![true, true, true]
        );
    }

    // ── prompt-side renderers ───────────────────────────────────────────

    #[test]
    fn plan_contract_round_trips_through_the_transcript_parser() {
        // The compact revision payload must itself satisfy the contract the
        // plan AI is asked to emit, so a revision run starts from parity.
        let plan = plan();
        let contract = render_plan_contract(&plan);
        let reparsed = parse_plan_transcript(&contract).expect("contract parses");
        assert_eq!(reparsed.task_description, plan.task_description);
        assert_eq!(reparsed.complexity, plan.complexity);
        assert_eq!(reparsed.sections.len(), plan.sections.len());
        assert_eq!(reparsed.sections[0].name, "Data model");
    }

    #[test]
    fn sections_for_prompt_contain_only_the_given_sections() {
        let plan = plan();
        let one = render_sections_for_prompt(&[&plan.sections[1]]);
        assert!(one.contains("### Section 2 — Exporter"), "{one}");
        assert!(!one.contains("Data model"), "{one}");
        assert!(!one.contains("CLI flag"), "{one}");
        let all: Vec<&PlanSection> = plan.sections.iter().collect();
        let block = render_sections_for_prompt(&all);
        assert!(block.contains("### Section 1"));
        assert!(block.contains("### Section 3"));
    }
}
